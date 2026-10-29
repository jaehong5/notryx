import json
import socket
import socketserver
import threading
import time


class ContentConflictProxy(socketserver.ThreadingTCPServer):
    allow_reuse_address = True
    daemon_threads = True

    def __init__(self, target_port, document_id):
        super().__init__(("127.0.0.1", 0), Handler)
        self.target_port = target_port
        self.document_path = f"/v1/nodes/{document_id}/content"
        self.lock = threading.Lock()
        self.changed = threading.Condition(self.lock)
        self.hold_get = threading.Event()
        self.get_held = threading.Event()
        self.release_get = threading.Event()
        self.hold_original_request = threading.Event()
        self.original_request_held = threading.Event()
        self.release_original_request = threading.Event()
        self.original_operation_path = None
        self.hold_original_success = threading.Event()
        self.original_success_held = threading.Event()
        self.release_original_success = threading.Event()
        self.discard_original_success = threading.Event()
        self.original_success_discarded = threading.Event()
        self.hold_uncertain_lookup = threading.Event()
        self.release_uncertain_lookup = threading.Event()
        self.hold_put_412 = threading.Event()
        self.hold_all_original_412 = threading.Event()
        self.hold_original_412_lookup = threading.Event()
        self.put_412_held = threading.Event()
        self.release_put_412 = threading.Event()
        self.copy_content_path = None
        self.hold_copy_put = threading.Event()
        self.copy_put_held = threading.Event()
        self.release_copy_put = threading.Event()
        self.hold_ack = threading.Event()
        self.ack_held = threading.Event()
        self.release_ack = threading.Event()
        self.ack_target = None
        self.ack_observed = None
        self.hold_events = threading.Event()
        self.release_events = threading.Event()
        self.capture_changes = threading.Event()
        self.changes_seen = threading.Event()
        self.stopping = threading.Event()
        self.failure = threading.Event()
        self.get_count = 0
        self.get_statuses = []
        self.route_counts = {}
        self.response_counts = {}
        self.content_method_status_counts = {}
        self.trace = []
        self.inflight = {}
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

    def content_count(self, method, status):
        with self.lock:
            return self.content_method_status_counts.get((method, status), 0)

    def route_count(self, route):
        with self.lock:
            return self.route_counts.get(route, 0)

    def close(self):
        with self.changed:
            self.stopping.set()
            connections = list(self.connections)
        self.release_get.set()
        self.release_original_request.set()
        self.release_original_success.set()
        self.release_uncertain_lookup.set()
        self.release_put_412.set()
        self.release_copy_put.set()
        self.release_ack.set()
        self.release_events.set()
        for connection in connections:
            try:
                connection.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
        try:
            if self.thread.is_alive():
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
        self.counted_route = None
        try:
            self.forward()
        except (BrokenPipeError, ConnectionResetError):
            pass
        except (OSError, ValueError, TimeoutError):
            if not self.server.stopping.is_set():
                self.server.failure.set()
        finally:
            if self.counted_route is not None:
                with self.server.lock:
                    self.server.inflight[self.counted_route] -= 1

    def forward(self):
        self.connection.settimeout(30)
        request_line = self.rfile.readline(8193)
        if not request_line or len(request_line) > 8192:
            raise ValueError("invalid request line")
        method, path, _version = request_line.decode("ascii").strip().split(" ")
        headers = []
        total = len(request_line)
        length = 0
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
                length = int(value.strip())
            elif name == b"transfer-encoding":
                raise ValueError("unsupported transfer encoding")
            if name != b"connection":
                headers.append(line)
        if not 0 <= length <= 26 * 1024 * 1024:
            raise ValueError("request too large")
        if path == "/v1/pair":
            route = "pair"
        elif path == "/v1/server-info":
            route = "server-info"
        elif path.startswith("/v1/nodes?"):
            route = "inventory"
        elif path.startswith("/v1/changes?"):
            route = "changes"
        elif path == "/v1/events":
            route = "events"
        elif path == "/v1/operation-stream/abandon":
            route = "abandon"
        elif path == "/v1/operation-stream/ack":
            route = "ack"
        elif path == self.server.document_path:
            route = "content"
        else:
            route = "other"
        with self.server.lock:
            self.server.route_counts[route] = self.server.route_counts.get(route, 0) + 1
            self.server.inflight[route] = self.server.inflight.get(route, 0) + 1
            self.counted_route = route
            if self.server.capture_changes.is_set():
                self.server.trace.append(f"{route}:request")
        hold = False
        if method == "GET" and path.startswith("/v1/changes?") and self.server.capture_changes.is_set():
            with self.server.lock:
                if any(status in (410, 412) for status in self.server.get_statuses):
                    self.server.changes_seen.set()
        if method == "GET" and path == self.server.document_path:
            with self.server.lock:
                self.server.get_count += 1
                if self.server.hold_get.is_set():
                    self.server.hold_get.clear()
                    hold = True
        if hold:
            self.server.get_held.set()
            if not self.server.release_get.wait(45):
                raise TimeoutError("content request release timed out")
        if method == "PUT" and path == self.server.document_path and self.server.hold_original_request.is_set():
            self.server.original_request_held.set()
            if not self.server.release_original_request.wait(180):
                raise TimeoutError("original request release timed out")
        if method == "GET" and path == self.server.original_operation_path and self.server.hold_original_success.is_set():
            if not self.server.release_original_success.wait(180):
                raise TimeoutError("original operation lookup release timed out")
        if method == "GET" and path == self.server.original_operation_path and self.server.hold_uncertain_lookup.is_set():
            if not self.server.release_uncertain_lookup.wait(180):
                raise TimeoutError("uncertain operation lookup release timed out")
        if method == "GET" and path == self.server.original_operation_path and self.server.hold_original_412_lookup.is_set():
            if not self.server.release_put_412.wait(180):
                raise TimeoutError("conflict operation lookup release timed out")
        with socket.create_connection(("127.0.0.1", self.server.target_port), timeout=15) as upstream:
            with self.server.changed:
                if self.server.stopping.is_set():
                    return
                self.server.connections.add(upstream)
            try:
                self.forward_upstream(upstream, request_line, headers, length, method, path, route)
            finally:
                with self.server.changed:
                    self.server.connections.discard(upstream)
                    self.server.changed.notify_all()

    def forward_upstream(self, upstream, request_line, headers, length, method, path, route):
        upstream.settimeout(2)
        upstream.sendall(request_line + b"".join(headers) + b"Connection: close\r\n\r\n")
        remaining = length
        ack_body = bytearray() if method == "POST" and route == "ack" and length <= 65536 else None
        while remaining:
            chunk = self.rfile.read(min(65536, remaining))
            if not chunk:
                raise ValueError("incomplete request")
            if ack_body is not None:
                ack_body.extend(chunk)
            upstream.sendall(chunk)
            remaining -= len(chunk)
        ack_identity = None
        if ack_body is not None:
            try:
                ack = json.loads(ack_body)
                if isinstance(ack, dict) and isinstance(ack.get("device_stream_id"), str) and isinstance(ack.get("ack_through"), str):
                    ack_identity = (ack["device_stream_id"], ack["ack_through"])
            except (ValueError, UnicodeError):
                pass
        deadline = time.monotonic() + 180
        first_line = bytearray()
        while not self.server.stopping.is_set() and time.monotonic() < deadline:
            try:
                chunk = upstream.recv(65536)
            except socket.timeout:
                continue
            if not chunk:
                return
            if method == "GET" and path == "/v1/events" and self.server.hold_events.is_set():
                if not self.server.release_events.wait(45):
                    raise TimeoutError("event release timed out")
            if first_line is not None:
                first_line.extend(chunk)
                if b"\r\n" not in first_line:
                    if len(first_line) > 8192:
                        raise ValueError("invalid response status")
                    continue
                status = bytes(first_line).split(b"\r\n", 1)[0].split(b" ", 2)[1]
                with self.server.lock:
                    status = int(status)
                    key = f"{route}:{status}"
                    self.server.response_counts[key] = self.server.response_counts.get(key, 0) + 1
                    if path == self.server.document_path and method in ("GET", "PUT"):
                        method_key = (method, status)
                        self.server.content_method_status_counts[method_key] = self.server.content_method_status_counts.get(method_key, 0) + 1
                    if method == "GET" and path == self.server.document_path:
                        self.server.get_statuses.append(status)
                    if self.server.capture_changes.is_set():
                        self.server.trace.append(f"{route}:{status}")
                if method == "PUT" and path == self.server.document_path and 200 <= status < 300 and self.server.hold_original_success.is_set():
                    self.server.original_success_held.set()
                    if not self.server.release_original_success.wait(180):
                        raise TimeoutError("original response release timed out")
                    if self.server.discard_original_success.is_set():
                        self.server.original_success_discarded.set()
                        return
                if method == "PUT" and path == self.server.document_path and status == 412 and (self.server.hold_put_412.is_set() or self.server.hold_all_original_412.is_set()):
                    repeated = self.server.hold_all_original_412.is_set()
                    if not repeated:
                        self.server.hold_put_412.clear()
                    self.server.put_412_held.set()
                    if not self.server.release_put_412.wait(180 if repeated else 45):
                        raise TimeoutError("412 response release timed out")
                if method == "PUT" and path == self.server.copy_content_path and 200 <= status < 300 and self.server.hold_copy_put.is_set():
                    self.server.hold_copy_put.clear()
                    self.server.copy_put_held.set()
                    if not self.server.release_copy_put.wait(25):
                        raise TimeoutError("copy response release timed out")
                with self.server.lock:
                    target_ack = self.server.hold_ack.is_set() and ack_identity is not None and ack_identity == self.server.ack_target and 200 <= status < 300
                    if target_ack:
                        self.server.hold_ack.clear()
                        self.server.ack_observed = (*ack_identity, status)
                if target_ack:
                    self.server.ack_held.set()
                    if not self.server.release_ack.wait(25):
                        raise TimeoutError("ACK response release timed out")
                chunk = bytes(first_line)
                first_line = None
            self.wfile.write(chunk)
        if not self.server.stopping.is_set():
            raise TimeoutError("upstream response timed out")
