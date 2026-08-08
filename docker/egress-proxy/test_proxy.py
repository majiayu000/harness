import contextlib
import socket
import socketserver
import threading
import unittest
from unittest import mock

import proxy


class OneShotTcpServer(socketserver.TCPServer):
    allow_reuse_address = True


class RecordingHandler(socketserver.BaseRequestHandler):
    response = b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n"

    def handle(self):
        self.server.received = self.request.recv(65_536)
        self.request.sendall(self.response)


class EchoHandler(socketserver.BaseRequestHandler):
    def handle(self):
        payload = self.request.recv(65_536)
        self.request.sendall(payload)


@contextlib.contextmanager
def running_server(server):
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield server
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=2)


def request_proxy(server, request):
    with socket.create_connection(server.server_address, timeout=2) as client:
        client.sendall(request)
        client.shutdown(socket.SHUT_WR)
        response = bytearray()
        while chunk := client.recv(65_536):
            response.extend(chunk)
        return bytes(response)


class AllowlistTests(unittest.TestCase):
    def test_allowlist_normalizes_exact_dns_names(self):
        self.assertEqual(
            proxy.parse_allowlist(" GitHub.COM.,api.github.com "),
            frozenset({"github.com", "api.github.com"}),
        )

    def test_allowlist_rejects_ambiguous_or_local_targets(self):
        invalid = [
            "https://github.com",
            "*.github.com",
            "127.0.0.1",
            "localhost",
            "github.com:443",
            "github.com/path",
            "user@github.com",
            "",
        ]
        for value in invalid:
            with self.subTest(value=value):
                with self.assertRaises(ValueError):
                    proxy.parse_allowlist(value)

    def test_resolution_rejects_non_global_addresses(self):
        answers = [
            (socket.AF_INET, socket.SOCK_STREAM, 6, "", ("127.0.0.1", 443)),
            (socket.AF_INET, socket.SOCK_STREAM, 6, "", ("10.0.0.8", 443)),
        ]
        with mock.patch("socket.getaddrinfo", return_value=answers):
            with self.assertRaises(proxy.ProxyRefusal):
                proxy.resolve_public_endpoints("example.com", 443)

    def test_resolution_accepts_rfc2544_synthetic_dns_only_when_explicit(self):
        answers = [
            (socket.AF_INET, socket.SOCK_STREAM, 6, "", ("198.18.0.11", 443)),
        ]
        with mock.patch("socket.getaddrinfo", return_value=answers):
            endpoints = proxy.resolve_public_endpoints(
                "example.com", 443, allow_rfc2544_dns=True
            )
        self.assertEqual(endpoints, [("198.18.0.11", 443)])


class ProxyIntegrationTests(unittest.TestCase):
    def proxy_server(self, allowlist=frozenset({"example.com"})):
        return proxy.ThreadingProxyServer(("127.0.0.1", 0), allowlist)

    def test_denied_target_returns_403_without_resolving_it(self):
        with running_server(self.proxy_server()) as server:
            with mock.patch.object(proxy, "resolve_public_endpoints") as resolver:
                response = request_proxy(
                    server,
                    b"GET http://denied.invalid/ HTTP/1.1\r\n"
                    b"Host: denied.invalid\r\nConnection: close\r\n\r\n",
                )
        self.assertTrue(response.startswith(b"HTTP/1.1 403 Forbidden\r\n"))
        resolver.assert_not_called()

    def test_connect_tunnels_only_to_the_resolved_allowed_endpoint(self):
        upstream = OneShotTcpServer(("127.0.0.1", 0), EchoHandler)
        endpoint = upstream.server_address
        with running_server(upstream), running_server(self.proxy_server()) as server:
            with mock.patch.object(proxy, "resolve_public_endpoints", return_value=[endpoint]):
                with socket.create_connection(server.server_address, timeout=2) as client:
                    client.sendall(
                        b"CONNECT example.com:443 HTTP/1.1\r\n"
                        b"Host: example.com:443\r\n\r\n"
                    )
                    self.assertTrue(client.recv(4096).startswith(b"HTTP/1.1 200"))
                    client.sendall(b"tunnel-payload")
                    self.assertEqual(client.recv(4096), b"tunnel-payload")

    def test_http_proxy_rewrites_absolute_uri_and_strips_proxy_headers(self):
        upstream = OneShotTcpServer(("127.0.0.1", 0), RecordingHandler)
        endpoint = upstream.server_address
        with running_server(upstream), running_server(self.proxy_server()) as server:
            with mock.patch.object(proxy, "resolve_public_endpoints", return_value=[endpoint]):
                response = request_proxy(
                    server,
                    b"GET http://example.com/api?q=1 HTTP/1.1\r\n"
                    b"Host: example.com\r\n"
                    b"Proxy-Authorization: Basic secret\r\n"
                    b"Proxy-Connection: keep-alive\r\nConnection: close\r\n\r\n",
                )
        self.assertTrue(response.startswith(b"HTTP/1.1 204 No Content\r\n"))
        self.assertTrue(upstream.received.startswith(b"GET /api?q=1 HTTP/1.1\r\n"))
        self.assertNotIn(b"Proxy-Authorization", upstream.received)
        self.assertNotIn(b"Proxy-Connection", upstream.received)


if __name__ == "__main__":
    unittest.main()
