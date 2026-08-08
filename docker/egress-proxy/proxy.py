#!/usr/bin/env python3
import ipaddress
import os
import select
import signal
import socket
import socketserver
import sys
import threading
import time
import urllib.parse


HEADER_LIMIT = 65_536
IO_TIMEOUT_SECONDS = 15
TUNNEL_IDLE_TIMEOUT_SECONDS = 300
CANARY_HOST = "harness-egress-canary.invalid"
RFC2544_SYNTHETIC_DNS = ipaddress.ip_network("198.18.0.0/15")
STATUS_REASONS = {
    400: "Bad Request",
    403: "Forbidden",
    431: "Request Header Fields Too Large",
    502: "Bad Gateway",
}


class ProxyRefusal(Exception):
    def __init__(self, status, reason):
        super().__init__(reason)
        self.status = status
        self.reason = reason


def normalize_dns_name(value):
    candidate = value.strip().rstrip(".").lower()
    if not candidate or any(character in candidate for character in "/:@*?#"):
        raise ValueError(f"invalid egress allowlist hostname: {value!r}")
    try:
        candidate = candidate.encode("idna").decode("ascii")
    except UnicodeError as error:
        raise ValueError(f"invalid egress allowlist hostname: {value!r}") from error
    labels = candidate.split(".")
    if (
        len(candidate) > 253
        or len(labels) < 2
        or any(not label or len(label) > 63 for label in labels)
        or any(label.startswith("-") or label.endswith("-") for label in labels)
        or any(not all(character.isalnum() or character == "-" for character in label) for label in labels)
    ):
        raise ValueError(f"invalid egress allowlist hostname: {value!r}")
    try:
        literal_ip = ipaddress.ip_address(candidate)
    except ValueError:
        literal_ip = None
    if literal_ip is not None:
        raise ValueError("IP literals are not valid egress allowlist hostnames")
    if candidate == "localhost" or candidate.endswith(".localhost"):
        raise ValueError("localhost is not a valid egress allowlist hostname")
    return candidate


def parse_allowlist(raw_value):
    values = raw_value.split(",")
    if not values or any(not value.strip() for value in values):
        raise ValueError("HARNESS_EGRESS_ALLOWLIST must contain exact DNS hostnames")
    return frozenset(normalize_dns_name(value) for value in values)


def require_allowed_host(host, allowlist):
    try:
        normalized = normalize_dns_name(host)
    except ValueError as error:
        raise ProxyRefusal(403, "target hostname is invalid") from error
    if normalized not in allowlist:
        raise ProxyRefusal(403, "target hostname is not allowlisted")
    return normalized


def resolve_public_endpoints(host, port, allow_rfc2544_dns=False):
    try:
        answers = socket.getaddrinfo(host, port, type=socket.SOCK_STREAM)
    except socket.gaierror as error:
        raise ProxyRefusal(502, "allowlisted target did not resolve") from error
    endpoints = []
    seen = set()
    for _family, _kind, _protocol, _canonical, address in answers:
        ip = ipaddress.ip_address(address[0])
        if not ip.is_global and not (allow_rfc2544_dns and ip in RFC2544_SYNTHETIC_DNS):
            raise ProxyRefusal(403, "allowlisted target resolved to a non-global address")
        key = (str(ip), address[1])
        if key not in seen:
            seen.add(key)
            endpoints.append(address)
    if not endpoints:
        raise ProxyRefusal(502, "allowlisted target had no usable address")
    return endpoints


def connect_upstream(host, port, allow_rfc2544_dns=False):
    errors = []
    for endpoint in resolve_public_endpoints(host, port, allow_rfc2544_dns):
        try:
            return socket.create_connection(endpoint, timeout=IO_TIMEOUT_SECONDS)
        except OSError as error:
            errors.append(error)
    raise ProxyRefusal(502, "allowlisted target was unreachable") from errors[-1]


