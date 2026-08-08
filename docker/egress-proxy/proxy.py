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
TLS_CLIENT_HELLO_LIMIT = 65_536
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
    for family, kind, protocol, _canonical, address in answers:
        ip = ipaddress.ip_address(address[0])
        if not ip.is_global and not (allow_rfc2544_dns and ip in RFC2544_SYNTHETIC_DNS):
            raise ProxyRefusal(403, "allowlisted target resolved to a non-global address")
        key = (str(ip), address[1])
        if key not in seen:
            seen.add(key)
            endpoints.append((family, kind, protocol, address))
    if not endpoints:
        raise ProxyRefusal(502, "allowlisted target had no usable address")
    return endpoints


def connect_upstream(host, port, allow_rfc2544_dns=False):
    errors = []
    for family, kind, protocol, address in resolve_public_endpoints(
        host, port, allow_rfc2544_dns
    ):
        upstream = socket.socket(family, kind, protocol)
        upstream.settimeout(IO_TIMEOUT_SECONDS)
        try:
            upstream.connect(address)
            return upstream
        except OSError as error:
            upstream.close()
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


def receive_tls_client_hello(connection, buffered_data=b""):
    wire_data = bytearray(buffered_data)
    handshake_data = bytearray()
    offset = 0
    expected_handshake_size = None

    while expected_handshake_size is None or len(handshake_data) < expected_handshake_size:
        while len(wire_data) - offset < 5:
            chunk = connection.recv(8192)
            if not chunk:
                raise ProxyRefusal(403, "CONNECT tunnel did not provide a TLS ClientHello")
            wire_data.extend(chunk)
            if len(wire_data) > TLS_CLIENT_HELLO_LIMIT:
                raise ProxyRefusal(431, "TLS ClientHello is too large")

        content_type = wire_data[offset]
        legacy_major = wire_data[offset + 1]
        record_size = int.from_bytes(wire_data[offset + 3 : offset + 5], "big")
        record_end = offset + 5 + record_size
        if content_type != 22 or legacy_major != 3 or record_size == 0:
            raise ProxyRefusal(403, "CONNECT tunnel requires a TLS ClientHello")
        if record_end > TLS_CLIENT_HELLO_LIMIT:
            raise ProxyRefusal(431, "TLS ClientHello is too large")
        while len(wire_data) < record_end:
            chunk = connection.recv(min(8192, record_end - len(wire_data)))
            if not chunk:
                raise ProxyRefusal(400, "incomplete TLS ClientHello")
            wire_data.extend(chunk)
            if len(wire_data) > TLS_CLIENT_HELLO_LIMIT:
                raise ProxyRefusal(431, "TLS ClientHello is too large")

        handshake_data.extend(wire_data[offset + 5 : record_end])
        offset = record_end
        if len(handshake_data) >= 4 and expected_handshake_size is None:
            if handshake_data[0] != 1:
                raise ProxyRefusal(403, "CONNECT tunnel requires a TLS ClientHello")
            expected_handshake_size = 4 + int.from_bytes(handshake_data[1:4], "big")
            if expected_handshake_size > TLS_CLIENT_HELLO_LIMIT:
                raise ProxyRefusal(431, "TLS ClientHello is too large")

    return bytes(wire_data), bytes(handshake_data[:expected_handshake_size])


def tls_client_hello_server_name(handshake):
    if len(handshake) < 4 or handshake[0] != 1:
        raise ProxyRefusal(400, "invalid TLS ClientHello")
    declared_size = int.from_bytes(handshake[1:4], "big")
    if declared_size != len(handshake) - 4:
        raise ProxyRefusal(400, "invalid TLS ClientHello length")

    body = memoryview(handshake)[4:]
    offset = 34
    if len(body) < offset + 1:
        raise ProxyRefusal(400, "invalid TLS ClientHello")
    session_id_size = body[offset]
    offset += 1 + session_id_size
    if len(body) < offset + 2:
        raise ProxyRefusal(400, "invalid TLS ClientHello")
    cipher_suites_size = int.from_bytes(body[offset : offset + 2], "big")
    offset += 2 + cipher_suites_size
    if len(body) < offset + 1:
        raise ProxyRefusal(400, "invalid TLS ClientHello")
    compression_methods_size = body[offset]
    offset += 1 + compression_methods_size
    if len(body) < offset + 2:
        raise ProxyRefusal(403, "TLS ClientHello is missing SNI")
    extensions_size = int.from_bytes(body[offset : offset + 2], "big")
    offset += 2
    extensions_end = offset + extensions_size
    if extensions_end != len(body):
        raise ProxyRefusal(400, "invalid TLS ClientHello extensions")

    server_name = None
    while offset < extensions_end:
        if extensions_end - offset < 4:
            raise ProxyRefusal(400, "invalid TLS ClientHello extension")
        extension_type = int.from_bytes(body[offset : offset + 2], "big")
        extension_size = int.from_bytes(body[offset + 2 : offset + 4], "big")
        offset += 4
        extension_end = offset + extension_size
        if extension_end > extensions_end:
            raise ProxyRefusal(400, "invalid TLS ClientHello extension")
        if extension_type == 0:
            if server_name is not None:
                raise ProxyRefusal(400, "duplicate TLS SNI extension")
            extension = body[offset:extension_end]
            if len(extension) < 5:
                raise ProxyRefusal(400, "invalid TLS SNI extension")
            names_size = int.from_bytes(extension[:2], "big")
            if names_size != len(extension) - 2 or extension[2] != 0:
                raise ProxyRefusal(400, "invalid TLS SNI extension")
            name_size = int.from_bytes(extension[3:5], "big")
            if name_size != len(extension) - 5:
                raise ProxyRefusal(400, "invalid TLS SNI hostname")
            try:
                server_name = normalize_dns_name(bytes(extension[5:]).decode("ascii"))
            except (UnicodeDecodeError, ValueError) as error:
                raise ProxyRefusal(403, "TLS SNI hostname is invalid") from error
        offset = extension_end
    if server_name is None:
        raise ProxyRefusal(403, "TLS ClientHello is missing SNI")
    return server_name


def validate_connect_tls_identity(connection, buffered_data, expected_host):
    wire_data, handshake = receive_tls_client_hello(connection, buffered_data)
    server_name = tls_client_hello_server_name(handshake)
    if server_name != expected_host:
        raise ProxyRefusal(403, "TLS SNI does not match CONNECT target")
    return wire_data


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
            client_hello = validate_connect_tls_identity(self.request, buffered_body, host)
            upstream.sendall(client_hello)
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
        response = bytearray()
        expected = b"HTTP/1.1 403 Forbidden\r\n"
        while len(response) < len(expected):
            chunk = connection.recv(4096)
            if not chunk:
                break
            response.extend(chunk)
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
