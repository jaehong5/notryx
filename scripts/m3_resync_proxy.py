"""Loopback-only HTTP fault injection for the M3 WebDriver integration test."""

import json
import select
import socket
import socketserver
import threading
import time


class HeadObserver:
    def __init__(self):
        self.buffer = bytearray()
        self.events = bytearray()
        self.headers_read = False
        self.status = None
        self.chunked = False
        self.chunk_size = None

    def feed(self, data):
        self.buffer.extend(data)
        if not self.headers_read:
            end = self.buffer.find(b"\r\n\r\n")
            if end < 0:
                if len(self.buffer) > 65536:
                    raise ValueError("SSE response headers too large")
                return []
            headers = bytes(self.buffer[:end]).lower()
            status_line = headers.split(b"\r\n", 1)[0].split()
            if len(status_line) < 2 or not status_line[1].isdigit():
                raise ValueError("invalid SSE response status")
            self.status = int(status_line[1])
            self.chunked = b"transfer-encoding: chunked" in headers
            del self.buffer[:end + 4]
            self.headers_read = True
        if self.chunked:
            while True:
                if self.chunk_size is None:
                    end = self.buffer.find(b"\r\n")
                    if end < 0:
                        break
                    self.chunk_size = int(bytes(self.buffer[:end]).split(b";", 1)[0], 16)
                    del self.buffer[:end + 2]
                    if self.chunk_size > 65536:
                        raise ValueError("SSE chunk too large")
                if len(self.buffer) < self.chunk_size + 2:
                    break
                if self.buffer[self.chunk_size:self.chunk_size + 2] != b"\r\n":
                    raise ValueError("invalid SSE chunk")
                self.events.extend(self.buffer[:self.chunk_size])
                del self.buffer[:self.chunk_size + 2]
                self.chunk_size = None
        else:
            self.events.extend(self.buffer)
            self.buffer.clear()
        heads = []
        while (end := self.events.find(b"\n\n")) >= 0:
            frame = bytes(self.events[:end])
            del self.events[:end + 2]
            if b"event: head" not in frame:
                continue
            for line in frame.split(b"\n"):
                if line.startswith(b"id: "):
                    heads.append(int(line[4:]))
        if len(self.buffer) > 65536 or len(self.events) > 65536:
            raise ValueError("SSE frame too large")
        return heads


class ResyncProxy(socketserver.ThreadingTCPServer):
    allow_reuse_address = True
    daemon_threads = True

    def __init__(self, target_port, document_id):
        super().__init__(("127.0.0.1", 0), Handler)
        self.target_port = target_port
        self.document_path = f"/v1/nodes/{document_id}/content"
        self.lock = threading.Lock()
        self.hold_put = threading.Event()
        self.put_response_held = threading.Event()
        self.put_response_delivered = threading.Event()
        self.release_put = threading.Event()
        self.inject_changes = threading.Event()
        self.expected_head = None
        self.head_delivered = threading.Event()
        self.changes_injected = threading.Event()
        self.inventory_held = threading.Event()
        self.release_inventory = threading.Event()
        self.retry_lookup = threading.Event()
        self.stopping = threading.Event()
        self.operation_id = None
        self.put_started_at = None
        self.failure = threading.Event()
        self.timing_lock = threading.Lock()
        self.timing = {}
        self.sse_timing = []
        self.thread = threading.Thread(target=self.serve_forever, kwargs={"poll_interval": 0.1}, daemon=True)

    def mark(self, name):
        with self.timing_lock:
            self.timing.setdefault(name, time.monotonic())

    def diagnostics(self):
        with self.timing_lock:
            times = dict(self.timing)
            sse_events = list(self.sse_timing)
        origin = times.get("put_request")
        return {
            "put_relative_ms": {name: round((value - origin) * 1000) for name, value in times.items()} if origin is not None else {},
            "sse_events": [{"event": name, "ms": round((when - origin) * 1000), "status": status} for name, when, status in sse_events] if origin is not None else [],
            "put_seen": origin is not None,
            "proxy_failed": self.failure.is_set(),
        }

    def record_sse(self, name, status=None):
        with self.timing_lock:
            if len(self.sse_timing) < 64:
                self.sse_timing.append((name, time.monotonic(), status))

    def start(self):
        self.thread.start()

    def handle_error(self, _request, _address):
        self.failure.set()

    def close(self):
        self.stopping.set()
        self.release_put.set()
        self.release_inventory.set()
        try:
            if self.thread.ident is not None and self.thread.is_alive():
                self.shutdown()
        finally:
            self.server_close()
            if self.thread.ident is not None:
                self.thread.join(timeout=5)