def parse_authority(authority, default_port):
    try:
        parsed = urllib.parse.urlsplit(f"//{authority}")
        port = parsed.port or default_port
    except ValueError as error:
        raise ProxyRefusal(400, "invalid proxy target authority") from error
    if parsed.username or parsed.password or not parsed.hostname or not 1 <= port <= 65_535:
        raise ProxyRefusal(400, "invalid proxy target authority")
    return parsed.hostname, port


def read_request_headers(connection):
    payload = bytearray()
    while b"\r\n\r\n" not in payload:
        chunk = connection.recv(8192)
        if not chunk:
            raise ProxyRefusal(400, "incomplete proxy request headers")
        payload.extend(chunk)
        if len(payload) > HEADER_LIMIT:
            raise ProxyRefusal(431, "proxy request headers are too large")
    marker = payload.index(b"\r\n\r\n") + 4
    return bytes(payload[:marker]), bytes(payload[marker:])


def parse_request_line(header_block):
    try:
        first_line = header_block.split(b"\r\n", 1)[0].decode("ascii")
        method, target, version = first_line.split(" ")
    except (UnicodeDecodeError, ValueError) as error:
        raise ProxyRefusal(400, "invalid proxy request line") from error
    if version not in {"HTTP/1.0", "HTTP/1.1"} or not method.isalpha():
        raise ProxyRefusal(400, "invalid proxy request line")
    return method.upper(), target, version


def rewrite_http_request(header_block, method, target, version, allowlist):
    try:
        parsed = urllib.parse.urlsplit(target)
        port = parsed.port or 80
    except ValueError as error:
        raise ProxyRefusal(400, "invalid absolute proxy URI") from error
    if parsed.scheme.lower() != "http" or not parsed.hostname or parsed.username or parsed.password:
        raise ProxyRefusal(400, "HTTP proxy requests require an absolute http URI")
    host = require_allowed_host(parsed.hostname, allowlist)
    path = urllib.parse.urlunsplit(("", "", parsed.path or "/", parsed.query, ""))
    headers = []
    for raw_line in header_block.split(b"\r\n")[1:]:
        if not raw_line:
            continue
        if b":" not in raw_line:
            raise ProxyRefusal(400, "invalid proxy header")
        name, value = raw_line.split(b":", 1)
        normalized_name = name.strip().lower()
        if normalized_name in {b"host", b"proxy-authorization", b"proxy-connection", b"connection"}:
            continue
        headers.append(name.strip() + b":" + value)
    authority = host if port == 80 else f"{host}:{port}"
    request = [f"{method} {path} {version}".encode("ascii")]
    request.append(f"Host: {authority}".encode("ascii"))
    request.extend(headers)
    request.extend([b"Connection: close", b"", b""])
    return host, port, b"\r\n".join(request)


def relay_bidirectionally(client, upstream):
    sockets = {client, upstream}
    last_activity = time.monotonic()
    while sockets:
        readable, _, _ = select.select(list(sockets), [], [], 1)
        if not readable:
            if time.monotonic() - last_activity >= TUNNEL_IDLE_TIMEOUT_SECONDS:
                return
            continue
        for source in readable:
            destination = upstream if source is client else client
            data = source.recv(65_536)
            if not data:
                sockets.remove(source)
                try:
                    destination.shutdown(socket.SHUT_WR)
                except OSError:
                    sockets.discard(destination)
                continue
            destination.sendall(data)
            last_activity = time.monotonic()


class ThreadingProxyServer(socketserver.ThreadingMixIn, socketserver.TCPServer):
    allow_reuse_address = True
    daemon_threads = True

    def __init__(self, server_address, allowlist, allow_rfc2544_dns=False):
        self.allowlist = allowlist
        self.allow_rfc2544_dns = allow_rfc2544_dns
        super().__init__(server_address, ProxyHandler)


