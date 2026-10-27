#!/usr/bin/env python3
import argparse
import base64
import ctypes
import ctypes.util
import fcntl
import hashlib
import http.client
import http.server
import json
import os
from pathlib import Path
import platform
import select
import signal
import socket
import sqlite3
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request
import uuid


class TestFailure(Exception):
    pass


def require(condition, message):
    if not condition:
        raise TestFailure(message)


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def operation_id():
    value = bytearray(os.urandom(16))
    value[:6] = int(time.time() * 1000).to_bytes(6, "big")
    value[6] = (value[6] & 15) | 112
    value[8] = (value[8] & 63) | 128
    return str(uuid.UUID(bytes=bytes(value)))


def stop(process):
    if process and process.poll() is None:
        os.killpg(process.pid, signal.SIGTERM)
        try:
            process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait(timeout=10)


def driver_owns_port(pid, port):
    inodes = set()
    for line in Path("/proc/net/tcp").read_text().splitlines()[1:]:
        fields = line.split()
        if fields[1] == f"0100007F:{port:04X}" and fields[3] == "0A":
            inodes.add(fields[9])
    try:
        descriptors = Path(f"/proc/{pid}/fd").iterdir()
        return any(os.readlink(fd) in {f"socket:[{inode}]" for inode in inodes} for fd in descriptors)
    except (FileNotFoundError, PermissionError):
        return False


def xvfb_on_current_display():
    display = os.environ.get("DISPLAY", "").split(".", 1)[0]
    if not display.startswith(":") or not display[1:].isdigit():
        return False
    for process in Path("/proc").iterdir():
        if not process.name.isdigit():
            continue
        try:
            command = (process / "cmdline").read_bytes().split(b"\0")
            if command and Path(os.fsdecode(command[0])).name == "Xvfb" and display.encode() in command[1:]:
                return True
        except (OSError, ValueError):
            continue
    return False


def respond_to_recovery_dialog(destination):
    xlib = ctypes.CDLL(ctypes.util.find_library("X11"))
    xtest = ctypes.CDLL(ctypes.util.find_library("Xtst"))
    display_type = ctypes.c_void_p
    window_type = ctypes.c_ulong

    class WindowAttributes(ctypes.Structure):
        _fields_ = [
            ("x", ctypes.c_int), ("y", ctypes.c_int), ("width", ctypes.c_int), ("height", ctypes.c_int),
            ("border_width", ctypes.c_int), ("depth", ctypes.c_int), ("visual", ctypes.c_void_p),
            ("root", window_type), ("class_value", ctypes.c_int), ("bit_gravity", ctypes.c_int),
            ("win_gravity", ctypes.c_int), ("backing_store", ctypes.c_int),
            ("backing_planes", ctypes.c_ulong), ("backing_pixel", ctypes.c_ulong),
            ("save_under", ctypes.c_int), ("colormap", ctypes.c_ulong), ("map_installed", ctypes.c_int),
            ("map_state", ctypes.c_int), ("all_event_masks", ctypes.c_long),
            ("your_event_mask", ctypes.c_long), ("do_not_propagate_mask", ctypes.c_long),
            ("override_redirect", ctypes.c_int), ("screen", ctypes.c_void_p),
        ]

    xlib.XOpenDisplay.argtypes = [ctypes.c_char_p]
    xlib.XOpenDisplay.restype = display_type
    xlib.XDefaultRootWindow.argtypes = [display_type]
    xlib.XDefaultRootWindow.restype = window_type
    xlib.XQueryTree.argtypes = [display_type, window_type, ctypes.POINTER(window_type), ctypes.POINTER(window_type), ctypes.POINTER(ctypes.POINTER(window_type)), ctypes.POINTER(ctypes.c_uint)]
    xlib.XGetWindowAttributes.argtypes = [display_type, window_type, ctypes.POINTER(WindowAttributes)]
    xlib.XInternAtom.argtypes = [display_type, ctypes.c_char_p, ctypes.c_int]
    xlib.XInternAtom.restype = ctypes.c_ulong
    xlib.XGetWindowProperty.argtypes = [display_type, window_type, ctypes.c_ulong, ctypes.c_long, ctypes.c_long, ctypes.c_int, ctypes.c_ulong, ctypes.POINTER(ctypes.c_ulong), ctypes.POINTER(ctypes.c_int), ctypes.POINTER(ctypes.c_ulong), ctypes.POINTER(ctypes.c_ulong), ctypes.POINTER(ctypes.POINTER(ctypes.c_ubyte))]
    xlib.XFree.argtypes = [ctypes.c_void_p]
    xlib.XSetInputFocus.argtypes = [display_type, window_type, ctypes.c_int, ctypes.c_ulong]
    xlib.XRaiseWindow.argtypes = [display_type, window_type]
    xlib.XStringToKeysym.argtypes = [ctypes.c_char_p]
    xlib.XStringToKeysym.restype = ctypes.c_ulong
    xlib.XKeysymToKeycode.argtypes = [display_type, ctypes.c_ulong]
    xlib.XKeysymToKeycode.restype = ctypes.c_ubyte
    xlib.XFlush.argtypes = [display_type]
    xlib.XCloseDisplay.argtypes = [display_type]
    xtest.XTestFakeKeyEvent.argtypes = [display_type, ctypes.c_uint, ctypes.c_int, ctypes.c_ulong]
    display = xlib.XOpenDisplay(None)
    require(bool(display), "isolated X display could not be opened")
    try:
        root = xlib.XDefaultRootWindow(display)
        name_atom = xlib.XInternAtom(display, b"_NET_WM_NAME", 0)

        def dialog_window():
            root_return, parent = window_type(), window_type()
            children = ctypes.POINTER(window_type)()
            count = ctypes.c_uint()
            if not xlib.XQueryTree(display, root, ctypes.byref(root_return), ctypes.byref(parent), ctypes.byref(children), ctypes.byref(count)):
                return None
            try:
                for index in range(count.value):
                    child = children[index]
                    actual_type, length, remaining = ctypes.c_ulong(), ctypes.c_ulong(), ctypes.c_ulong()
                    actual_format = ctypes.c_int()
                    data = ctypes.POINTER(ctypes.c_ubyte)()
                    status = xlib.XGetWindowProperty(display, child, name_atom, 0, 1024, 0, 0, ctypes.byref(actual_type), ctypes.byref(actual_format), ctypes.byref(length), ctypes.byref(remaining), ctypes.byref(data))
                    try:
                        if status == 0 and bool(data) and actual_format.value == 8:
                            title = ctypes.string_at(data, length.value).decode("utf-8", "replace")
                            attributes = WindowAttributes()
                            if title == "원문 내보내기" and xlib.XGetWindowAttributes(display, child, ctypes.byref(attributes)) and attributes.map_state == 2:
                                return child
                    finally:
                        if data:
                            xlib.XFree(data)
            finally:
                if children:
                    xlib.XFree(children)
            return None

        deadline = time.monotonic() + 15
        window = None
        while time.monotonic() < deadline and window is None:
            window = dialog_window()
            if window is None:
                time.sleep(0.05)
        require(window is not None, "recovery file dialog did not appear")
        xlib.XRaiseWindow(display, window)
        xlib.XSetInputFocus(display, window, 2, 0)
        xlib.XFlush(display)
        time.sleep(0.15)

        def key(name, pressed):
            symbol = xlib.XStringToKeysym(name.encode("ascii"))
            code = xlib.XKeysymToKeycode(display, symbol)
            require(bool(code) and bool(xtest.XTestFakeKeyEvent(display, code, int(pressed), 0)), "isolated dialog keyboard input failed")
            xlib.XFlush(display)

        def tap(name):
            key(name, True)
            key(name, False)
            time.sleep(0.01)

        if destination is None:
            tap("Escape")
        else:
            key("Control_L", True)
            tap("l")
            key("Control_L", False)
            time.sleep(0.2)
            key("Control_L", True)
            tap("a")
            key("Control_L", False)
            for character in str(destination):
                if character == "_":
                    key("Shift_L", True)
                    tap("minus")
                    key("Shift_L", False)
                else:
                    name = {"/": "slash", "-": "minus", ".": "period"}.get(character, character)
                    require(len(name) == 1 and name.isascii() and name.isalnum() or name in ("slash", "minus", "period"), "unsupported isolated test path")
                    tap(name)
            tap("Return")
    finally:
        xlib.XCloseDisplay(display)


