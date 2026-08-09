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
        self.server.received = payload
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


def tls_record(payload):
    return b"\x16\x03\x01" + len(payload).to_bytes(2, "big") + payload


def tls_client_hello(server_name, record_split=None):
    extensions = b""
    if server_name is not None:
        encoded_name = server_name.encode("ascii")
        name = b"\x00" + len(encoded_name).to_bytes(2, "big") + encoded_name
        name_list = len(name).to_bytes(2, "big") + name
        extensions = b"\x00\x00" + len(name_list).to_bytes(2, "big") + name_list
    body = (
        b"\x03\x03"
        + bytes(32)
        + b"\x00"
        + b"\x00\x02\x13\x01"
        + b"\x01\x00"
        + len(extensions).to_bytes(2, "big")
        + extensions
    )
    handshake = b"\x01" + len(body).to_bytes(3, "big") + body
    if record_split is None:
        return tls_record(handshake)
    return tls_record(handshake[:record_split]) + tls_record(handshake[record_split:])


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
        self.assertEqual(
            endpoints,
            [(socket.AF_INET, socket.SOCK_STREAM, 6, ("198.18.0.11", 443))],
        )

    def test_connect_uses_the_resolved_ipv6_socket_family_and_address(self):
        endpoint = ("2001:4860:4860::8888", 443, 0, 0)
        fake_socket = mock.Mock()
        with mock.patch.object(
            proxy,
            "resolve_public_endpoints",
            return_value=[(socket.AF_INET6, socket.SOCK_STREAM, 6, endpoint)],
        ), mock.patch("socket.socket", return_value=fake_socket) as socket_factory:
            connected = proxy.connect_upstream("example.com", 443)

        self.assertIs(connected, fake_socket)
        socket_factory.assert_called_once_with(socket.AF_INET6, socket.SOCK_STREAM, 6)
        fake_socket.connect.assert_called_once_with(endpoint)

    def test_canary_accepts_a_fragmented_status_line(self):
        connection = mock.MagicMock()
        connection.__enter__.return_value = connection
        connection.recv.side_effect = [b"HTTP/1.1 403", b" Forbidden\r\n"]
        with mock.patch("socket.create_connection", return_value=connection):
            self.assertTrue(proxy.run_canary("127.0.0.1", 8080))

    def test_tls_client_hello_parser_accepts_fragmented_records_and_reads(self):
        hello = tls_client_hello("Example.COM", record_split=17)
        connection = mock.Mock()
        connection.recv.side_effect = [hello[2:9], hello[9:31], hello[31:]]

        wire_data, handshake = proxy.receive_tls_client_hello(connection, hello[:2])

        self.assertEqual(wire_data, hello)
        self.assertEqual(proxy.tls_client_hello_server_name(handshake), "example.com")

    def test_connect_tls_identity_rejects_mismatched_sni(self):
        with self.assertRaisesRegex(proxy.ProxyRefusal, "SNI does not match"):
            proxy.validate_connect_tls_identity(
                mock.Mock(), tls_client_hello("denied.invalid"), "example.com"
            )

    def test_connect_tls_identity_rejects_client_hello_without_sni(self):
        with self.assertRaisesRegex(proxy.ProxyRefusal, "missing SNI"):
            proxy.validate_connect_tls_identity(
                mock.Mock(), tls_client_hello(None), "example.com"
            )

    def test_http_body_reader_collects_only_the_declared_body(self):
        connection = mock.Mock()
        connection.recv.side_effect = [b"st"]

        body = proxy.read_http_request_body(connection, b"te", 4)

        self.assertEqual(body, b"test")
        connection.recv.assert_called_once_with(2)

    def test_http_rewrite_rejects_oversized_declared_body(self):
        header_block = (
            b"POST http://example.com/ HTTP/1.1\r\n"
            b"Host: example.com\r\n"
            + f"Content-Length: {proxy.HTTP_BODY_LIMIT + 1}\r\n\r\n".encode()
        )

        with self.assertRaisesRegex(proxy.ProxyRefusal, "body is too large") as refusal:
            proxy.rewrite_http_request(
                header_block,
                "POST",
                "http://example.com/",
                "HTTP/1.1",
                frozenset({"example.com"}),
            )

        self.assertEqual(refusal.exception.status, 413)


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

    def test_connect_tunnels_matching_tls_sni_to_the_resolved_allowed_endpoint(self):
        upstream = OneShotTcpServer(("127.0.0.1", 0), EchoHandler)
        endpoint = upstream.server_address
        with running_server(upstream), running_server(self.proxy_server()) as server:
            with mock.patch.object(
                proxy,
                "resolve_public_endpoints",
                return_value=[(socket.AF_INET, socket.SOCK_STREAM, 6, endpoint)],
            ):
                with socket.create_connection(server.server_address, timeout=2) as client:
                    client.sendall(
                        b"CONNECT example.com:443 HTTP/1.1\r\n"
                        b"Host: example.com:443\r\n\r\n"
                    )
                    self.assertTrue(client.recv(4096).startswith(b"HTTP/1.1 200"))
                    client_hello = tls_client_hello("example.com")
                    client.sendall(client_hello)
                    self.assertEqual(client.recv(4096), client_hello)
        self.assertEqual(upstream.received, client_hello)

    def test_connect_rejects_denied_sni_before_forwarding_tunnel_bytes(self):
        upstream = OneShotTcpServer(("127.0.0.1", 0), EchoHandler)
        endpoint = upstream.server_address
        with running_server(upstream), running_server(self.proxy_server()) as server:
            with mock.patch.object(
                proxy,
                "resolve_public_endpoints",
                return_value=[(socket.AF_INET, socket.SOCK_STREAM, 6, endpoint)],
            ):
                with socket.create_connection(server.server_address, timeout=2) as client:
                    client.sendall(
                        b"CONNECT example.com:443 HTTP/1.1\r\n"
                        b"Host: example.com:443\r\n\r\n"
                    )
                    self.assertTrue(client.recv(4096).startswith(b"HTTP/1.1 200"))
                    client.sendall(tls_client_hello("denied.invalid"))
                    self.assertTrue(client.recv(4096).startswith(b"HTTP/1.1 403"))
        self.assertEqual(upstream.received, b"")

    def test_http_proxy_rewrites_absolute_uri_and_strips_proxy_headers(self):
        upstream = OneShotTcpServer(("127.0.0.1", 0), RecordingHandler)
        endpoint = upstream.server_address
        with running_server(upstream), running_server(self.proxy_server()) as server:
            with mock.patch.object(
                proxy,
                "resolve_public_endpoints",
                return_value=[(socket.AF_INET, socket.SOCK_STREAM, 6, endpoint)],
            ):
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

    def test_http_proxy_rejects_pipelined_request_before_connecting(self):
        with running_server(self.proxy_server()) as server:
            with mock.patch.object(proxy, "resolve_public_endpoints") as resolver:
                response = request_proxy(
                    server,
                    b"GET http://example.com/allowed HTTP/1.1\r\n"
                    b"Host: example.com\r\n\r\n"
                    b"GET http://denied.invalid/ HTTP/1.1\r\n"
                    b"Host: denied.invalid\r\n\r\n",
                )

        self.assertTrue(response.startswith(b"HTTP/1.1 400 Bad Request\r\n"))
        resolver.assert_not_called()

    def test_http_proxy_rejects_ambiguous_request_framing(self):
        requests = [
            (
                b"POST http://example.com/ HTTP/1.1\r\n"
                b"Host: example.com\r\n"
                b"Content-Length: 4\r\n"
                b"Content-Length: 4\r\n\r\ntest"
            ),
            (
                b"POST http://example.com/ HTTP/1.1\r\n"
                b"Host: example.com\r\n"
                b"Content-Length: 4\r\n"
                b"Transfer-Encoding: chunked\r\n\r\ntest"
            ),
            (
                b"POST http://example.com/ HTTP/1.1\r\n"
                b"Host: example.com\r\n"
                b"Transfer-Encoding: chunked\r\n\r\n4\r\ntest\r\n0\r\n\r\n"
            ),
        ]
        for request in requests:
            with self.subTest(request=request):
                with running_server(self.proxy_server()) as server:
                    with mock.patch.object(
                        proxy, "resolve_public_endpoints"
                    ) as resolver:
                        response = request_proxy(server, request)
                self.assertTrue(
                    response.startswith(b"HTTP/1.1 400 Bad Request\r\n")
                )
                resolver.assert_not_called()

    def test_http_proxy_forwards_exact_declared_body(self):
        upstream = OneShotTcpServer(("127.0.0.1", 0), RecordingHandler)
        endpoint = upstream.server_address
        with running_server(upstream), running_server(self.proxy_server()) as server:
            with mock.patch.object(
                proxy,
                "resolve_public_endpoints",
                return_value=[(socket.AF_INET, socket.SOCK_STREAM, 6, endpoint)],
            ):
                response = request_proxy(
                    server,
                    b"POST http://example.com/api HTTP/1.1\r\n"
                    b"Host: example.com\r\nContent-Length: 4\r\n\r\ntest",
                )

        self.assertTrue(response.startswith(b"HTTP/1.1 204 No Content\r\n"))
        self.assertTrue(upstream.received.startswith(b"POST /api HTTP/1.1\r\n"))
        self.assertTrue(upstream.received.endswith(b"\r\n\r\ntest"))


if __name__ == "__main__":
    unittest.main()