class Handler(socketserver.StreamRequestHandler):
    def handle(self):
        try:
            self._handle_request()
        except (BrokenPipeError, ConnectionResetError):
            pass
        except (OSError, ValueError, TimeoutError):
            if not self.server.stopping.is_set():
                self.server.failure.set()

    def _handle_request(self):
        self.connection.settimeout(30)
        request_line = self.rfile.readline(8193)
        if not request_line or len(request_line) > 8192:
            raise ValueError("invalid request line")
        method, path, _version = request_line.decode("ascii").strip().split(" ")
        headers = []
        total = len(request_line)
        content_length = 0
        operation = None
        while True:
            line = self.rfile.readline(8193)
            total += len(line)
            if not line or total > 65536:
                raise ValueError("invalid request headers")
            if line == b"\r\n":
                break
            name, value = line.split(b":", 1)
            name = name.strip().lower()
            if name == b"content-length":
                content_length = int(value.strip())
            elif name == b"transfer-encoding":
                raise ValueError("unsupported request transfer encoding")
            elif name == b"idempotency-key":
                operation = value.strip().decode("ascii")
            if name != b"connection":
                headers.append(line)
        if not 0 <= content_length <= 26 * 1024 * 1024:
            raise ValueError("request body too large")

        server = self.server
        is_sse = method == "GET" and path == "/v1/events"
        if is_sse:
            server.record_sse("request")
        with server.lock:
            inject = method == "GET" and path.startswith("/v1/changes?") and server.inject_changes.is_set() and server.head_delivered.is_set()
            if inject:
                server.inject_changes.clear()
            hold_inventory = method == "GET" and path.startswith("/v1/nodes?") and server.changes_injected.is_set() and not server.inventory_held.is_set()
            if hold_inventory:
                server.mark("inventory_request")
                server.inventory_held.set()
            hold_put = method == "PUT" and path == server.document_path and server.hold_put.is_set()
            if hold_put:
                server.hold_put.clear()
                if not operation:
                    raise ValueError("missing operation id")
                server.operation_id = operation
                server.put_started_at = time.monotonic()
                server.mark("put_request")
            if method == "GET" and server.release_put.is_set() and server.operation_id and path == f"/v1/operations/{server.operation_id}":
                server.mark("operation_lookup")
                server.retry_lookup.set()

        if inject:
            server.mark("changes_410")
            body = json.dumps({"code": "resync_required", "message": "synthetic", "request_id": "019a1000-0000-7000-8000-000000000001"}).encode()
            server.changes_injected.set()
            self.wfile.write(b"HTTP/1.1 410 Gone\r\nContent-Type: application/json\r\nContent-Length: " + str(len(body)).encode() + b"\r\nConnection: close\r\n\r\n" + body)
            return
        if hold_inventory and not server.release_inventory.wait(60):
            raise TimeoutError("inventory release timed out")

        with socket.create_connection(("127.0.0.1", server.target_port), timeout=15) as upstream:
            upstream.settimeout(2)
            upstream.sendall(request_line + b"".join(headers) + b"Connection: close\r\n\r\n")
            remaining = content_length
            while remaining:
                chunk = self.rfile.read(min(65536, remaining))
                if not chunk:
                    raise ValueError("incomplete request body")
                upstream.sendall(chunk)
                remaining -= len(chunk)
            if hold_put:
                server.mark("put_forwarded")
                response = bytearray()
                deadline = server.put_started_at + 25
                while time.monotonic() < deadline:
                    try:
                        chunk = upstream.recv(65536)
                    except socket.timeout:
                        continue
                    if not chunk:
                        break
                    response.extend(chunk)
                    if len(response) > 1024 * 1024:
                        raise ValueError("PUT response too large")
                else:
                    raise TimeoutError("PUT response timed out")
                if not response.startswith(b"HTTP/1.1 200 "):
                    raise ValueError("unexpected PUT response")
                server.mark("put_response_received")
                server.put_response_held.set()
                if not server.release_put.wait(max(0, deadline - time.monotonic())):
                    server.mark("put_hold_expired")
                    raise TimeoutError("PUT release timed out")
                self.wfile.write(response)
                server.mark("put_response_delivered")
                server.put_response_delivered.set()
                return
            observer = HeadObserver() if is_sse else None
            reported_status = False
            deadline = time.monotonic() + 180
            while not server.stopping.is_set() and time.monotonic() < deadline:
                if is_sse:
                    readable, _, _ = select.select([upstream, self.connection], [], [], 1)
                    if self.connection in readable:
                        if not self.connection.recv(1, socket.MSG_PEEK):
                            server.record_sse("client_eof")
                            try:
                                upstream.shutdown(socket.SHUT_RDWR)
                            except OSError:
                                pass
                            server.record_sse("upstream_shutdown")
                            return
                        raise ValueError("unexpected SSE client input")
                    if upstream not in readable:
                        continue
                try:
                    chunk = upstream.recv(65536)
                except socket.timeout:
                    continue
                if not chunk:
                    if is_sse:
                        server.record_sse("upstream_eof")
                    return
                if observer is None:
                    self.wfile.write(chunk)
                else:
                    with server.lock:
                        heads = observer.feed(chunk)
                        self.wfile.write(chunk)
                        if observer.headers_read and not reported_status:
                            server.record_sse("response", observer.status)
                            reported_status = True
                        if server.expected_head is not None and any(head >= server.expected_head for head in heads):
                            server.mark("peer_head_delivered")
                            server.head_delivered.set()
            if not server.stopping.is_set():
                raise TimeoutError("upstream response timed out")