def proc_fields(path):
    try:
        values = {}
        for line in Path(path).read_text().splitlines():
            name, separator, value = line.partition(":")
            if separator and value.strip().split() and value.strip().split()[0].isdigit():
                values[name] = int(value.strip().split()[0])
        return values
    except (OSError, ValueError):
        return {}


def client_pid(driver_pid, executable):
    pending = [driver_pid]
    visited = set()
    while pending:
        pid = pending.pop()
        if pid in visited:
            continue
        visited.add(pid)
        try:
            if Path(f"/proc/{pid}/exe").resolve(strict=True) == Path(executable):
                return pid
            for task in Path(f"/proc/{pid}/task").iterdir():
                children = task / "children"
                pending.extend(int(value) for value in children.read_text().split())
        except (OSError, ValueError):
            continue
    return None


class ResourceMeter:
    def __init__(self, pid):
        self.pid = pid
        self.stop_event = threading.Event()
        self.samples = []
        self.worker = threading.Thread(target=self.sample, daemon=True)

    def sample(self):
        while not self.stop_event.is_set():
            self.sample_once()
            self.stop_event.wait(0.2)

    def sample_once(self):
        memory = proc_fields("/proc/meminfo")
        status = proc_fields(f"/proc/{self.pid}/status")
        io = proc_fields(f"/proc/{self.pid}/io")
        if memory and status and io:
            self.samples.append({
                "available_kib": memory.get("MemAvailable", 0),
                "swap_used_kib": memory.get("SwapTotal", 0) - memory.get("SwapFree", 0),
                "rss_kib": status.get("VmRSS", 0),
                "write_bytes": io.get("write_bytes", 0),
            })

    def __enter__(self):
        self.sample_once()
        self.worker.start()
        return self

    def __exit__(self, *_):
        self.stop_event.set()
        self.worker.join(timeout=5)
        self.sample_once()

    def result(self, elapsed):
        require(len(self.samples) >= 2, "resource sampling did not produce enough observations")
        initial, final = self.samples[0], self.samples[-1]
        return {
            "app_process_write_bytes": max(0, final["write_bytes"] - initial["write_bytes"]),
            "app_process_write_mib_per_second": max(0, final["write_bytes"] - initial["write_bytes"]) / elapsed / 1048576,
            "app_process_sampled_peak_rss_kib": max(sample["rss_kib"] for sample in self.samples),
            "host_min_available_kib": min(sample["available_kib"] for sample in self.samples),
            "host_swap_growth_kib": max(0, max(sample["swap_used_kib"] for sample in self.samples) - initial["swap_used_kib"]),
            "sample_interval_ms": 200,
        }


class CheckpointMeter:
    def __init__(self, path, node_ids):
        self.path = path
        self.node_ids = node_ids
        self.stop_event = threading.Event()
        self.ready = threading.Event()
        self.events = []
        self.failed = False
        self.worker = threading.Thread(target=self.sample, daemon=True)

    def sample(self):
        previous = {}
        first = True
        try:
            with sqlite3.connect(f"file:{self.path}?mode=ro", uri=True, timeout=5) as db:
                while True:
                    rows = db.execute("""
                        SELECT d.document_id, c.generation FROM registered_documents d
                        JOIN client_registrations r ON r.id = d.registration_id
                        JOIN draft_checkpoints c ON c.document_id = d.storage_id
                        WHERE r.status = 'active' AND d.document_id IN (?, ?)
                    """, self.node_ids).fetchall()
                    observed_at = time.time() * 1000
                    current = dict(rows)
                    if not first:
                        for document, generation in current.items():
                            if generation > previous.get(document, 0):
                                self.events.append({"document": document, "generation": generation, "observed_at": observed_at})
                    previous = current
                    first = False
                    self.ready.set()
                    if self.stop_event.is_set():
                        break
                    self.stop_event.wait(0.025)
        except (sqlite3.Error, OSError):
            self.failed = True
            self.ready.set()

    def __enter__(self):
        self.worker.start()
        require(self.ready.wait(5) and not self.failed, "checkpoint observer could not start")
        return self

    def __exit__(self, *_):
        self.stop_event.set()
        self.worker.join(timeout=5)
        require(not self.worker.is_alive() and not self.failed, "checkpoint observer failed")


