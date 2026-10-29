import socket
import socketserver
import threading
import time


class OpenProxy(socketserver.ThreadingTCPServer):
    allow_reuse_address = True
    daemon_threads = True

    def __init__(self, target_port, document_id):
        super().__init__(("127.0.0.1", 0), Handler)
        self.target_port = target_port
        self.document_path = f"/v1/nodes/{document_id}/content"
        self.lock = threading.Lock()
        self.hold_get = threading.Event()
        self.get_held = threading.Event()
        self.release_get = threading.Event()
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
        self.trace = []
        self.inflight = {}
        self.thread = threading.Thread(target=self.serve_forever, kwargs={"poll_interval": 0.1}, daemon=True)

    def start(self):
        self.thread.start()

    def handle_error(self, _request, _address):
        self.failure.set()

    def close(self):
        self.stopping.set()
        self.release_get.set()
        self.release_events.set()
        if self.thread.is_alive():
            self.shutdown()
        self.server_close()
        if self.thread.ident is not None:
            self.thread.join(timeout=5)


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
        with socket.create_connection(("127.0.0.1", self.server.target_port), timeout=15) as upstream:
            upstream.settimeout(2)
            upstream.sendall(request_line + b"".join(headers) + b"Connection: close\r\n\r\n")
            remaining = length
            while remaining:
                chunk = self.rfile.read(min(65536, remaining))
                if not chunk:
                    raise ValueError("incomplete request")
                upstream.sendall(chunk)
                remaining -= len(chunk)
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
                        if method == "GET" and path == self.server.document_path:
                            self.server.get_statuses.append(status)
                        if self.server.capture_changes.is_set():
                            self.server.trace.append(f"{route}:{status}")
                    chunk = bytes(first_line)
                    first_line = None
                self.wfile.write(chunk)
            if not self.server.stopping.is_set():
                raise TimeoutError("upstream response timed out")
