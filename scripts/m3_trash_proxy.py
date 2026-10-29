"""Observe selected HTTP statuses in an isolated loopback WebView test."""

import hashlib
import json
import select
import socket
import socketserver
import threading
import time


class TrashProxy(socketserver.ThreadingTCPServer):
    allow_reuse_address = True
    daemon_threads = True

    def __init__(self, target_port, node_id):
        super().__init__(("127.0.0.1", 0), Handler)
        self.target_port = target_port
        self.restore_path = f"/v1/nodes/{node_id}/restore"
        self.lock = threading.Lock()
        self.changed = threading.Condition(self.lock)
        self.stopping = threading.Event()
        self.failure = threading.Event()
        self.events = []
        self.restore_operation = None
        self.restore_requests = []
        self.abandon_operations = []
        self.acknowledgements = []
        self.connections = set()
        self.thread = threading.Thread(target=self.serve_forever, kwargs={"poll_interval": 0.1}, daemon=True)

    def start(self):
        self.thread.start()

    def handle_error(self, _request, _address):
        self.failure.set()

    def process_request(self, request, client_address):
        with self.changed:
            stopping = self.stopping.is_set()
            if not stopping:
                self.connections.add(request)
        if stopping:
            self.shutdown_request(request)
            return
        try:
            super().process_request(request, client_address)
        except BaseException:
            with self.changed:
                self.connections.discard(request)
                self.changed.notify_all()
            self.shutdown_request(request)
            raise

    def process_request_thread(self, request, client_address):
        try:
            super().process_request_thread(request, client_address)
        finally:
            with self.changed:
                self.connections.discard(request)
                self.changed.notify_all()

    def record(self, route, status):
        with self.changed:
            if len(self.events) >= 512:
                self.failure.set()
            else:
                self.events.append((route, status))
            self.changed.notify_all()

    def count(self, route, status=None):
        with self.lock:
            return sum(name == route and (status is None or actual == status) for name, actual in self.events)

    def close(self):
        with self.changed:
            self.stopping.set()
            connections = list(self.connections)
        for connection in connections:
            try:
                connection.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
        try:
            if self.thread.ident is not None and self.thread.is_alive():
                self.shutdown()
        finally:
            self.server_close()
            if self.thread.ident is not None:
                self.thread.join(timeout=5)
            deadline = time.monotonic() + 5
            with self.changed:
                while self.connections and time.monotonic() < deadline:
                    self.changed.wait(timeout=max(0, deadline - time.monotonic()))
                if self.connections or self.thread.is_alive():
                    raise RuntimeError("isolated proxy did not stop")


class Handler(socketserver.StreamRequestHandler):
    def handle(self):
        server = self.server
        try:
            self.forward()
        except (BrokenPipeError, ConnectionResetError):
            pass
        except (OSError, ValueError, TimeoutError, json.JSONDecodeError):
            if not server.stopping.is_set():
                server.failure.set()

    def forward(self):
        server = self.server
        self.connection.settimeout(20)
        request_line = self.rfile.readline(8193)
        if not request_line or len(request_line) > 8192:
            raise ValueError("invalid request line")
        method, path, _version = request_line.decode("ascii").strip().split(" ")
        route = (
            "restore" if method == "POST" and path == server.restore_path else
            "abandon" if method == "POST" and path == "/v1/operation-stream/abandon" else
            "ack" if method == "POST" and path == "/v1/operation-stream/ack" else
            "trash" if method == "GET" and (path == "/v1/trash" or path.startswith("/v1/trash?")) else
            "other"
        )
        is_sse = method == "GET" and path == "/v1/events"
        headers = []
        total = len(request_line)
        content_length = 0
        operation_id = None
        operation_seq = None
        device_stream = None
        while True:
            line = self.rfile.readline(8193)
            total += len(line)
            if not line or total > 65536:
                raise ValueError("invalid headers")
            if line == b"\r\n":
                break
            name, value = line.split(b":", 1)
            name = name.strip().lower()
            if name == b"content-length":
                content_length = int(value.strip())
            elif name == b"transfer-encoding":
                raise ValueError("unexpected request transfer encoding")
            elif name == b"idempotency-key":
                operation_id = value.strip().decode("ascii")
            elif name == b"x-operation-seq":
                operation_seq = value.strip().decode("ascii")
            elif name == b"x-device-stream":
                device_stream = value.strip().decode("ascii")
            if name != b"connection":
                headers.append(line)
        if not 0 <= content_length <= 65536:
            raise ValueError("unexpected request size")
        body = self.rfile.read(content_length)
        if len(body) != content_length:
            raise ValueError("incomplete request")
        if route in ("abandon", "ack"):
            payload = json.loads(body)
            with server.lock:
                if route == "abandon":
                    server.abandon_operations.append((payload.get("operation_id"), payload.get("operation_seq")))
                else:
                    server.acknowledgements.append(payload.get("ack_through"))
        if route == "restore":
            if not operation_id or not operation_seq or not device_stream:
                raise ValueError("restore has incomplete identity")
            with server.lock:
                if server.restore_operation not in (None, operation_id):
                    raise ValueError("unexpected restore operation")
                server.restore_operation = operation_id
                if len(server.restore_requests) >= 512:
                    raise ValueError("too many restore requests")
                server.restore_requests.append((operation_id, operation_seq, device_stream, hashlib.sha256(body).digest()))
        with socket.create_connection(("127.0.0.1", server.target_port), timeout=15) as upstream:
            upstream.settimeout(2)
            with server.changed:
                server.connections.add(upstream)
            try:
                upstream.sendall(request_line + b"".join(headers) + b"Connection: close\r\n\r\n" + body)
                first_line = bytearray()
                deadline = time.monotonic() + 180
                while not server.stopping.is_set() and time.monotonic() < deadline:
                    if is_sse:
                        readable, _, _ = select.select([upstream, self.connection], [], [], 1)
                        if self.connection in readable and not self.connection.recv(1, socket.MSG_PEEK):
                            return
                        if upstream not in readable:
                            continue
                    try:
                        chunk = upstream.recv(65536)
                    except socket.timeout:
                        continue
                    if not chunk:
                        return
                    if first_line is not None:
                        first_line.extend(chunk)
                        if b"\r\n" not in first_line:
                            if len(first_line) > 8192:
                                raise ValueError("invalid response line")
                            continue
                        response_status = int(bytes(first_line).split(b"\r\n", 1)[0].split(b" ", 2)[1])
                        if route != "other":
                            server.record(route, response_status)
                        chunk = bytes(first_line)
                        first_line = None
                    self.wfile.write(chunk)
                if not server.stopping.is_set():
                    raise TimeoutError("upstream did not finish")
            finally:
                with server.changed:
                    server.connections.discard(upstream)
                    server.changed.notify_all()