def stats(values):
    require(bool(values), "performance phase produced no timing samples")
    values = sorted(values)
    return {
        "samples": len(values),
        "p50_ms": values[(len(values) * 50 + 99) // 100 - 1],
        "p95_ms": values[(len(values) * 95 + 99) // 100 - 1],
        "max_ms": values[-1],
    }


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--driver", default="WebKitWebDriver")
    parser.add_argument("--binary", default="target/debug/notryx-client")
    parser.add_argument("--server", default="target/debug/notryx-server")
    parser.add_argument("--measure", action="store_true")
    parser.add_argument("--measure-output", default=".artifacts/m2-performance.json")
    parser.add_argument("--paste-probe", action="store_true")
    parser.add_argument("--recovery-dialogs", action="store_true")
    args = parser.parse_args()
    require(not (args.measure and args.recovery_dialogs), "performance and dialog verification must run separately")
    require(not args.paste_probe or args.measure, "paste probe requires the release measurement setup")
    server_binary = str(Path(args.server).resolve())
    client_binary = str(Path(args.binary).resolve())
    if args.measure:
        require(Path(client_binary) == Path("target/release/notryx-client").resolve(),
                "performance measurement requires the release client binary")
        require(xvfb_on_current_display(), "performance measurement requires a local Xvfb display")
    if args.recovery_dialogs:
        require(xvfb_on_current_display(), "recovery dialog verification requires an isolated Xvfb display")
    server = driver = remote_probe = remote_thread = None
    session_id = None

    with tempfile.TemporaryDirectory(prefix="notryx-m2-webdriver-") as temporary:
        root = Path(temporary)
        notes, state, run, client_data = (root / name for name in ("notes", "state", "run", "client"))
        for directory in (notes, state, run, client_data):
            directory.mkdir(mode=0o700)
        port = free_port()
        config = root / "server.toml"
        config.write_text(
            f'[server]\nlisten = "127.0.0.1:{port}"\nadmin_socket = "{run}/admin.sock"\n'
            f'[workspace]\nnotes_root = "{notes}"\nstate_root = "{state}"\n', encoding="utf-8"
        )
        config.chmod(0o600)

        def cli(*arguments):
            try:
                result = subprocess.run(
                    [server_binary, *arguments], stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                    text=True, timeout=30, check=True,
                )
                return result.stdout
            except (OSError, subprocess.SubprocessError):
                raise TestFailure("synthetic server command failed") from None

        def admin(*arguments):
            try:
                return json.loads(cli("admin", "--config", str(config), *arguments))
            except (ValueError, KeyError):
                raise TestFailure("synthetic server admin response was invalid") from None

        def server_request(method, path, *, data=None, headers=None, expected=200):
            connection = http.client.HTTPConnection("127.0.0.1", port, timeout=20)
            try:
                connection.request(method, path, data, headers or {})
                response = connection.getresponse()
                body = response.read()
                if response.status != expected:
                    raise TestFailure("synthetic server returned an unexpected status")
                return json.loads(body) if body else None
            except (OSError, http.client.HTTPException, ValueError):
                raise TestFailure("synthetic server request failed") from None
            finally:
                connection.close()

        def wait(check, seconds=20):
            deadline = time.monotonic() + seconds
            while time.monotonic() < deadline:
                result = check()
                if result:
                    return result
                time.sleep(0.1)
            raise TestFailure("timed out waiting for expected application state")

        try:
            remote_requests = []

            class RemoteImageProbe(http.server.BaseHTTPRequestHandler):
                def do_GET(self):
                    remote_requests.append(True)
                    self.send_response(204)
                    self.end_headers()

                def log_message(self, _format, *args):
                    pass

            class QuietProbeServer(http.server.HTTPServer):
                def handle_error(self, _request, _client_address):
                    pass

            remote_probe = QuietProbeServer(("127.0.0.1", 0), RemoteImageProbe)
            remote_thread = threading.Thread(target=remote_probe.serve_forever, daemon=True)
            remote_thread.start()
            cli("init", "--config", str(config))
            server = subprocess.Popen(
                [server_binary, "serve", "--config", str(config)], stdout=subprocess.PIPE,
                stderr=subprocess.DEVNULL, text=True, start_new_session=True,
            )
            ready, _, _ = select.select([server.stdout], [], [], 30)
            require(bool(ready) and server.stdout.readline().strip() == "server_started" and server.poll() is None,
                    "synthetic server did not start")
            admin_status = admin("status")
            setup_grant = admin("issue-pairing", admin_status["owner_principal_id"])
            pair = server_request("POST", "/v1/pair", data=json.dumps({
                "protocol": "mdw-sync/1.2", "workspace_id": setup_grant["workspace_id"],
                "pairing_secret": setup_grant["pairing_secret"],
            }).encode(), headers={"Content-Type": "application/json"})
            authorization = {"Authorization": f"Bearer {pair['device_token']}"}
            info = server_request("GET", "/v1/server-info", headers=authorization)
            folder_id, document_id = operation_id(), operation_id()

            def create(identifier, parent, name, kind, sequence, upload_id=None):
                payload = {"node_id": identifier, "parent_id": parent, "name": name, "kind": kind}
                if upload_id is not None:
                    payload["upload_id"] = upload_id
                headers = {
                    **authorization, "Content-Type": "application/json", "X-Workspace-Epoch": info["workspace_epoch"],
                    "X-Device-Stream": pair["device_stream_id"], "X-Operation-Seq": str(sequence),
                    "Idempotency-Key": operation_id(),
                }
                return server_request("POST", "/v1/nodes", data=json.dumps(payload).encode(), headers=headers)

            create(folder_id, info["root_id"], "Test Folder", "folder", 1)
            png = base64.b64decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==")
            png_digest = base64.b64encode(hashlib.sha256(png).digest()).decode()
            png_upload = server_request("POST", "/v1/uploads", data=png, expected=201, headers={
                **authorization, "Content-Type": "application/octet-stream", "Content-Digest": f"sha-256=:{png_digest}:",
            })
            create(operation_id(), folder_id, "pixel.png", "file", 2, png_upload["upload_id"])
            original = ("\ufeff# Synthetic\r\n\r\nOriginal source.\r\n\r\n"
                        "![local](pixel.png)\r\n"
                        f"![remote](http://127.0.0.1:{remote_probe.server_port}/image)\r\n").encode()
            digest = base64.b64encode(hashlib.sha256(original).digest()).decode()
            upload = server_request("POST", "/v1/uploads", data=original, expected=201, headers={
                **authorization, "Content-Type": "application/octet-stream", "Content-Digest": f"sha-256=:{digest}:",
            })
            create(document_id, folder_id, "Example.md", "file", 3, upload["upload_id"])
            readonly_id = operation_id()
            readonly_size = 5 * 1024 * 1024 + 1
            readonly_line = b"r" * 63 + b"\n"
            readonly_source = (readonly_line * (readonly_size // len(readonly_line) + 1))[:readonly_size]
            readonly_digest = base64.b64encode(hashlib.sha256(readonly_source).digest()).decode()
            readonly_upload = server_request("POST", "/v1/uploads", data=readonly_source, expected=201, headers={
                **authorization, "Content-Type": "application/octet-stream", "Content-Digest": f"sha-256=:{readonly_digest}:",
            })
            create(readonly_id, folder_id, "Readonly.md", "file", 4, readonly_upload["upload_id"])
            performance_nodes = {}
            if args.measure:
                sequence = 4
                for size in (100 * 1024, 5 * 1024 * 1024):
                    line = ("# Synthetic measurement\n" + "x" * 39 + "\n").encode()
                    content = (line * (size // len(line) + 1))[:size]
                    for suffix in ("A", "B"):
                        digest = base64.b64encode(hashlib.sha256(content).digest()).decode()
                        measured_upload = server_request("POST", "/v1/uploads", data=content, expected=201, headers={
                            **authorization, "Content-Type": "application/octet-stream", "Content-Digest": f"sha-256=:{digest}:",
                        })
                        identifier = operation_id()
                        sequence += 1
                        create(identifier, folder_id, f"Perf {size} {suffix}.md", "file", sequence, measured_upload["upload_id"])
                        performance_nodes[(size, suffix)] = identifier
            ui_grant = admin("issue-pairing", admin_status["owner_principal_id"])
            driver_port = free_port()
            driver_origin = f"http://127.0.0.1:{driver_port}"
            environment = os.environ.copy()
            environment.update(TAURI_WEBVIEW_AUTOMATION="true", NOTRYX_M0_DATA_DIR=str(client_data))
            if args.recovery_dialogs:
                environment["GTK_USE_PORTAL"] = "0"
            driver = subprocess.Popen(
                [args.driver, f"--port={driver_port}", "--host=127.0.0.1"], env=environment,
                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True,
            )

            def wd(method, path, payload=None):
                data = None if payload is None else json.dumps(payload).encode()
                request = urllib.request.Request(driver_origin + path, data=data, method=method,
                                                 headers={"Content-Type": "application/json"})
                try:
                    with urllib.request.urlopen(request, timeout=180 if args.measure else 35) as response:
                        result = json.load(response)
                    value = result["value"]
                    if isinstance(value, dict) and "error" in value:
                        code = value["error"]
                        safe_codes = {"script timeout", "timeout", "javascript error", "no such window", "invalid session id", "unknown error"}
                        raise TestFailure(f"WebDriver error: {code if code in safe_codes else 'other'}")
                    return value
                except (OSError, ValueError, KeyError, urllib.error.HTTPError):
                    raise TestFailure("WebDriver request failed") from None

            wait(lambda: driver.poll() is None and driver_owns_port(driver.pid, driver_port))

            def driver_ready():
                try:
                    return wd("GET", "/status")
                except TestFailure:
                    return False

            wait(driver_ready)
            capabilities = {"capabilities": {"alwaysMatch": {"browserName": "wry", "webkitgtk:browserOptions": {"binary": client_binary}}}}
            session_id = wd("POST", "/session", capabilities)["sessionId"]
            wd("POST", f"/session/{session_id}/timeouts", {"script": 150000 if args.measure else 30000})

            def js(script, *values, asynchronous=False):
                mode = "async" if asynchronous else "sync"
                return wd("POST", f"/session/{session_id}/execute/{mode}", {"script": script, "args": list(values)})

            def native_result(command, arguments):
                return js("""
                    const [command, args, done] = arguments;
                    window.__TAURI_INTERNALS__.invoke(command, args)
                      .then(() => done({ok:true}))
                      .catch(error => done({ok:false, failure:String(error)}));
                """, command, arguments, asynchronous=True)

            def checkpoint(node_id=document_id):
                db_path = client_data / "drafts.sqlite3"
                if not db_path.exists():
                    return None
                with sqlite3.connect(f"file:{db_path}?mode=ro", uri=True) as db:
                    return db.execute("""
                        SELECT c.generation, c.bytes, b.bytes FROM registered_documents d
                        JOIN client_registrations r ON r.id = d.registration_id
                        JOIN draft_checkpoints c ON c.document_id = d.storage_id
                        JOIN draft_bases b ON b.document_id = d.storage_id
                        WHERE r.status = 'active' AND d.document_id = ?
                    """, (node_id,)).fetchone()

            def durable_generation(node_id):
                with sqlite3.connect(f"file:{client_data / 'drafts.sqlite3'}?mode=ro", uri=True) as db:
                    row = db.execute("""
                        SELECT c.generation FROM registered_documents d
                        JOIN client_registrations r ON r.id = d.registration_id
                        JOIN draft_checkpoints c ON c.document_id = d.storage_id
                        WHERE r.status = 'active' AND d.document_id = ?
                    """, (node_id,)).fetchone()
                return row[0] if row else 0

            def acknowledged(generation):
                with sqlite3.connect(f"file:{client_data / 'drafts.sqlite3'}?mode=ro", uri=True) as db:
                    row = db.execute("""
                        SELECT r.device_stream_id, o.operation_seq, s.ack_through
                        FROM client_registrations r
                        JOIN client_outbox o ON o.registration_id = r.id
                        JOIN outbox_streams s ON s.registration_id = r.id
                        WHERE r.status = 'active' AND o.document_id = ? AND o.generation = ?
                          AND o.status = 'committed' AND o.terminal_kind IS NULL
                        ORDER BY o.operation_seq DESC LIMIT 1
                    """, (document_id, generation)).fetchone()
                if row is None or row[2] < row[1]:
                    return False
                with sqlite3.connect(f"file:{state / 'state.db'}?mode=ro", uri=True) as db:
                    watermark = db.execute("SELECT last_seq, retired_through FROM operation_streams WHERE stream_id = ?", (row[0],)).fetchone()
                return watermark is not None and int(watermark[0]) == row[1] and int(watermark[1]) >= row[1]

            def operation_count():
                with sqlite3.connect(f"file:{client_data / 'drafts.sqlite3'}?mode=ro", uri=True) as db:
                    return db.execute("""
                        SELECT COUNT(*) FROM client_outbox o
                        JOIN client_registrations r ON r.id = o.registration_id
                        WHERE r.status = 'active' AND o.document_id = ?
                    """, (document_id,)).fetchone()[0]

            wait(lambda: js("return !!document.querySelector('.cm-content[contenteditable=true]')"))
            wait(lambda: js("return !document.querySelector('.workspace-connect button').disabled"))
            js("""
                const form = document.querySelector('.workspace-connect');
                form.elements.namedItem('origin').value = arguments[0];
                form.elements.namedItem('workspace_id').value = arguments[1];
                form.elements.namedItem('pairing_secret').value = arguments[2];
                form.requestSubmit();
            """, f"http://127.0.0.1:{port}", ui_grant["workspace_id"], ui_grant["pairing_secret"])
            wait(lambda: js("return !!document.querySelector('[data-document=\"' + arguments[0] + '\"]:not(:disabled)')", document_id))
            require(js("return document.querySelector('[name=pairing_secret]').value === ''"), "pairing secret remained in the form")
            require(js("return document.querySelector('[data-document=\"' + arguments[0] + '\"]').textContent", document_id) == "Test Folder/Example.md", "folder path was not shown")
            js("document.querySelector('[data-document=\"' + arguments[0] + '\"]').click()", document_id)
            wait(lambda: js("return document.querySelector('[aria-current=page]')?.dataset.document === arguments[0] && !!document.querySelector('.cm-content[contenteditable=true]')", document_id))
            require(checkpoint() is None, "unmodified document unexpectedly created a draft")
            opened_text = js("return document.querySelector('.cm-content').cmTile.root.view.state.doc.toString()")
            require(opened_text == original.decode("utf-8-sig").replace("\r\n", "\n"), "editor did not open the expected source")
            if args.recovery_dialogs:
                export_dir = root / "exports"
                export_dir.mkdir(mode=0o700)
                js("document.querySelector('#copy-source').click()")
                wait(lambda: js("return [...document.querySelectorAll('[role=status]')].some(e=>e.textContent.includes('원문을 복사했습니다'))"), seconds=30)
                clipboard_probe = """
import hashlib, json, gi
gi.require_version('Gtk', '3.0')
from gi.repository import Gdk, Gtk
clipboard = Gtk.Clipboard.get(Gdk.SELECTION_CLIPBOARD)
value = clipboard.wait_for_text()
if value is None:
    raise SystemExit(1)
data = value.encode('utf-8')
raw = clipboard.wait_for_contents(Gdk.Atom.intern('UTF8_STRING', False))
raw_data = bytes(raw.get_data()) if raw is not None and raw.get_data() is not None else b''
print(json.dumps({'sha256':hashlib.sha256(data).hexdigest(), 'length':len(data),
                  'bom':data.startswith(bytes([239,187,191])),
                  'crlf':data.count(bytes([13,10])), 'cr':data.count(bytes([13])), 'lf':data.count(bytes([10])),
                  'raw_sha256':hashlib.sha256(raw_data).hexdigest(),
                  'raw_length':len(raw_data), 'raw_bom':raw_data.startswith(bytes([239,187,191])),
                  'raw_crlf':raw_data.count(bytes([13,10])), 'raw_cr':raw_data.count(bytes([13])), 'raw_lf':raw_data.count(bytes([10]))}))
"""
                try:
                    copied = json.loads(subprocess.run([sys.executable, "-c", clipboard_probe], env=environment,
                                                       stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                                                       text=True, timeout=10, check=True).stdout)
                except (OSError, subprocess.SubprocessError, ValueError):
                    raise TestFailure("isolated clipboard could not be inspected") from None
                normalized_text = original.decode("utf-8").replace("\r\n", "\n").replace("\r", "\n").encode("utf-8")
                if (copied["raw_sha256"] != hashlib.sha256(original).hexdigest()
                        or copied["sha256"] != hashlib.sha256(normalized_text).hexdigest()):
                    expected_meta = {"length": len(original), "bom": original.startswith(b"\xef\xbb\xbf"),
                                     "crlf": original.count(b"\r\n"), "cr": original.count(b"\r"), "lf": original.count(b"\n")}
                    copied_meta = {key: copied[key] for key in expected_meta}
                    raw_meta = {key: copied[f"raw_{key}"] for key in expected_meta}
                    print(json.dumps({"clipboard_metadata": copied_meta, "raw_selection_metadata": raw_meta, "expected_metadata": expected_meta}))
                    raise TestFailure("copied source did not preserve original bytes")

                def export(destination):
                    failures = []

                    def interact():
                        try:
                            respond_to_recovery_dialog(destination)
                        except Exception:
                            failures.append(True)

                    worker = threading.Thread(target=interact, daemon=True)
                    worker.start()
                    try:
                        js("document.querySelector('#export-source').click()")
                    finally:
                        worker.join(timeout=20)
                    require(not worker.is_alive() and not failures, "recovery dialog interaction failed")
                    wait(lambda: js("return !document.querySelector('#export-source').disabled"), seconds=30)

                wait(lambda: js("return !!document.querySelector('#export-source')"))
                export(None)
                require(not list(export_dir.iterdir()), "cancelled export created a file")
                target = export_dir / "new.md"
                export(target)
                wait(lambda: target.exists(), seconds=30)
                require(target.read_bytes() == original, "new export did not contain the original bytes")
                existing = export_dir / "existing.md"
                existing.write_bytes(b"synthetic pre-existing file\n")
                export(existing)
                require(existing.read_bytes() == b"synthetic pre-existing file\n", "export replaced an existing file")
                wait(lambda: js("return [...document.querySelectorAll('[role=status]')].some(e=>e.textContent.includes('이미 있는 파일은 변경하지 않았습니다'))"), seconds=30)
                require(set(export_dir.iterdir()) == {target, existing}, "recovery export left unexpected files")
            js("document.querySelector('#preview-toggle').click()")
            wait(lambda: js("const image=document.querySelector('.markdown-preview img[alt=local]'); return !!image && image.src.startsWith('blob:') && image.complete && image.naturalWidth === 1"))
            time.sleep(0.5)
            require(not remote_requests and not js("return !!document.querySelector('.markdown-preview img[alt=remote]')"), "preview requested a remote image")
            js("document.querySelector('#preview-toggle').click()")
            before_reload = "편집 입력😀\n"
            js("""
                const started=performance.now();
                window.addEventListener('pagehide',()=>sessionStorage.setItem('notryx-m2-reload-probe',JSON.stringify({elapsed:performance.now()-started})),{once:true});
                const view=document.querySelector('.cm-content').cmTile.root.view;
                view.dispatch({changes:{from:view.state.doc.length,insert:arguments[0]}});
                document.querySelector('#save').click();
            """, before_reload)
            saved = wait(lambda: (row if (row := checkpoint()) and before_reload.encode() in row[1] else None))
            require(saved[1] == original.replace(b"\r\n", b"\n") + before_reload.encode(), "edited checkpoint did not preserve source")
            require(saved[2] == original and operation_count() == 0, "checkpoint-to-reload test reached the queue too late")
            synced_bytes = saved[1]
            wd("POST", f"/session/{session_id}/refresh", {})
            reload_probe = js("const value=sessionStorage.getItem('notryx-m2-reload-probe'); sessionStorage.removeItem('notryx-m2-reload-probe'); return value ? JSON.parse(value) : null")
            require(reload_probe is not None and reload_probe["elapsed"] < 750, "checkpoint-to-reload test did not exercise the pre-queue interval")
            wait(lambda: js("return !!document.querySelector('[data-document=\"' + arguments[0] + '\"]:not(:disabled)')", document_id))
            js("document.querySelector('[data-document=\"' + arguments[0] + '\"]').click()", document_id)
            wait(lambda: js("return document.querySelector('[aria-current=page]')?.dataset.document === arguments[0] && !!document.querySelector('.cm-content[contenteditable=true]')", document_id))
            reopened = js("return document.querySelector('.cm-content').cmTile.root.view.state.doc.toString()")
            require(before_reload in reopened and checkpoint()[:2] == saved[:2], "reload did not recover the checkpoint")
            wait(lambda: acknowledged(saved[0]) and (notes / "Test Folder" / "Example.md").read_bytes() == synced_bytes and checkpoint()[2] == synced_bytes, seconds=45)
            wait(lambda: js("return document.querySelector('#sync-status')?.textContent.includes('서버 반영됨')"), seconds=45)
            time.sleep(5.5)
            require(operation_count() == 1 and acknowledged(saved[0]), "reload produced duplicate or unacknowledged operations")
            saved = checkpoint()
            js("document.querySelector('[data-document=\"' + arguments[0] + '\"]').click()", readonly_id)
            wait(lambda: js("return document.querySelector('[aria-current=page]')?.dataset.document === arguments[0] && !!document.querySelector('.cm-content[contenteditable=false]')", readonly_id), seconds=45)
            readonly_result = js("""
                const content=document.querySelector('.cm-content');
                const view=content.cmTile.root.view;
                const before=view.state.doc.toString();
                content.focus();
                document.execCommand('insertText',false,'unexpected-edit');
                return {beforeLength:before.length,afterLength:view.state.doc.length,unchanged:before===view.state.doc.toString()};
            """)
            js("document.querySelector('#save').click()")
            require(readonly_result["beforeLength"] == readonly_size and readonly_result["unchanged"] and checkpoint(readonly_id) is None, "oversized Markdown did not stay read-only")
            require((notes / "Test Folder" / "Readonly.md").read_bytes() == readonly_source, "read-only document source changed")
            js("document.querySelector('[data-document=\"' + arguments[0] + '\"]').click()", document_id)
            wait(lambda: js("return document.querySelector('[aria-current=page]')?.dataset.document === arguments[0] && !!document.querySelector('.cm-content[contenteditable=true]')", document_id))
            require(checkpoint() == saved, "returning from read-only document changed the draft")
            owner = js("const done=arguments[arguments.length-1]; window.__TAURI_INTERNALS__.invoke('m0_renderer_session').then(done)", asynchronous=True)
            js("document.querySelector('#isolation-open').click()")
            handles = wait(lambda: (items if len(items := wd("GET", f"/session/{session_id}/window/handles")) == 2 else None))
            initial = wd("GET", f"/session/{session_id}/window")
            probe_handle = next(handle for handle in handles if handle != initial)
            wd("POST", f"/session/{session_id}/window", {"handle": probe_handle})
            wait(lambda: js("return !!document.querySelector('#result') && document.querySelector('#result').textContent.startsWith('{')"))
            rejected = native_result("workspace_inventory", {"request": {"renderer_session_id": owner, "after_id": None}})
            require(not rejected["ok"] and any(token in rejected.get("failure", "").lower() for token in ("not allowed", "denied", "invalid_caller")), "untrusted WebView accessed workspace IPC")
            wd("POST", f"/session/{session_id}/window", {"handle": initial})
            if args.measure:
                pid = wait(lambda: client_pid(driver.pid, client_binary))
                report = {
                    "profile": "m2-real-workspace-full-checkpoint-v1",
                    "complete": False,
                    "environment": {"os_release": platform.release(), "architecture": platform.machine(), "logical_cpus": os.cpu_count()},
                    "scope": {
                        "ui": "WebKitGTK on Xvfb; synthetic CodeMirror transactions, not physical keyboard or IME latency",
                        "storage": "real paired server and local SQLite WAL with synchronous FULL; durable generation first observed by a separate read-only SQLite connection",
                        "write_bytes": "Linux app process write_bytes counter; not total device writes or renderer process writes",
                        "memory": "sampled app process RSS, excluding WebKit subprocesses; host availability and swap are system-wide",
                        "display": "two requestAnimationFrame callbacks; not GPU or physical display scanout",
                        "checkpoint_count": "observed generation changes are a lower bound; multiple commits between 25 ms polls may be coalesced",
                        "queue": "input schedule lateness and unobserved generation count are separate observations; neither is an internal SQLite queue length",
                        "network": "online server traffic and concurrent synchronization may affect timings; network command count is not measured",
                        "timing": "input-to-durable includes batching, SQLite observation latency and wall-clock timestamping; it is not a checkpoint IPC or fsync duration",
                        "limitations": "SQLite polling adds measurement overhead; process observations are not power-loss, physical IME, IPC, direct fsync or device write measurements",
                    },
                    "profile_limits": {
                        "checkpoint_completion_p95_ms": {"102400": 50, "5242880": 200},
                        "checkpoint_ipc_and_fsync_directly_measured": False,
                        "typing_app_write_mib_per_second": {"102400": 2, "5242880": 50},
                        "typing_frame_p95_ms": {"102400": 50, "5242880": 100},
                        "switch_p95_ms": {"102400": 100, "5242880": 400},
                        "max_checkpoint_queue_batches": 1,
                        "checkpoint_queue_batches_directly_measured": False,
                        "minimum_available_memory_kib": 3 * 1024 * 1024,
                        "maximum_swap_growth_kib_exclusive": 256 * 1024,
                        "typing_checkpoints": 40,
                    },
                    "results": [],
                }
                destination = Path(args.measure_output)
                destination.parent.mkdir(parents=True, exist_ok=True)

                def open_measurement(identifier):
                    js("document.querySelector('[data-document=\"' + arguments[0] + '\"]').click()", identifier)
                    wait(lambda: js("return document.querySelector('[aria-current=page]')?.dataset.document === arguments[0] && !!document.querySelector('.cm-content[contenteditable=true]')", identifier), seconds=45)

                if args.paste_probe:
                    report = {
                        "profile": "m2-paste-save-state-probe-v1", "complete": False,
                        "scope": "Xvfb synthetic 5 MiB paste; state sampled in WebView and durable generation read immediately afterward; not a physical keyboard or fsync timing test",
                        "samples": [],
                    }
                    node = performance_nodes[(5 * 1024 * 1024, "A")]
                    open_measurement(node)
                    try:
                        for index in range(10):
                            before_generation = durable_generation(node)
                            sample = js("""
                                const [index, done] = arguments;
                                const status = () => {
                                  const text=document.querySelector('#save-status')?.textContent;
                                  return text==='이 기기에 저장됨'?'saved':text==='이 기기에 저장 중'?'saving':text==='저장 대기 중'?'waiting':'other';
                                };
                                const events=[];
                                const observer=new MutationObserver(()=>events.push({at:Date.now(),state:status()}));
                                observer.observe(document.querySelector('#save-status'),{subtree:true,childList:true,characterData:true});
                                const frame=()=>new Promise(resolve=>requestAnimationFrame(()=>requestAnimationFrame(resolve)));
                                const sleep=ms=>new Promise(resolve=>setTimeout(resolve,ms));
                                (async()=>{
                                  const before=status(), started=Date.now();
                                  const view=document.querySelector('.cm-content').cmTile.root.view;
                                  const line=(index%2?'p':'q').repeat(63)+'\\n';
                                  const size=5*1024*1024;
                                  const text=line.repeat(Math.floor(size/64))+'x'.repeat(size%64);
                                  view.dispatch({changes:{from:0,to:view.state.doc.length,insert:text}});
                                  await frame();
                                  const beforeClick=status();
                                  document.querySelector('#save').click();
                                  const limit=Date.now()+45000;
                                  while(status()!=='saved') { if(Date.now()>limit) throw new Error('timeout'); await sleep(20); }
                                  const saved=Date.now();
                                  observer.disconnect();
                                  done({before,beforeClick,savedAfterMs:saved-started,events:events.map(e=>({afterMs:e.at-started,state:e.state}))});
                                })().catch(()=>{observer.disconnect();done({probeFailure:true});});
                            """, index, asynchronous=True)
                            require(isinstance(sample, dict) and not sample.get("probeFailure"), "paste state probe did not complete")
                            at_ui_saved = durable_generation(node)
                            expected = before_generation + 1
                            sample.update({"generation_before": before_generation, "generation_at_ui_saved_check": at_ui_saved, "expected_generation": expected})
                            report["samples"].append(sample)
                            wait(lambda: durable_generation(node) >= expected, seconds=45)
                        report["complete"] = True
                    finally:
                        destination.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
                    print("Paste save-state probe completed using synthetic data.")
                    return

                try:
                    for size in (100 * 1024, 5 * 1024 * 1024):
                        node_a, node_b = performance_nodes[(size, "A")], performance_nodes[(size, "B")]
                        open_measurement(node_a)
                        for phase, count in (("typing", 40), ("paste", 10), ("switch", 20)):
                            if phase != "switch":
                                open_measurement(node_a)
                            initial_checkpoint = checkpoint(node_a)
                            initial_generation = initial_checkpoint[0] if initial_checkpoint else 0
                            before = time.monotonic()
                            try:
                                with ResourceMeter(pid) as resources, CheckpointMeter(client_data / "drafts.sqlite3", (node_a, node_b)) as checkpoints:
                                    measured = js("""
                                    const [size, phase, count, nodeA, nodeB, startGeneration, done] = arguments;
                                    const frame = () => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve)));
                                    const sleep = ms => new Promise(resolve => setTimeout(resolve, Math.max(0,ms)));
                                    const input = [], inputWall = [], uiSaved = [], frames = [], lateness = [], switches = [];
                                    const deadline = async (test) => {
                                      const end = performance.now()+45000;
                                      while (!test()) { if (performance.now()>end) throw new Error('measurement_timeout'); await sleep(20); }
                                    };
                                    (async () => {
                                      let active=nodeA;
                                      const start=performance.now();
                                      for (let i=0;i<count;i++) {
                                        if (phase==='typing') await sleep(start+i*250-performance.now());
                                        const view=document.querySelector('.cm-content').cmTile.root.view;
                                        let changes;
                                        if (phase==='paste') {
                                          const line=(i%2?'p':'q').repeat(63)+'\\n';
                                          const text=line.repeat(Math.floor(size/64))+'x'.repeat(size%64);
                                          changes={from:0,to:view.state.doc.length,insert:text};
                                        } else changes={from:0,to:1,insert:(phase==='switch' ? Math.floor(i/2)%2 : i%2)?'a':'b'};
                                        const at=performance.now();
                                        input.push(at);
                                        inputWall.push(Date.now());
                                        if (phase==='typing') lateness.push(Math.max(0,at-(start+i*250)));
                                        view.dispatch({changes});
                                        if (phase==='switch') {
                                          const target=active===nodeA?nodeB:nodeA;
                                          document.querySelector('[data-document="'+target+'"]').click();
                                          await deadline(()=>document.querySelector('[aria-current=page]')?.dataset.document===target && !!document.querySelector('.cm-content[contenteditable=true]'));
                                          await frame(); switches.push(performance.now()-at); active=target;
                                        } else {
                                          await frame(); frames.push(performance.now()-at);
                                          if (phase==='paste') {
                                            document.querySelector('#save').click();
                                            await deadline(()=>document.querySelector('#save-status')?.textContent==='이 기기에 저장됨');
                                            uiSaved.push(Date.now());
                                          }
                                        }
                                      }
                                      if (phase!=='switch') {
                                        document.querySelector('#save').click();
                                        await deadline(()=>document.querySelector('#save-status')?.textContent==='이 기기에 저장됨');
                                        if (phase==='typing') uiSaved.push(Date.now());
                                      }
                                      done({input,inputWall,uiSaved,frames,lateness,switches});
                                    })().catch(error=>done({
                                      measurement_failure: error?.message==='measurement_timeout' ? 'timeout' : 'javascript',
                                      completed_inputs: input.length,
                                      completed_frames: frames.length,
                                    }));
                                    """, size, phase, count, node_a, node_b, initial_generation, asynchronous=True)
                            except TestFailure:
                                report["failure"] = {"phase": phase, "document_bytes": size, "kind": "webdriver"}
                                raise
                            elapsed = time.monotonic() - before
                            if isinstance(measured, dict) and "measurement_failure" in measured:
                                report["failure"] = {
                                    "phase": phase, "document_bytes": size, "kind": measured["measurement_failure"],
                                    "completed_inputs": measured["completed_inputs"],
                                    "completed_frames": measured["completed_frames"],
                                }
                                raise TestFailure("performance phase did not complete")
                            require(isinstance(measured, dict), "performance phase produced an invalid result")
                            resource = resources.result(elapsed)
                            events = checkpoints.events
                            require(bool(events), "performance phase produced no observed durable generations")
                            item = {
                                "phase": phase, "document_bytes": size, "input_samples": count,
                                "elapsed_seconds": elapsed,
                                "checkpoint_ipc_roundtrip": None,
                                "observed_durable_generation_changes_lower_bound": len(events),
                                "observed_checkpoint_payload_bytes_lower_bound": size * len(events),
                                "logical_edit_bytes": size * count if phase == "paste" else count,
                                "network_commands": None,
                                "resources": resource,
                            }
                            if phase == "switch":
                                item["switch_flush_open_two_frames"] = stats(measured["switches"])
                            else:
                                expected = initial_generation + count
                                durable = wait(lambda: (row if (row := checkpoint(node_a)) and row[0] >= expected else None), seconds=45)
                                require(durable[0] >= expected, "final measured generation was not durable")
                                durable_delay, pending_generations = [], []
                                for index, timestamp in enumerate(measured["inputWall"]):
                                    completions = [event["observed_at"] for event in events if event["document"] == node_a and event["generation"] >= initial_generation + index + 1 and event["observed_at"] >= timestamp]
                                    require(bool(completions), "input generation has no later observed durable checkpoint")
                                    durable_delay.append(min(completions) - timestamp)
                                    completed_before_input = [event["generation"] for event in events if event["document"] == node_a and event["observed_at"] <= timestamp]
                                    last_completed = max(completed_before_input, default=initial_generation)
                                    pending_generations.append(max(0, initial_generation + index - last_completed))
                                item["input_to_observed_durable"] = stats(durable_delay)
                                item["max_previous_unobserved_generations_at_input"] = max(pending_generations)
                                item["editor_transaction_two_frames"] = stats(measured["frames"])
                                if phase == "paste":
                                    require(len(measured["uiSaved"]) == count, "UI save completion count did not match paste count")
                                    item["input_to_ui_saved"] = stats([saved_at - input_at for saved_at, input_at in zip(measured["uiSaved"], measured["inputWall"])])
                                else:
                                    item["last_input_to_ui_saved_ms"] = measured["uiSaved"][0] - measured["inputWall"][-1]
                                if phase == "typing":
                                    item["input_schedule_lateness"] = stats(measured["lateness"])
                            if resource["host_min_available_kib"] < 3 * 1024 * 1024 or resource["host_swap_growth_kib"] >= 256 * 1024:
                                item["resource_limit_exceeded"] = True
                                report["results"].append(item)
                                raise TestFailure("performance run stopped at the host memory or swap limit")
                            report["results"].append(item)
                    report["complete"] = True
                finally:
                    destination.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
                open_measurement(document_id)
            stop(server)
            server = None
            offline = "오프라인 편집\n"
            js("const view=document.querySelector('.cm-content').cmTile.root.view; view.dispatch({changes:{from:view.state.doc.length,insert:arguments[0]}})", offline)
            offline_saved = wait(lambda: (row if (row := checkpoint()) and offline.encode() in row[1] else None))
            require(offline_saved[0] > saved[0] and offline_saved[2] == synced_bytes, "offline checkpoint or base was not preserved")
            failed = native_result("workspace_inventory", {"request": {"renderer_session_id": owner, "after_id": None}})
            require(not failed["ok"] and checkpoint() == offline_saved, "network failure changed the local draft")
            require((notes / "Test Folder" / "Example.md").read_bytes() == synced_bytes, "offline edits unexpectedly changed the server file")
            wd("DELETE", f"/session/{session_id}")
            session_id = None

            def database_unlocked():
                try:
                    with (client_data / "drafts.sqlite3").open("rb") as database:
                        fcntl.flock(database, fcntl.LOCK_EX | fcntl.LOCK_NB)
                        fcntl.flock(database, fcntl.LOCK_UN)
                    return True
                except BlockingIOError:
                    return False

            wait(database_unlocked)
            session_id = wd("POST", "/session", capabilities)["sessionId"]
            wd("POST", f"/session/{session_id}/timeouts", {"script": 30000})
            wait(lambda: js("return !!document.querySelector('[data-draft-document=\"' + arguments[0] + '\"]:not(:disabled)')", document_id))
            new_owner = js("const done=arguments[arguments.length-1]; window.__TAURI_INTERNALS__.invoke('m0_renderer_session').then(done)", asynchronous=True)
            disconnected = native_result("workspace_inventory", {"request": {"renderer_session_id": new_owner, "after_id": None}})
            require(not disconnected["ok"] and disconnected.get("failure") == "not_connected", "authentication unexpectedly survived the process restart")
            js("document.querySelector('[data-draft-document=\"' + arguments[0] + '\"]').click()", document_id)
            wait(lambda: js("return document.querySelector('[data-draft-document=\"' + arguments[0] + '\"]')?.getAttribute('aria-current') === 'page' && !!document.querySelector('.cm-content[contenteditable=true]')", document_id))
            recovered = js("return document.querySelector('.cm-content').cmTile.root.view.state.doc.toString()")
            require(offline in recovered and checkpoint() == offline_saved, "offline restart did not recover the saved draft")
            resumed = "재시작 후 편집\n"
            js("const view=document.querySelector('.cm-content').cmTile.root.view; view.dispatch({changes:{from:view.state.doc.length,insert:arguments[0]}})", resumed)
            final_saved = wait(lambda: (row if (row := checkpoint()) and resumed.encode() in row[1] else None))
            require(final_saved[0] > offline_saved[0] and final_saved[2] == synced_bytes, "recovered draft was not saved locally")
            if args.recovery_dialogs:
                print("Recovery verification passed: copied source, dialog cancellation, new file bytes and existing file preservation.")
            print("M2 workspace WebView integration passed: synthetic pairing, image preview, server acknowledgement, reload, oversized read-only source, IPC isolation, offline draft and restart recovery.")
        finally:
            if session_id:
                try:
                    wd("DELETE", f"/session/{session_id}")
                except Exception:
                    pass
            stop(driver)
            stop(server)
            if remote_probe:
                remote_probe.shutdown()
                remote_probe.server_close()
            if remote_thread:
                remote_thread.join(timeout=5)


if __name__ == "__main__":
    try:
        main()
    except TestFailure as failure:
        print(f"M2 workspace WebView integration failed: {failure}")
        raise SystemExit(1) from None
    except Exception as failure:
        print(f"M2 workspace WebView integration failed unexpectedly: {type(failure).__name__}")
        raise SystemExit(1) from None