class ProxyHandler(socketserver.BaseRequestHandler):
    def handle(self):
        self.request.settimeout(IO_TIMEOUT_SECONDS)
        try:
            header_block, buffered_body = read_request_headers(self.request)
            method, target, version = parse_request_line(header_block)
            if method == "CONNECT":
                self.handle_connect(target, buffered_body)
            else:
                self.handle_http(header_block, buffered_body, method, target, version)
        except ProxyRefusal as error:
            self.send_error(error.status, error.reason)
        except (OSError, TimeoutError) as error:
            print(f"egress proxy transport failure: {error}", file=sys.stderr, flush=True)
            self.send_error(502, "egress proxy transport failure")

    def handle_connect(self, target, buffered_body):
        host, port = parse_authority(target, 443)
        host = require_allowed_host(host, self.server.allowlist)
        with connect_upstream(host, port, self.server.allow_rfc2544_dns) as upstream:
            self.request.sendall(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            if buffered_body:
                upstream.sendall(buffered_body)
            relay_bidirectionally(self.request, upstream)

    def handle_http(self, header_block, buffered_body, method, target, version):
        host, port, rewritten = rewrite_http_request(
            header_block, method, target, version, self.server.allowlist
        )
        with connect_upstream(host, port, self.server.allow_rfc2544_dns) as upstream:
            upstream.sendall(rewritten)
            if buffered_body:
                upstream.sendall(buffered_body)
            relay_bidirectionally(self.request, upstream)

    def send_error(self, status, reason):
        body = f"{status} {reason}\n".encode("utf-8")
        status_reason = STATUS_REASONS.get(status, "Proxy Error")
        response = (
            f"HTTP/1.1 {status} {status_reason}\r\n"
            f"Content-Type: text/plain; charset=utf-8\r\n"
            f"Content-Length: {len(body)}\r\n"
            "Connection: close\r\n\r\n"
        ).encode("ascii", "replace") + body
        try:
            self.request.sendall(response)
        except OSError as error:
            print(f"failed to send proxy refusal: {error}", file=sys.stderr, flush=True)


def run_canary(host, port):
    request = (
        f"GET http://{CANARY_HOST}/ HTTP/1.1\r\n"
        f"Host: {CANARY_HOST}\r\nConnection: close\r\n\r\n"
    ).encode("ascii")
    with socket.create_connection((host, port), timeout=IO_TIMEOUT_SECONDS) as connection:
        connection.sendall(request)
        response = connection.recv(4096)
    return response.startswith(b"HTTP/1.1 403 Forbidden\r\n")


def main():
    if len(sys.argv) == 3 and sys.argv[1] == "--canary":
        host, port = parse_authority(sys.argv[2], 8080)
        return 0 if run_canary(host, port) else 1
    if len(sys.argv) != 1:
        print("usage: proxy.py [--canary HOST:PORT]", file=sys.stderr)
        return 2
    try:
        allowlist = parse_allowlist(os.environ.get("HARNESS_EGRESS_ALLOWLIST", ""))
    except ValueError as error:
        print(f"egress proxy configuration error: {error}", file=sys.stderr)
        return 2
    rfc2544_setting = os.environ.get("HARNESS_EGRESS_ALLOW_RFC2544_DNS", "")
    if rfc2544_setting not in {"", "0", "1"}:
        print(
            "egress proxy configuration error: "
            "HARNESS_EGRESS_ALLOW_RFC2544_DNS must be 0 or 1",
            file=sys.stderr,
        )
        return 2
    server = ThreadingProxyServer(
        ("0.0.0.0", 8080),
        allowlist,
        allow_rfc2544_dns=rfc2544_setting == "1",
    )

    def stop_server(_signum, _frame):
        threading.Thread(target=server.shutdown, daemon=True).start()

    signal.signal(signal.SIGTERM, stop_server)
    signal.signal(signal.SIGINT, stop_server)
    print(f"egress proxy ready with {len(allowlist)} exact hosts", flush=True)
    try:
        server.serve_forever(poll_interval=0.2)
    finally:
        server.server_close()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
