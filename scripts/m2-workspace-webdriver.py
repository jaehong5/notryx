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
import re
import select
import signal
import socket
import sqlite3
import stat
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


def leader_running(process):
    if process.returncode is not None:
        return False
    try:
        return os.waitid(os.P_PID, process.pid, os.WEXITED | os.WNOHANG | os.WNOWAIT) is None
    except ChildProcessError:
        return False


def stop(process):
    if not process or process.returncode is not None:
        return
    try:
        os.waitid(os.P_PID, process.pid, os.WEXITED | os.WNOHANG | os.WNOWAIT)
    except ChildProcessError:
        return
    try:
        os.killpg(process.pid, signal.SIGTERM)
    except ProcessLookupError:
        pass
    deadline = time.monotonic() + 10
    while leader_running(process) and time.monotonic() < deadline:
        time.sleep(0.05)
    try:
        os.killpg(process.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
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


def blur_open_race_window():
    require(xvfb_on_current_display(), "open race requires an isolated Xvfb display")
    xlib = ctypes.CDLL(ctypes.util.find_library("X11"))
    xlib.XOpenDisplay.argtypes = [ctypes.c_char_p]
    xlib.XOpenDisplay.restype = ctypes.c_void_p
    xlib.XDefaultRootWindow.argtypes = [ctypes.c_void_p]
    xlib.XDefaultRootWindow.restype = ctypes.c_ulong
    xlib.XGetInputFocus.argtypes = [ctypes.c_void_p, ctypes.POINTER(ctypes.c_ulong), ctypes.POINTER(ctypes.c_int)]
    xlib.XSetInputFocus.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.c_int, ctypes.c_ulong]
    xlib.XSync.argtypes = [ctypes.c_void_p, ctypes.c_int]
    xlib.XCloseDisplay.argtypes = [ctypes.c_void_p]
    display = xlib.XOpenDisplay(None)
    require(bool(display), "isolated display could not be opened")
    previous = ctypes.c_ulong()
    revert = ctypes.c_int()
    xlib.XGetInputFocus(display, ctypes.byref(previous), ctypes.byref(revert))
    xlib.XSetInputFocus(display, xlib.XDefaultRootWindow(display), 1, 0)
    xlib.XSync(display, 0)
    return xlib, display, previous.value, revert.value


def restore_open_race_focus(focus):
    if focus is None:
        return
    xlib, display, previous, revert = focus
    try:
        xlib.XSetInputFocus(display, previous, revert, 0)
        xlib.XSync(display, 0)
    finally:
        xlib.XCloseDisplay(display)


def capture_remote_change_focus(exclude_window=None):
    require(xvfb_on_current_display(), "remote change focus requires an isolated Xvfb display")
    xlib = ctypes.CDLL(ctypes.util.find_library("X11"))
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
    xlib.XGetInputFocus.argtypes = [display_type, ctypes.POINTER(window_type), ctypes.POINTER(ctypes.c_int)]
    xlib.XSetInputFocus.argtypes = [display_type, window_type, ctypes.c_int, ctypes.c_ulong]
    xlib.XRaiseWindow.argtypes = [display_type, window_type]
    xlib.XSync.argtypes = [display_type, ctypes.c_int]
    xlib.XCloseDisplay.argtypes = [display_type]
    display = xlib.XOpenDisplay(None)
    require(bool(display), "isolated display could not be opened")
    try:
        root = xlib.XDefaultRootWindow(display)
        name_atom = xlib.XInternAtom(display, b"_NET_WM_NAME", 0)
        root_return, parent = window_type(), window_type()
        children = ctypes.POINTER(window_type)()
        count = ctypes.c_uint()
        require(bool(xlib.XQueryTree(display, root, ctypes.byref(root_return), ctypes.byref(parent), ctypes.byref(children), ctypes.byref(count))), "receiver window enumeration failed")
        matches = []
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
                        if child != exclude_window and title == "Notryx" and xlib.XGetWindowAttributes(display, child, ctypes.byref(attributes)) and attributes.map_state == 2:
                            matches.append(child)
                finally:
                    if data:
                        xlib.XFree(data)
        finally:
            if children:
                xlib.XFree(children)
        require(len(matches) == 1, "target Notryx window is missing or ambiguous")
        window = matches[0]
        xlib.XRaiseWindow(display, window)
        xlib.XSetInputFocus(display, window, 2, 0)
        xlib.XSync(display, 0)
        focused, revert = window_type(), ctypes.c_int()
        xlib.XGetInputFocus(display, ctypes.byref(focused), ctypes.byref(revert))
        require(focused.value == window, "target X11 window focus failed")
        return xlib, display, window, 2
    except BaseException:
        xlib.XCloseDisplay(display)
        raise


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
    parser.add_argument("--boundary-25m", action="store_true")
    parser.add_argument("--verify-5m", action="store_true")
    parser.add_argument("--unscoped-recovery", action="store_true")
    parser.add_argument("--resync-race", action="store_true")
    parser.add_argument("--open-race", action="store_true")
    parser.add_argument("--open-race-delete", action="store_true")
    parser.add_argument("--remote-change", choices=("clean", "dirty"))
    args = parser.parse_args()
    require(not (args.measure and args.recovery_dialogs), "performance and dialog verification must run separately")
    require(not args.paste_probe or args.measure, "paste probe requires the release measurement setup")
    require(not args.boundary_25m or not (args.measure or args.recovery_dialogs), "25 MiB boundary verification must run separately")
    require(not args.verify_5m or not (args.measure or args.recovery_dialogs or args.boundary_25m), "5 MiB verification must run separately")
    require(not args.unscoped_recovery or not (args.measure or args.recovery_dialogs or args.boundary_25m or args.verify_5m), "unscoped recovery verification must run separately")
    require(not args.resync_race or not (args.measure or args.recovery_dialogs or args.boundary_25m or args.verify_5m or args.unscoped_recovery), "resync race verification must run separately")
    require(not args.open_race or not (args.measure or args.recovery_dialogs or args.boundary_25m or args.verify_5m or args.unscoped_recovery or args.resync_race), "open race verification must run separately")
    require(not args.open_race_delete or not (args.measure or args.recovery_dialogs or args.boundary_25m or args.verify_5m or args.unscoped_recovery or args.resync_race or args.open_race), "open deletion race verification must run separately")
    require(not args.remote_change or not (args.measure or args.recovery_dialogs or args.boundary_25m or args.verify_5m or args.unscoped_recovery or args.resync_race or args.open_race or args.open_race_delete), "remote change verification must run separately")
    server_binary = str(Path(args.server).resolve())
    client_binary = str(Path(args.binary).resolve())
    if args.measure:
        require(Path(client_binary) == Path("target/release/notryx-client").resolve(),
                "performance measurement requires the release client binary")
        require(xvfb_on_current_display(), "performance measurement requires a local Xvfb display")
    if args.recovery_dialogs:
        require(xvfb_on_current_display(), "recovery dialog verification requires an isolated Xvfb display")
    if args.remote_change:
        test_bus_prefix = os.environ.get("NOTRYX_TEST_SECRET_BUS_PREFIX", "")
        test_bus_address = os.environ.get("DBUS_SESSION_BUS_ADDRESS", "")
        test_bus_directory = Path(test_bus_prefix)
        require(
            test_bus_directory.parent == Path("/tmp")
            and re.fullmatch(r"notryx-secret-test\.[A-Za-z0-9]{8}", test_bus_directory.name) is not None
            and not test_bus_directory.is_symlink()
            and test_bus_directory.is_dir()
            and test_bus_directory.stat().st_uid == os.getuid()
            and stat.S_IMODE(test_bus_directory.stat().st_mode) == 0o700
            and not (test_bus_directory / "bus.sock").is_symlink()
            and (test_bus_directory / "bus.sock").is_socket()
            and re.fullmatch(r"unix:path=" + re.escape(str(test_bus_directory / "bus.sock")) + r"(?:,guid=[0-9a-fA-F]{32})?", test_bus_address) is not None
            and not os.environ.get("DBUS_STARTER_ADDRESS")
            and not os.environ.get("DBUS_STARTER_BUS_TYPE"),
            "remote change verification requires scripts/m3-secret-no-service.sh with an isolated session bus",
        )
        require(xvfb_on_current_display(), "remote change verification requires an isolated Xvfb display")
    server = driver = peer_driver = remote_probe = remote_thread = resync_proxy = None
    session_id = None
    peer_session = None
    peer_wd = None
    race_focus = None
    remote_focus = None

    with tempfile.TemporaryDirectory(prefix="notryx-m2-webdriver-") as temporary:
        root = Path(temporary)
        notes, state, run, client_data, xdg_data, xdg_cache, xdg_config = (
            root / name for name in ("notes", "state", "run", "client", "xdg-data", "xdg-cache", "xdg-config")
        )
        for directory in (notes, state, run, client_data, xdg_data, xdg_cache, xdg_config):
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
            connection = http.client.HTTPConnection("127.0.0.1", port, timeout=90 if args.boundary_25m or args.verify_5m else 20)
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

        def wait(check, seconds=20, description="expected application state"):
            deadline = time.monotonic() + seconds
            while time.monotonic() < deadline:
                result = check()
                if result:
                    return result
                time.sleep(0.1)
            raise TestFailure(f"timed out waiting for {description}")

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
            require(bool(ready) and server.stdout.readline().strip() == "server_started" and leader_running(server),
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
            original = ("\ufeff# Synthetic\r\n\r\nOriginal source.\r\n\r\n" +
                        "![local](pixel.png)\r\n" * 17 +
                        "[same](#synthetic) [other](Target.md#destination)\r\n" +
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
            target_id = operation_id()
            target_source = b"# Target\n\n## Destination\n"
            target_digest = base64.b64encode(hashlib.sha256(target_source).digest()).decode()
            target_upload = server_request("POST", "/v1/uploads", data=target_source, expected=201, headers={
                **authorization, "Content-Type": "application/octet-stream", "Content-Digest": f"sha-256=:{target_digest}:",
            })
            create(target_id, folder_id, "Target.md", "file", 5, target_upload["upload_id"])
            boundary_id = None
            if args.boundary_25m:
                boundary_id = operation_id()
                boundary_size = 25 * 1024 * 1024
                boundary_line = b"r" * 63 + b"\n"
                boundary_body_size = boundary_size - 3
                boundary_body = (boundary_line * (boundary_body_size // len(boundary_line) + 1))[:boundary_body_size]
                boundary_source = b"\xef\xbb\xbf" + boundary_body
                require(len(boundary_source) == boundary_size, "25 MiB fixture has the wrong byte length")
                boundary_hash = hashlib.sha256(boundary_source).digest()
                boundary_upload = server_request("POST", "/v1/uploads", data=boundary_source, expected=201, headers={
                    **authorization, "Content-Type": "application/octet-stream", "Content-Digest": "sha-256=:" + base64.b64encode(boundary_hash).decode() + ":",
                })
                create(boundary_id, folder_id, "Boundary 25 MiB.md", "file", 6, boundary_upload["upload_id"])
                boundary_prefix = boundary_body[:63].decode()
                boundary_last_line = boundary_body.rsplit(b"\n", 1)[-1].decode()
                del boundary_source, boundary_body
                boundary_draft_id = operation_id()
                boundary_draft_source = b"# Synthetic draft base\n"
                boundary_draft_digest = base64.b64encode(hashlib.sha256(boundary_draft_source).digest()).decode()
                boundary_draft_upload = server_request("POST", "/v1/uploads", data=boundary_draft_source, expected=201, headers={
                    **authorization, "Content-Type": "application/octet-stream", "Content-Digest": f"sha-256=:{boundary_draft_digest}:",
                })
                create(boundary_draft_id, folder_id, "Boundary Draft.md", "file", 7, boundary_draft_upload["upload_id"])
            verify_5m_id = None
            if args.verify_5m:
                verify_5m_id = operation_id()
                verify_5m_size = 5 * 1024 * 1024
                verify_5m_prefix_text = "# 한글 문서😀\n"
                verify_5m_prefix = b"\xef\xbb\xbf" + verify_5m_prefix_text.encode("utf-8")
                verify_5m_line = b"x" * 63 + b"\n"
                verify_5m_body_size = verify_5m_size - len(verify_5m_prefix) - 1
                verify_5m_body = (verify_5m_line * (verify_5m_body_size // len(verify_5m_line) + 1))[:verify_5m_body_size]
                verify_5m_source = verify_5m_prefix + verify_5m_body
                verify_5m_expected = verify_5m_source + b"!"
                require(len(verify_5m_source) == verify_5m_size - 1 and len(verify_5m_expected) == verify_5m_size, "5 MiB fixture has the wrong byte length")
                verify_5m_hash = hashlib.sha256(verify_5m_expected).digest()
                upload_digest = base64.b64encode(hashlib.sha256(verify_5m_source).digest()).decode()
                verify_5m_upload = server_request("POST", "/v1/uploads", data=verify_5m_source, expected=201, headers={
                    **authorization, "Content-Type": "application/octet-stream", "Content-Digest": f"sha-256=:{upload_digest}:",
                })
                create(verify_5m_id, folder_id, "Boundary 5 MiB.md", "file", 6, verify_5m_upload["upload_id"])
            performance_nodes = {}
            if args.measure:
                sequence = 5
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
            if args.resync_race:
                from m3_resync_proxy import ResyncProxy
                resync_proxy = ResyncProxy(port, document_id)
                resync_proxy.start()
            if args.open_race or args.open_race_delete:
                from m3_open_proxy import OpenProxy
                resync_proxy = OpenProxy(port, document_id)
                resync_proxy.start()
            ui_grant = admin("issue-pairing", admin_status["owner_principal_id"])
            driver_port = free_port()
            driver_origin = f"http://127.0.0.1:{driver_port}"
            local_opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
            environment = os.environ.copy()
            environment.update(
                TAURI_WEBVIEW_AUTOMATION="true", NOTRYX_M0_DATA_DIR=str(client_data),
                XDG_DATA_HOME=str(xdg_data), XDG_CACHE_HOME=str(xdg_cache), XDG_CONFIG_HOME=str(xdg_config),
            )
            if args.recovery_dialogs:
                environment["GTK_USE_PORTAL"] = "0"
            driver = subprocess.Popen(
                [args.driver, f"--port={driver_port}", "--host=127.0.0.1"], env=environment,
                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True,
            )
            test_stage = "webdriver startup"

            def wd(method, path, payload=None):
                data = None if payload is None else json.dumps(payload).encode()
                request = urllib.request.Request(driver_origin + path, data=data, method=method,
                                                 headers={"Content-Type": "application/json"})
                try:
                    with local_opener.open(request, timeout=180 if args.measure or args.boundary_25m or args.verify_5m else 35) as response:
                        result = json.load(response)
                    value = result["value"]
                    if isinstance(value, dict) and "error" in value:
                        code = value["error"]
                        safe_codes = {"script timeout", "timeout", "javascript error", "no such window", "invalid session id", "unknown error"}
                        raise TestFailure(f"WebDriver error: {code if code in safe_codes else 'other'}")
                    return value
                except (OSError, ValueError, KeyError, urllib.error.HTTPError) as error:
                    operation = "execute" if "/execute/" in path else "session" if "/session" in path else "status"
                    status = error.code if isinstance(error, urllib.error.HTTPError) else None
                    code = "unknown"
                    category = "unavailable"
                    if isinstance(error, urllib.error.HTTPError):
                        try:
                            value = json.loads(error.read(16384)).get("value", {})
                            known = {"script timeout", "timeout", "javascript error", "no such window", "invalid session id", "unknown error"}
                            if value.get("error") in known:
                                code = value["error"]
                            message = str(value.get("message", "")).lower()
                            if "null is not an object" in message or "cannot read properties of null" in message:
                                category = "missing element"
                            elif "not a function" in message:
                                category = "missing function"
                            elif "timeout" in message:
                                category = "timeout"
                            else:
                                category = "other"
                        except (OSError, ValueError, AttributeError):
                            pass
                    raise TestFailure(f"WebDriver request failed: {test_stage}, {operation}, {type(error).__name__}, HTTP {status}, {code}, {category}") from None

            wait(lambda: leader_running(driver) and driver_owns_port(driver.pid, driver_port))

            def driver_ready():
                try:
                    return wd("GET", "/status")
                except TestFailure:
                    return False

            wait(driver_ready)
            capabilities = {"capabilities": {"alwaysMatch": {"browserName": "wry", "webkitgtk:browserOptions": {"binary": client_binary}}}}
            session_id = wd("POST", "/session", capabilities)["sessionId"]
            wd("POST", f"/session/{session_id}/timeouts", {"script": 150000 if args.measure or args.boundary_25m or args.verify_5m else 30000})

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

            def acknowledged(generation, node_id=document_id):
                with sqlite3.connect(f"file:{client_data / 'drafts.sqlite3'}?mode=ro", uri=True) as db:
                    row = db.execute("""
                        SELECT r.device_stream_id, o.operation_seq, s.ack_through
                        FROM client_registrations r
                        JOIN client_outbox o ON o.registration_id = r.id
                        JOIN outbox_streams s ON s.registration_id = r.id
                        WHERE r.status = 'active' AND o.document_id = ? AND o.generation = ?
                          AND o.status = 'committed' AND o.terminal_kind IS NULL
                        ORDER BY o.operation_seq DESC LIMIT 1
                    """, (node_id, generation)).fetchone()
                if row is None or row[2] < row[1]:
                    return False
                with sqlite3.connect(f"file:{state / 'state.db'}?mode=ro", uri=True) as db:
                    watermark = db.execute("SELECT last_seq, retired_through FROM operation_streams WHERE stream_id = ?", (row[0],)).fetchone()
                return watermark is not None and int(watermark[0]) == row[1] and int(watermark[1]) >= row[1]

            def operation_count(node_id=document_id):
                with sqlite3.connect(f"file:{client_data / 'drafts.sqlite3'}?mode=ro", uri=True) as db:
                    return db.execute("""
                        SELECT COUNT(*) FROM client_outbox o
                        JOIN client_registrations r ON r.id = o.registration_id
                        WHERE r.status = 'active' AND o.document_id = ?
                    """, (node_id,)).fetchone()[0]

            def outbox_snapshot():
                with sqlite3.connect(f"file:{client_data / 'drafts.sqlite3'}?mode=ro", uri=True) as db:
                    return db.execute("SELECT * FROM client_outbox ORDER BY operation_id").fetchall()

            test_stage = "initial screen"
            if not (args.open_race or args.open_race_delete):
                try:
                    wait(lambda: js("return !!document.querySelector('.cm-content[contenteditable=true]')"), description="initial editor")
                except TestFailure:
                    diagnostic = js("""
                        const done=arguments[arguments.length-1];
                        const result={ready:document.readyState, rootChildren:document.getElementById('root')?.childElementCount ?? -1,
                          editor:!!document.querySelector('.cm-content'), pairing:!!document.querySelector('.workspace-connect'),
                          startupFailure:!![...document.querySelectorAll('h1')].find(el=>el.textContent.includes('시작 상태 확인 실패')),
                          unavailable:!![...document.querySelectorAll('h1')].find(el=>el.textContent.includes('로컬 저장소를 열 수 없습니다')),
                          recovery:!!document.querySelector('[data-migration-recovery]'), startupMode:null};
                        if (!window.__TAURI_INTERNALS__) { done(result); return; }
                        window.__TAURI_INTERNALS__.invoke('client_startup_status')
                          .then(value=>{result.startupMode=value.mode; done(result);}).catch(()=>done(result));
                    """, asynchronous=True)
                    print("Initial UI diagnostic:", json.dumps(diagnostic, ensure_ascii=False), flush=True)
                    raise
            wait(lambda: js("const button=document.querySelector('.workspace-connect button'); return !!button && !button.disabled"), description="pairing form")
            test_stage = "pairing submit"
            js("""
                const form = document.querySelector('.workspace-connect');
                form.elements.namedItem('origin').value = arguments[0];
                form.elements.namedItem('workspace_id').value = arguments[1];
                form.elements.namedItem('pairing_secret').value = arguments[2];
                form.requestSubmit();
            """, f"http://127.0.0.1:{resync_proxy.server_address[1] if resync_proxy else port}", ui_grant["workspace_id"], ui_grant["pairing_secret"])
            test_stage = "paired list"
            wait(lambda: js("return !!document.querySelector('[data-document=\"' + arguments[0] + '\"]:not(:disabled)')", document_id), description="paired document list")
            require(js("return document.querySelector('[name=pairing_secret]').value === ''"), "pairing secret remained in the form")
            require(js("return document.querySelector('[data-document=\"' + arguments[0] + '\"]').textContent", document_id) == "Test Folder/Example.md", "folder path was not shown")
            if args.open_race or args.open_race_delete:
                test_stage = "initial sync"
                owner = js("const done=arguments[arguments.length-1]; window.__TAURI_INTERNALS__.invoke('m0_renderer_session').then(done)", asynchronous=True)
                def sync_ready():
                    return js("""
                        const done=arguments[arguments.length-1];
                        window.__TAURI_INTERNALS__.invoke('workspace_sync_status', {request:{renderer_session_id:arguments[0]}})
                          .then(value=>done(value.status === 'ready')).catch(()=>done(false));
                    """, owner, asynchronous=True)
                try:
                    wait(sync_ready, seconds=45, description="open race initial sync")
                except TestFailure:
                    status = js("""
                        const done=arguments[arguments.length-1];
                        window.__TAURI_INTERNALS__.invoke('workspace_sync_status', {request:{renderer_session_id:arguments[0]}})
                          .then(value=>done(['connecting','ready','offline','auth_required','gateway_auth_required','needs_review'].includes(value.status) ? value.status : 'unknown'))
                          .catch(error=>{
                            const value=String(error);
                            const codes=['stale_renderer_session','stale_registration','not_connected','pairing_in_progress','invalid args','invalid value'];
                            done(codes.find(code=>value.includes(code)) ?? 'request_failed');
                          });
                    """, owner, asynchronous=True)
                    owner_shape = isinstance(owner, str) and len(owner) == 36
                    with sqlite3.connect(f"file:{client_data / 'drafts.sqlite3'}?mode=ro", uri=True) as db:
                        index_present = db.execute("SELECT EXISTS(SELECT 1 FROM remote_index)").fetchone()[0] == 1
                        active_registration = db.execute("SELECT EXISTS(SELECT 1 FROM client_registrations WHERE status='active')").fetchone()[0] == 1
                    with resync_proxy.lock:
                        print("Open sync diagnostic:", json.dumps({"status": status, "owner_shape": owner_shape, "index_present": index_present, "active_registration": active_registration, "routes": resync_proxy.route_counts, "responses": resync_proxy.response_counts, "proxy_failed": resync_proxy.failure.is_set()}), flush=True)
                    raise
                node = server_request("GET", f"/v1/nodes/{document_id}", headers=authorization)
                updated = b"# Synthetic updated source\n"
                race_focus = blur_open_race_window()
                time.sleep(1)
                resync_proxy.hold_get.set()
                resync_proxy.hold_events.set()
                test_stage = "open selected document"
                js("document.querySelector('[data-document=\"' + arguments[0] + '\"]').click()", document_id)
                require(resync_proxy.get_held.wait(20), "initial content GET was not held")
                time.sleep(1)
                with resync_proxy.lock:
                    require(resync_proxy.inflight.get("inventory", 0) == 0, "initial inventory request was still running")
                resync_proxy.capture_changes.set()
                if args.open_race_delete:
                    server_request("DELETE", f"/v1/nodes/{document_id}", headers={
                        **authorization, "If-Match": node["node_etag"], "X-Workspace-Epoch": info["workspace_epoch"],
                        "X-Device-Stream": pair["device_stream_id"], "X-Operation-Seq": "6", "Idempotency-Key": operation_id(),
                    })
                else:
                    server_request("PUT", f"/v1/nodes/{document_id}/content", data=updated, headers={
                        **authorization, "Content-Type": "application/octet-stream",
                        "Content-Digest": "sha-256=:" + base64.b64encode(hashlib.sha256(updated).digest()).decode() + ":",
                        "If-Match": node["content_etag"], "X-Workspace-Epoch": info["workspace_epoch"],
                        "X-Device-Stream": pair["device_stream_id"], "X-Operation-Seq": "6", "Idempotency-Key": operation_id(),
                    })
                resync_proxy.release_get.set()
                test_stage = "open race result"
                require(resync_proxy.changes_seen.wait(20), "stale content did not trigger metadata catch-up")
                if args.open_race_delete:
                    wait(lambda: js("return [...document.querySelectorAll('[role=alert]')].some(item => item.textContent.includes('삭제된 문서입니다'))"), seconds=45, description="deleted document notice")
                    wait(lambda: js("return !document.querySelector('[data-document=\"' + arguments[0] + '\"]')", document_id), seconds=45, description="deleted document list refresh")
                    with resync_proxy.lock:
                        require(resync_proxy.get_count == 1 and resync_proxy.get_statuses == [410], "deleted content GET did not return 410")
                        trace = list(resync_proxy.trace)
                    print("Open deletion proxy trace:", json.dumps(trace), flush=True)
                    require(trace.count("server-info:request") == 1 and trace.count("changes:request") == 1, "background catch-up overlapped the open request")
                    require("inventory:request" not in trace[:trace.index("changes:request")], "inventory refresh preceded the open catch-up")
                    require(not resync_proxy.failure.is_set(), "open test proxy failed")
                    require(checkpoint() is None, "deleted content created a draft")
                    resync_proxy.release_events.set()
                    print("M3 WebView deletion race passed: 410, tombstone catch-up and list refresh.", flush=True)
                    return
                wait(lambda: js("return document.querySelector('[aria-current=page]')?.dataset.document === arguments[0] && document.querySelector('.cm-content')?.cmTile.root.view.state.doc.toString() === arguments[1]", document_id, updated.decode()), seconds=45, description="updated document after one retry")
                with resync_proxy.lock:
                    require(resync_proxy.get_count == 2 and resync_proxy.get_statuses == [412, 200], "open did not retry one stale content GET")
                require(not resync_proxy.failure.is_set(), "open test proxy failed")
                with sqlite3.connect(f"file:{client_data / 'drafts.sqlite3'}?mode=ro", uri=True) as db:
                    row = db.execute("SELECT c.bytes, n.content_ready FROM cached_content c JOIN remote_nodes n ON n.registration_id=c.registration_id AND n.node_id=c.node_id JOIN client_registrations r ON r.id=c.registration_id WHERE r.status='active' AND c.node_id=?", (document_id,)).fetchone()
                require(row == (updated, 1), "retried content did not become the current cache")
                require(checkpoint() is None, "unmodified retried content created a draft")
                resync_proxy.release_events.set()
                print("M3 WebView open race passed: 412, one current revision retry and verified cache.", flush=True)
                return
            if args.resync_race:
                test_stage = "resync initial sync"
                owner = js("const done=arguments[arguments.length-1]; window.__TAURI_INTERNALS__.invoke('m0_renderer_session').then(done)", asynchronous=True)

                def sync_status():
                    return js("""
                        const done=arguments[arguments.length-1];
                        window.__TAURI_INTERNALS__.invoke('workspace_sync_status',
                          {request:{renderer_session_id:arguments[0]}})
                          .then(value=>done(value.status)).catch(()=>done(null));
                    """, owner, asynchronous=True)

                def sync_cursor():
                    with sqlite3.connect(f"file:{client_data / 'drafts.sqlite3'}?mode=ro", uri=True) as db:
                        row = db.execute("""
                            SELECT i.cursor FROM remote_index i JOIN client_registrations r ON r.id=i.registration_id
                            WHERE r.status='active'
                        """).fetchone()
                    return int(row[0]) if row else None

                wait(lambda: sync_cursor() is not None and sync_status() == "ready" and js("return !document.querySelector('#workspace-sync-status')"), seconds=45, description="resync initial ready")
            if args.remote_change:
                test_stage = "remote change pre-open sync"
                setup_owner = js("const done=arguments[arguments.length-1]; window.__TAURI_INTERNALS__.invoke('m0_renderer_session').then(done)", asynchronous=True)

                def pre_open_cursor(data):
                    with sqlite3.connect(f"file:{data / 'drafts.sqlite3'}?mode=ro", uri=True) as db:
                        row = db.execute("""
                            SELECT i.cursor FROM remote_index i JOIN client_registrations r ON r.id=i.registration_id
                            WHERE r.status='active'
                        """).fetchone()
                    return int(row[0]) if row else None

                sync_details_script = """
                    const [owner, nodeId, done] = arguments;
                    const statuses=['connecting','ready','offline','auth_required','gateway_auth_required','needs_review'];
                    const labels=[
                      ['open_network', '서버에 연결할 수 없습니다. 연결 상태를 확인하고 다시 열어 주세요.'],
                      ['open_deleted', '삭제된 문서입니다. 목록을 갱신해 주세요.'],
                      ['open_changed', '문서가 계속 변경되고 있습니다. 목록을 갱신한 뒤 다시 열어 주세요.'],
                      ['open_sync', '동기화 상태를 확인한 뒤 문서를 다시 열어 주세요.'],
                      ['open_other', '문서를 열지 못했습니다. 상태를 확인한 뒤 다시 시도해 주세요.'],
                      ['inventory_refresh', '문서 목록을 다시 불러와 주세요.'],
                      ['editor_closed', '편집기 화면이 닫혔습니다.'],
                      ['open_response', '잘못된 문서 응답입니다.'],
                      ['other_document', '다른 문서의 응답입니다.'],
                      ['composition', '한글 조합을 마친 뒤 문서를 전환해 주세요.'],
                    ];
                    const button=document.querySelector('[data-document="' + nodeId + '"]');
                    const editor=document.querySelector('.cm-content');
                    const alerts=[...document.querySelectorAll('[role=alert]')].map(el=>el.textContent ?? '');
                    const result={focused:document.hasFocus(), button_present:!!button,
                      button_disabled:!!button?.disabled, selected_target:button?.getAttribute('aria-current') === 'page',
                      editor_present:!!editor, editor_editable:editor?.getAttribute('contenteditable') === 'true',
                      alert_present:alerts.length > 0,
                      error_labels:labels.filter(([, message])=>alerts.some(alert=>alert.includes(message))).map(([label])=>label),
                      status:'unavailable'};
                    window.__TAURI_INTERNALS__.invoke('workspace_sync_status', {request:{renderer_session_id:owner}})
                      .then(value=>{result.status=statuses.includes(value.status) ? value.status : 'unknown'; done(result);})
                      .catch(()=>done(result));
                """

                def remote_pre_open_ready():
                    return pre_open_cursor(client_data) is not None and js(sync_details_script, setup_owner, document_id, asynchronous=True)["status"] == "ready"

                try:
                    wait(remote_pre_open_ready, seconds=45, description="remote change pre-open sync")
                except TestFailure:
                    try:
                        print("M3 receiver pre-open diagnostic:", json.dumps({
                            **js(sync_details_script, setup_owner, document_id, asynchronous=True),
                            "cursor": pre_open_cursor(client_data),
                        }), flush=True)
                    except Exception:
                        print("M3 receiver pre-open diagnostic unavailable.", flush=True)
                    raise
            test_stage = "open selected document"
            js("document.querySelector('[data-document=\"' + arguments[0] + '\"]').click()", document_id)
            try:
                wait(lambda: js("return document.querySelector('[aria-current=page]')?.dataset.document === arguments[0] && !!document.querySelector('.cm-content[contenteditable=true]')" , document_id), description="selected document editor")
            except TestFailure:
                if args.remote_change:
                    try:
                        diagnostic = js("""
                            const [nodeId, done] = arguments;
                            const button=document.querySelector('[data-document="' + nodeId + '"]');
                            const selected=document.querySelector('[aria-current=page]');
                            const editor=document.querySelector('.cm-content');
                            const alerts=[...document.querySelectorAll('[role=alert]')].map(el=>el.textContent ?? '');
                            const codes=['stale_renderer_session','stale_registration','not_connected','bootstrap_required',
                              'stale_sync','server_not_ready','history_regressed','resync_required','binding_mismatch',
                              'protocol_mismatch','document_not_found','network_error'];
                            const labels=[
                              ['open_network', '서버에 연결할 수 없습니다. 연결 상태를 확인하고 다시 열어 주세요.'],
                              ['open_deleted', '삭제된 문서입니다. 목록을 갱신해 주세요.'],
                              ['open_changed', '문서가 계속 변경되고 있습니다. 목록을 갱신한 뒤 다시 열어 주세요.'],
                              ['open_sync', '동기화 상태를 확인한 뒤 문서를 다시 열어 주세요.'],
                              ['open_other', '문서를 열지 못했습니다. 상태를 확인한 뒤 다시 시도해 주세요.'],
                              ['inventory_refresh', '문서 목록을 다시 불러와 주세요.'],
                              ['editor_closed', '편집기 화면이 닫혔습니다.'],
                              ['open_response', '잘못된 문서 응답입니다.'],
                              ['other_document', '다른 문서의 응답입니다.'],
                              ['composition', '한글 조합을 마친 뒤 문서를 전환해 주세요.'],
                            ];
                            const statuses=['connecting','ready','offline','auth_required','gateway_auth_required','needs_review'];
                            const result={button_present:!!button, button_disabled:!!button?.disabled,
                              selected_target:button?.getAttribute('aria-current') === 'page',
                              selected_other:!!selected && selected !== button,
                              editor_present:!!editor, editor_editable:editor?.getAttribute('contenteditable') === 'true',
                              focused:document.hasFocus(), alert_present:alerts.length > 0,
                              error_codes:codes.filter(code=>alerts.some(alert=>alert.includes(code))),
                              error_labels:labels.filter(([, message])=>alerts.some(alert=>alert.includes(message))).map(([label])=>label),
                              status:'unavailable'};
                            window.__TAURI_INTERNALS__.invoke('m0_renderer_session')
                              .then(owner=>window.__TAURI_INTERNALS__.invoke('workspace_sync_status', {request:{renderer_session_id:owner}}))
                              .then(value=>{result.status=statuses.includes(value.status) ? value.status : 'unknown'; done(result);})
                              .catch(()=>done(result));
                        """, document_id, asynchronous=True)
                        print("M3 remote change open diagnostic:", json.dumps(diagnostic), flush=True)
                    except Exception:
                        print("M3 remote change open diagnostic unavailable.", flush=True)
                raise
            require(checkpoint() is None, "unmodified document unexpectedly created a draft")
            opened_text = js("return document.querySelector('.cm-content').cmTile.root.view.state.doc.toString()")
            require(opened_text == original.decode("utf-8-sig").replace("\r\n", "\n"), "editor did not open the expected source")
            if args.remote_change:
                test_stage = "remote change initial sync"
                owner = js("const done=arguments[arguments.length-1]; window.__TAURI_INTERNALS__.invoke('m0_renderer_session').then(done)", asynchronous=True)

                def remote_cursor():
                    with sqlite3.connect(f"file:{client_data / 'drafts.sqlite3'}?mode=ro", uri=True) as db:
                        row = db.execute("""
                            SELECT i.cursor FROM remote_index i JOIN client_registrations r ON r.id=i.registration_id
                            WHERE r.status='active'
                        """).fetchone()
                    return int(row[0]) if row else None

                def remote_ready():
                    return js("""
                        const done=arguments[arguments.length-1];
                        window.__TAURI_INTERNALS__.invoke('workspace_sync_status', {request:{renderer_session_id:arguments[0]}})
                          .then(value=>done(value.status === 'ready')).catch(()=>done(false));
                    """, owner, asynchronous=True)

                wait(lambda: remote_cursor() is not None and remote_ready(), seconds=45, description="remote change initial sync")
                test_stage = "receiver focus setup"
                remote_focus = capture_remote_change_focus()
                wait(lambda: js("return document.hasFocus()"), seconds=10, description="receiver WebView initial focus")
                test_stage = "peer client setup"
                peer_data, peer_xdg_data, peer_xdg_cache, peer_xdg_config = (
                    root / name for name in ("peer-client", "peer-xdg-data", "peer-xdg-cache", "peer-xdg-config")
                )
                for directory in (peer_data, peer_xdg_data, peer_xdg_cache, peer_xdg_config):
                    directory.mkdir(mode=0o700)
                peer_grant = admin("issue-pairing", admin_status["owner_principal_id"])
                peer_port = free_port()
                peer_origin = f"http://127.0.0.1:{peer_port}"
                peer_environment = os.environ.copy()
                peer_environment.update(
                    TAURI_WEBVIEW_AUTOMATION="true", NOTRYX_M0_DATA_DIR=str(peer_data),
                    XDG_DATA_HOME=str(peer_xdg_data), XDG_CACHE_HOME=str(peer_xdg_cache), XDG_CONFIG_HOME=str(peer_xdg_config),
                )
                peer_driver = subprocess.Popen(
                    [args.driver, f"--port={peer_port}", "--host=127.0.0.1"], env=peer_environment,
                    stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True,
                )

                def peer_wd(method, path, payload=None):
                    data = None if payload is None else json.dumps(payload).encode()
                    request = urllib.request.Request(peer_origin + path, data=data, method=method,
                                                     headers={"Content-Type": "application/json"})
                    try:
                        with local_opener.open(request, timeout=35) as response:
                            value = json.load(response)["value"]
                        if isinstance(value, dict) and "error" in value:
                            raise TestFailure(f"peer WebDriver returned an error: {test_stage}")
                        return value
                    except (OSError, ValueError, KeyError, urllib.error.HTTPError):
                        raise TestFailure(f"peer WebDriver request failed: {test_stage}") from None

                def peer_driver_ready():
                    try:
                        return leader_running(peer_driver) and driver_owns_port(peer_driver.pid, peer_port) and peer_wd("GET", "/status")
                    except TestFailure:
                        return False

                wait(peer_driver_ready, seconds=45, description="peer WebDriver startup")
                peer_session = peer_wd("POST", "/session", capabilities)["sessionId"]
                peer_wd("POST", f"/session/{peer_session}/timeouts", {"script": 30000})

                def peer_js(script, *values, asynchronous=False):
                    mode = "async" if asynchronous else "sync"
                    return peer_wd("POST", f"/session/{peer_session}/execute/{mode}", {"script": script, "args": list(values)})

                wait(lambda: peer_js("const button=document.querySelector('.workspace-connect button'); return !!button && !button.disabled"), description="peer pairing form")
                peer_js("""
                    const form=document.querySelector('.workspace-connect');
                    form.elements.namedItem('origin').value=arguments[0];
                    form.elements.namedItem('workspace_id').value=arguments[1];
                    form.elements.namedItem('pairing_secret').value=arguments[2];
                    form.requestSubmit();
                """, f"http://127.0.0.1:{port}", peer_grant["workspace_id"], peer_grant["pairing_secret"])
                wait(lambda: peer_js("return !!document.querySelector('[data-document=\"' + arguments[0] + '\"]:not(:disabled)')", document_id), seconds=45, description="peer document list")
                require(peer_js("return document.querySelector('[name=pairing_secret]').value === ''"), "peer pairing secret remained in form")
                test_stage = "peer focus setup"
                peer_focus = capture_remote_change_focus(exclude_window=remote_focus[2])
                peer_focus[0].XCloseDisplay(peer_focus[1])
                wait(lambda: peer_js("return document.hasFocus()"), seconds=10, description="peer WebView focus")
                peer_owner = peer_js("const done=arguments[arguments.length-1]; window.__TAURI_INTERNALS__.invoke('m0_renderer_session').then(done)", asynchronous=True)

                def peer_pre_open_ready():
                    return pre_open_cursor(peer_data) is not None and peer_js(sync_details_script, peer_owner, document_id, asynchronous=True)["status"] == "ready"

                test_stage = "peer pre-open sync"
                try:
                    wait(peer_pre_open_ready, seconds=45, description="peer pre-open sync")
                except TestFailure:
                    try:
                        print("M3 peer pre-open diagnostic:", json.dumps({
                            **peer_js(sync_details_script, peer_owner, document_id, asynchronous=True),
                            "cursor": pre_open_cursor(peer_data),
                            "receiver_focused": js("return document.hasFocus()"),
                        }), flush=True)
                    except Exception:
                        print("M3 peer pre-open diagnostic unavailable.", flush=True)
                    raise
                print("M3 peer pre-open state:", json.dumps({
                    **peer_js(sync_details_script, peer_owner, document_id, asynchronous=True),
                    "cursor": pre_open_cursor(peer_data),
                    "receiver_focused": js("return document.hasFocus()"),
                }), flush=True)
                test_stage = "peer open selected document"
                peer_js("document.querySelector('[data-document=\"' + arguments[0] + '\"]').click()", document_id)
                try:
                    wait(lambda: peer_js("return document.querySelector('[aria-current=page]')?.dataset.document === arguments[0] && !!document.querySelector('.cm-content[contenteditable=true]')", document_id), description="peer selected editor")
                except TestFailure:
                    try:
                        print("M3 peer open diagnostic:", json.dumps({
                            **peer_js(sync_details_script, peer_owner, document_id, asynchronous=True),
                            "cursor": pre_open_cursor(peer_data),
                            "receiver_focused": js("return document.hasFocus()"),
                        }), flush=True)
                    except Exception:
                        print("M3 peer open diagnostic unavailable.", flush=True)
                    raise
                require(peer_js("return document.querySelector('.cm-content').cmTile.root.view.state.doc.toString()") == opened_text, "peer editor did not open original source")
                with sqlite3.connect(f"file:{client_data / 'drafts.sqlite3'}?mode=ro", uri=True) as db:
                    local_device = db.execute("SELECT device_id FROM client_registrations WHERE status='active'").fetchone()
                with sqlite3.connect(f"file:{peer_data / 'drafts.sqlite3'}?mode=ro", uri=True) as db:
                    peer_device = db.execute("SELECT device_id FROM client_registrations WHERE status='active'").fetchone()
                require(local_device is not None and peer_device is not None and local_device != peer_device, "client stores do not have independent device registrations")
                test_stage = "receiver draft setup"
                saved = None
                original_outbox = outbox_snapshot()
                require(not original_outbox, "receiving client started with an unexpected outbox")
                if args.remote_change == "dirty":
                    marker = "\nSynthetic local edit 한글\n"
                    js("""
                        const element=document.querySelector('.cm-content');
                        element.dispatchEvent(new CompositionEvent('compositionstart', {bubbles:true}));
                        const view=element.cmTile.root.view;
                        view.dispatch({changes:{from:view.state.doc.length,insert:arguments[0]},userEvent:'input.type'});
                    """, marker)
                    saved = wait(lambda: (row if (row := checkpoint()) and marker.encode() in row[1] else None), seconds=45, description="remote change durable draft")
                    require(saved[2] == original and operation_count() == 0, "local draft was sent or lost its original base")
                else:
                    js("document.querySelector('.cm-content').cmTile.root.view.dispatch({selection:{anchor:10}})")

                updated_text = opened_text.replace("Original source.", "Remote   source.")
                require(updated_text != opened_text and len(updated_text) == len(opened_text), "synthetic peer edit is invalid")
                updated = b"\xef\xbb\xbf" + updated_text.encode()
                test_stage = "peer editor save"
                peer_js("""
                    const view=document.querySelector('.cm-content').cmTile.root.view;
                    const from=view.state.doc.toString().indexOf('Original source.');
                    view.dispatch({changes:{from,to:from+'Original source.'.length,insert:'Remote   source.'},userEvent:'input.type'});
                """)

                def peer_acknowledged():
                    with sqlite3.connect(f"file:{peer_data / 'drafts.sqlite3'}?mode=ro", uri=True) as db:
                        row = db.execute("""
                            SELECT o.status, o.operation_seq, s.ack_through FROM client_outbox o
                            JOIN client_registrations r ON r.id=o.registration_id
                            JOIN outbox_streams s ON s.registration_id=r.id
                            WHERE r.status='active' AND o.document_id=? AND o.terminal_kind IS NULL
                            ORDER BY o.operation_seq DESC LIMIT 1
                        """, (document_id,)).fetchone()
                    return row is not None and row[0] == 'committed' and row[2] >= row[1]

                wait(lambda: (notes / "Test Folder" / "Example.md").read_bytes() == updated and peer_acknowledged(), seconds=60, description="peer editor save and acknowledgement")
                peer_head = int(server_request("GET", "/v1/server-info", headers=authorization)["head_seq"])
                peer_node = server_request("GET", f"/v1/nodes/{document_id}", headers=authorization)
                test_stage = "receiver focus restore"
                print("M3 resume focus before restore:", json.dumps({
                    "receiver_focused": js("return document.hasFocus()"),
                    "peer_focused": peer_js("return document.hasFocus()"),
                    "cursor": remote_cursor(), "head": peer_head,
                }), flush=True)
                saved_focus, remote_focus = remote_focus, None
                restore_open_race_focus(saved_focus)
                wait(lambda: js("return document.hasFocus()"), seconds=10, description="receiver WebView focus restoration")
                test_stage = "receiver resume metadata catch-up"

                def remote_status():
                    return js("""
                        const done=arguments[arguments.length-1];
                        const allowed=['connecting','ready','offline','auth_required','gateway_auth_required','needs_review'];
                        window.__TAURI_INTERNALS__.invoke('workspace_sync_status', {request:{renderer_session_id:arguments[0]}})
                          .then(value=>done(allowed.includes(value.status) ? value.status : 'unknown'))
                          .catch(()=>done('unavailable'));
                    """, owner, asynchronous=True)

                def remote_metadata_snapshot():
                    with sqlite3.connect(f"file:{client_data / 'drafts.sqlite3'}?mode=ro", uri=True) as db:
                        row = db.execute("""
                            SELECT n.state, i.cursor FROM remote_nodes n
                            JOIN remote_index i ON i.registration_id=n.registration_id
                            JOIN client_registrations r ON r.id=n.registration_id
                            WHERE r.status='active' AND n.node_id=?
                        """, (document_id,)).fetchone()
                    return {
                        "cursor": int(row[1]) if row else None,
                        "revision_matches": row is not None and json.loads(row[0])["content_rev"] == peer_node["content_rev"],
                    }

                def remote_metadata_applied():
                    snapshot = remote_metadata_snapshot()
                    return snapshot["cursor"] is not None and snapshot["cursor"] >= peer_head and snapshot["revision_matches"]

                try:
                    wait(remote_metadata_applied, seconds=45, description="peer metadata applied after receiver resume")
                finally:
                    try:
                        print("M3 resume metadata diagnostic:", json.dumps({
                            **remote_metadata_snapshot(), "head": peer_head,
                            "receiver_focused": js("return document.hasFocus()"), "status": remote_status(),
                        }), flush=True)
                    except Exception:
                        print("M3 resume metadata diagnostic unavailable.", flush=True)
                require((notes / "Test Folder" / "Example.md").read_bytes() == updated, "peer update did not reach synthetic server file")
                test_stage = "receiver editor result"
                if args.remote_change == "clean":
                    wait(lambda: js("return document.querySelector('[aria-current=page]')?.dataset.document === arguments[0] && document.querySelector('.cm-content')?.cmTile.root.view.state.doc.toString() === arguments[1]", document_id, updated_text), seconds=45, description="clean editor peer update")
                    require(js("return document.querySelector('.cm-content').cmTile.root.view.state.selection.main.anchor") == 10, "clean editor did not preserve selection before changed text")
                    require(checkpoint() is None and operation_count() == 0, "clean remote refresh created a local draft or operation")
                    test_stage = "receiver first edit after remote refresh"
                    first_edit = "\n합성 한글 편집\n"
                    expected_bytes = updated + first_edit.encode()
                    js("""
                        const element=document.querySelector('.cm-content');
                        element.dispatchEvent(new CompositionEvent('compositionstart', {bubbles:true}));
                        const view=element.cmTile.root.view;
                        view.dispatch({changes:{from:view.state.doc.length,insert:arguments[0]},userEvent:'input.type'});
                    """, first_edit)
                    first_checkpoint = wait(lambda: (row if (row := checkpoint()) and row[1] == expected_bytes else None), seconds=45, description="first post-refresh checkpoint")
                    with sqlite3.connect(f"file:{client_data / 'drafts.sqlite3'}?mode=ro", uri=True) as db:
                        base_revision = db.execute("""
                            SELECT b.revision FROM registered_documents d
                            JOIN client_registrations r ON r.id=d.registration_id
                            JOIN draft_bases b ON b.document_id=d.storage_id
                            WHERE r.status='active' AND d.document_id=?
                        """, (document_id,)).fetchone()
                    require(first_checkpoint[0] == 1 and first_checkpoint[2] == updated and base_revision == (peer_node["content_rev"],), "first post-refresh checkpoint did not retain the peer base")
                    require(operation_count() == 0 and (notes / "Test Folder" / "Example.md").read_bytes() == updated, "composition sent the post-refresh edit early")
                    js("document.querySelector('.cm-content').dispatchEvent(new CompositionEvent('compositionend', {bubbles:true}))")
                    test_stage = "receiver first edit acknowledgement"
                    wait(lambda: (notes / "Test Folder" / "Example.md").read_bytes() == expected_bytes and acknowledged(first_checkpoint[0]), seconds=60, description="first post-refresh edit and acknowledgement")
                    require(operation_count() == 1, "first post-refresh edit created an unexpected number of operations")
                else:
                    require(js("return document.querySelector('.cm-content').cmTile.root.view.state.doc.toString()") == opened_text + marker, "peer update replaced local editor text")
                    require(checkpoint() == saved and outbox_snapshot() == original_outbox, "peer update changed the local draft, base or outbox")
                    wait(lambda: js("return document.body.textContent.includes('다른 기기에서 변경됨')"), seconds=45, description="dirty editor peer change notice")
                    require(js("return document.querySelector('.cm-content').cmTile.root.view.state.doc.toString()") == opened_text + marker, "dirty editor changed after peer notice")
                    require(checkpoint() == saved and outbox_snapshot() == original_outbox, "dirty draft changed after peer notice")
                print(f"M3 WebView resume catch-up passed: {args.remote_change} editor and synthetic peer.", flush=True)
                return
            if args.resync_race:
                def document_change_count():
                    with sqlite3.connect(f"file:{state / 'state.db'}?mode=ro", uri=True) as db:
                        return db.execute("SELECT COUNT(*) FROM node_changes WHERE node_id=?", (document_id,)).fetchone()[0]

                test_stage = "resync local checkpoint"
                original_change_count = document_change_count()
                resync_proxy.hold_put.set()
                marker = "\nM3 synthetic sync race 한글\n"
                js("""
                    const view=document.querySelector('.cm-content').cmTile.root.view;
                    view.dispatch({changes:{from:view.state.doc.length,insert:arguments[0]},userEvent:'input.type'});
                """, marker)
                saved = wait(lambda: (row if (row := checkpoint()) and marker.encode() in row[1] else None), seconds=45, description="resync durable checkpoint")
                resync_proxy.mark("checkpoint_observed")
                require(saved[2] == original, "initial checkpoint changed the base")
                require(resync_proxy.put_response_held.wait(45), "PUT response was not held")
                resync_proxy.mark("put_hold_observed")
                require(not resync_proxy.failure.is_set(), "test proxy failed before resync")
                require(operation_count() == 1 and checkpoint() == saved, "held PUT changed local checkpoint or outbox")
                held_outbox = outbox_snapshot()
                require((notes / "Test Folder" / "Example.md").read_bytes() == saved[1], "held PUT did not reach the synthetic server")
                resync_proxy.mark("local_snapshot_checked")
                test_stage = "resync head and 410"
                previous_head = int(server_request("GET", "/v1/server-info", headers=authorization)["head_seq"])
                resync_proxy.mark("pre_peer_head_read")
                with resync_proxy.lock:
                    resync_proxy.expected_head = previous_head + 1
                    resync_proxy.inject_changes.set()
                create(operation_id(), folder_id, "Remote change", "folder", 6)
                resync_proxy.mark("peer_mutation_complete")
                peer_head = int(server_request("GET", "/v1/server-info", headers=authorization)["head_seq"])
                resync_proxy.mark("peer_head_read")
                require(peer_head == previous_head + 1, "unexpected head change during synthetic peer mutation")
                require(resync_proxy.head_delivered.wait(45), "peer head hint was not delivered to the client")
                require(resync_proxy.changes_injected.wait(45), "changes request did not receive synthetic 410")
                require(resync_proxy.inventory_held.wait(45), "rebootstrap inventory did not start")
                resync_proxy.mark("inventory_hold_observed")
                wait(lambda: js("return document.querySelector('#workspace-sync-status')?.textContent.includes('서버 동기화 중')"), seconds=45, description="resync connecting UI")
                resync_proxy.mark("connecting_ui_observed")
                require(sync_status() == "connecting", "resync did not expose its connecting state")
                require(checkpoint() == saved and outbox_snapshot() == held_outbox, "resync changed checkpoint, base or outbox")
                resync_proxy.mark("inventory_release")
                resync_proxy.release_inventory.set()
                test_stage = "resync bootstrap completion"

                def resync_ready():
                    cursor = sync_cursor()
                    if cursor is None or cursor < peer_head:
                        return False
                    resync_proxy.mark("cursor_observed")
                    if sync_status() != "ready":
                        return False
                    resync_proxy.mark("ready_native_observed")
                    if not js("return !document.querySelector('#workspace-sync-status')"):
                        return False
                    resync_proxy.mark("ready_ui_observed")
                    return True

                wait(resync_ready, seconds=60, description="resync ready after bootstrap")
                require(checkpoint() == saved and outbox_snapshot() == held_outbox, "bootstrap changed checkpoint, base or outbox")
                resync_proxy.mark("margin_checked")
                require(resync_proxy.put_started_at is not None and time.monotonic() - resync_proxy.put_started_at < 20,
                        "resync exceeded the held PUT timeout margin")
                resync_proxy.release_put.set()
                test_stage = "resync late PUT response"
                require(resync_proxy.put_response_delivered.wait(5), "held PUT response was not delivered")
                require(resync_proxy.retry_lookup.wait(45), "scheduler did not recheck the held operation")
                wait(lambda: acknowledged(saved[0]) and checkpoint()[1:] == (saved[1], saved[1]), seconds=60, description="resync operation acknowledgement")
                resync_proxy.mark("acknowledgement_observed")
                require(operation_count() == 1 and (notes / "Test Folder" / "Example.md").read_bytes() == saved[1], "operation was duplicated or server content changed")
                with sqlite3.connect(f"file:{state / 'state.db'}?mode=ro", uri=True) as db:
                    committed = db.execute("""
                        SELECT COUNT(*) FROM change_sets c JOIN node_changes n ON n.seq=c.seq
                        WHERE c.operation_id=? AND n.node_id=?
                    """, (resync_proxy.operation_id, document_id)).fetchone()[0]
                require(committed == 1 and document_change_count() == original_change_count + 1,
                        "server journal did not record exactly one document change")
                require(not resync_proxy.failure.is_set(), "test proxy failed during resync")
                test_stage = "resync renderer reload"
                wd("POST", f"/session/{session_id}/refresh", {})
                wait(lambda: js("return !!document.querySelector('[data-document=\"' + arguments[0] + '\"]:not(:disabled)')", document_id), seconds=45, description="resync document list after reload")
                js("document.querySelector('[data-document=\"' + arguments[0] + '\"]').click()", document_id)
                wait(lambda: js("return document.querySelector('[aria-current=page]')?.dataset.document === arguments[0] && !!document.querySelector('.cm-content[contenteditable=true]')", document_id), seconds=45, description="resync document reopen")
                reopened = js("return document.querySelector('.cm-content').cmTile.root.view.state.doc.toString()")
                require(marker in reopened and checkpoint()[1:] == (saved[1], saved[1]) and operation_count() == 1, "renderer reload changed the result")
                print("M3 WebView resync race passed: held PUT, 410, bootstrap, operation recheck, ACK and renderer reload.")
                return
            if not args.measure and not args.recovery_dialogs:
                oversized = js("""
                    const view=document.querySelector('.cm-content').cmTile.root.view;
                    const before=view.state.doc.toString();
                    view.dispatch({changes:{from:view.state.doc.length,insert:'x'.repeat(64*1024*1024+1)},userEvent:'input.paste'});
                    return {unchanged:view.state.doc.toString()===before, beforeLength:before.length, afterLength:view.state.doc.length};
                """)
                require(oversized["unchanged"] and oversized["beforeLength"] == oversized["afterLength"], "oversized paste changed the editor source")
                wait(lambda: js("return [...document.querySelectorAll('[role=alert]')].some(e=>e.textContent.includes('입력은 적용되지 않았습니다') && e.textContent.includes('5 MiB'))"))
                time.sleep(0.35)
                require(checkpoint() is None and operation_count() == 0, "oversized paste created a checkpoint or operation")
                require((notes / "Test Folder" / "Example.md").read_bytes() == original, "oversized paste changed the server source")
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
            wait(lambda: js("""
                const images=[...document.querySelectorAll('.markdown-preview img[alt=local]')];
                return images.length === 17 && images.every(image => image.src.startsWith('blob:') && image.complete && image.naturalWidth === 1);
            """), seconds=45)
            js("""
                window.__notryxScrolls=[];
                window.__notryxOriginalScroll=Element.prototype.scrollIntoView;
                Element.prototype.scrollIntoView=function(){window.__notryxScrolls.push(this.getAttribute('data-notryx-heading'))};
            """)
            js("const button=[...document.querySelectorAll('.markdown-preview button')].find(item=>item.textContent==='same'); if (!button) throw new Error('same_anchor_missing'); button.click()")
            wait(lambda: js("return window.__notryxScrolls.includes('synthetic')"))
            js("const button=[...document.querySelectorAll('.markdown-preview button')].find(item=>item.textContent==='other'); if (!button) throw new Error('cross_anchor_missing'); button.click()")
            wait(lambda: js("return document.querySelector('[aria-current=page]')?.dataset.document === arguments[0]", target_id))
            wait(lambda: js("return window.__notryxScrolls.includes('destination')"))
            js("Element.prototype.scrollIntoView=window.__notryxOriginalScroll; delete window.__notryxOriginalScroll; delete window.__notryxScrolls")
            js("document.querySelector('[data-document=\"' + arguments[0] + '\"]').click()", document_id)
            wait(lambda: js("return document.querySelector('[aria-current=page]')?.dataset.document === arguments[0] && !!document.querySelector('.cm-content[contenteditable=true]')", document_id))
            wait(lambda: js("const image=document.querySelector('.markdown-preview img[alt=local]'); return !!image && image.src.startsWith('blob:') && image.complete && image.naturalWidth === 1"))
            time.sleep(0.5)
            require(not remote_requests and not js("return !!document.querySelector('.markdown-preview img[alt=remote]')"), "preview requested a remote image")
            js("document.querySelector('#preview-toggle').click()")
            before_reload = "편집 입력😀\n"
            with sqlite3.connect(client_data / "drafts.sqlite3", timeout=10) as db:
                db.execute("CREATE TRIGGER m2_fail_queue BEFORE INSERT ON client_outbox BEGIN SELECT RAISE(ABORT, 'synthetic queue failure'); END")
                db.commit()
            js("""
                const view=document.querySelector('.cm-content').cmTile.root.view;
                view.dispatch({changes:{from:view.state.doc.length,insert:arguments[0]}});
            """, before_reload)
            saved = wait(lambda: (row if (row := checkpoint()) and before_reload.encode() in row[1] else None))
            require(saved[1] == original.replace(b"\r\n", b"\n") + before_reload.encode(), "edited checkpoint did not preserve source")
            wait(lambda: js("return [...document.querySelectorAll('[role=alert]')].some(e=>e.textContent.includes('전송을 준비하지 못했습니다'))"), seconds=45)
            require(saved[2] == original and operation_count() == 0, "failed queue changed the durable checkpoint or created an operation")
            require((notes / "Test Folder" / "Example.md").read_bytes() == original, "failed queue changed the server source")
            synced_bytes = saved[1]
            wd("POST", f"/session/{session_id}/refresh", {})
            require(checkpoint()[:2] == saved[:2] and operation_count() == 0, "reload changed the unqueued durable checkpoint")
            with sqlite3.connect(client_data / "drafts.sqlite3", timeout=10) as db:
                db.execute("DROP TRIGGER m2_fail_queue")
                db.commit()
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
            if args.unscoped_recovery:
                db_path = client_data / "drafts.sqlite3"
                with sqlite3.connect(db_path, timeout=10) as db:
                    old_registration, recovery_storage = db.execute("""
                        SELECT r.id, d.storage_id FROM registered_documents d
                        JOIN client_registrations r ON r.id = d.registration_id
                        WHERE r.status = 'active' AND d.document_id = ?
                    """, (document_id,)).fetchone()
                recovery_grant = admin("issue-pairing", admin_status["owner_principal_id"])
                js("""
                    const form=document.querySelector('.workspace-connect');
                    form.elements.namedItem('origin').value=arguments[0];
                    form.elements.namedItem('workspace_id').value=arguments[1];
                    form.elements.namedItem('pairing_secret').value=arguments[2];
                    form.requestSubmit();
                """, f"http://127.0.0.1:{port}", recovery_grant["workspace_id"], recovery_grant["pairing_secret"])

                def active_registration():
                    with sqlite3.connect(f"file:{db_path}?mode=ro", uri=True) as db:
                        row = db.execute("SELECT id FROM client_registrations WHERE status = 'active'").fetchone()
                    return row[0] if row and row[0] != old_registration else None

                current_registration = wait(active_registration, seconds=45)
                wait(lambda: js("return !!document.querySelector('[data-draft-document=\"' + arguments[0] + '\"]:not(:disabled)') && !!document.querySelector('[data-document=\"' + arguments[1] + '\"]:not(:disabled)')", document_id, target_id), seconds=45)
                with sqlite3.connect(db_path, timeout=10) as db:
                    require(db.execute("SELECT status FROM client_registrations WHERE id = ?", (old_registration,)).fetchone()[0] == "frozen", "old registration was not frozen")
                    db.execute(f"CREATE TRIGGER m2_hold_send BEFORE UPDATE OF status ON client_outbox WHEN OLD.registration_id = {current_registration} AND NEW.status = 'sent' BEGIN SELECT RAISE(ABORT, 'synthetic send preparation failure'); END")
                    db.commit()
                js("document.querySelector('[data-document=\"' + arguments[0] + '\"]').click()", target_id)
                wait(lambda: js("return document.querySelector('[aria-current=page]')?.dataset.document === arguments[0] && !!document.querySelector('.cm-content[contenteditable=true]')", target_id))
                pending_text = "queued current registration\n"
                js("const view=document.querySelector('.cm-content').cmTile.root.view; view.dispatch({changes:{from:view.state.doc.length,insert:arguments[0]},userEvent:'input.type'})", pending_text)
                pending = wait(lambda: (row if (row := checkpoint(target_id)) and pending_text.encode() in row[1] else None), seconds=45)

                def current_outbox():
                    with sqlite3.connect(f"file:{db_path}?mode=ro", uri=True) as db:
                        return db.execute("SELECT operation_id, generation, status FROM client_outbox WHERE registration_id = ? AND document_id = ?", (current_registration, target_id)).fetchall()

                wait(lambda: current_outbox() and js("return [...document.querySelectorAll('[role=alert]')].some(e=>e.textContent.includes('Storage'))"), seconds=45)
                queued = current_outbox()
                require(len(queued) == 1 and queued[0][1:] == (pending[0], "queued"), "current registration operation was not held before sending")
                require((notes / "Test Folder" / "Target.md").read_bytes() == target_source, "held operation reached the server")
                js("document.querySelector('[data-draft-document=\"' + arguments[0] + '\"]').click()", document_id)
                wait(lambda: js("return document.querySelector('[data-draft-document=\"' + arguments[0] + '\"]')?.getAttribute('aria-current') === 'page' && !!document.querySelector('.cm-content[contenteditable=true]')", document_id), seconds=45)
                with sqlite3.connect(db_path, timeout=10) as db:
                    quoted_storage = db.execute("SELECT quote(?)", (recovery_storage,)).fetchone()[0]
                    db.execute(f"CREATE TRIGGER m2_fail_recovery BEFORE INSERT ON draft_checkpoints WHEN NEW.document_id = {quoted_storage} BEGIN SELECT RAISE(ABORT, 'synthetic recovery checkpoint failure'); END")
                    db.commit()

                def recovery_checkpoint():
                    with sqlite3.connect(f"file:{db_path}?mode=ro", uri=True) as db:
                        return db.execute("SELECT generation, bytes FROM draft_checkpoints WHERE document_id = ?", (recovery_storage,)).fetchone()

                recovery_before = recovery_checkpoint()
                require(recovery_before == saved[:2], "frozen recovery draft did not preserve its checkpoint")
                recovery_text = "recovery edit pending\n"
                js("const view=document.querySelector('.cm-content').cmTile.root.view; view.dispatch({changes:{from:view.state.doc.length,insert:arguments[0]},userEvent:'input.type'})", recovery_text)
                wait(lambda: js("return document.querySelector('#save-status')?.textContent.includes('저장 오류')"), seconds=45)
                require(recovery_checkpoint() == recovery_before and current_outbox() == queued, "failure injection did not preserve the expected pending state")
                with sqlite3.connect(db_path, timeout=10) as db:
                    db.execute("DROP TRIGGER m2_hold_send")
                    db.commit()
                previous_observation = time.monotonic()
                previous_renderer = js("return performance.now()")
                require(time.monotonic() - previous_observation < 5, "renderer did not respond before the retry observation")
                previous_observation = time.monotonic()
                deadline = previous_observation + 32
                while time.monotonic() < deadline:
                    require(recovery_checkpoint() == recovery_before and current_outbox() == queued, "pending recovery checkpoint did not block the existing outbox")
                    require((notes / "Test Folder" / "Target.md").read_bytes() == target_source, "server changed while recovery checkpoint was pending")
                    renderer_now = js("return performance.now()")
                    observation_now = time.monotonic()
                    require(renderer_now > previous_renderer and observation_now - previous_observation < 5, "renderer did not advance during the retry observation")
                    previous_renderer, previous_observation = renderer_now, observation_now
                    time.sleep(0.25)
                with sqlite3.connect(db_path, timeout=10) as db:
                    db.execute("DROP TRIGGER m2_fail_recovery")
                    db.commit()
                js("document.querySelector('#save').click()")
                restored = wait(lambda: (row if (row := recovery_checkpoint()) and row[0] > recovery_before[0] and recovery_text.encode() in row[1] else None), seconds=45)
                require(restored[1] == recovery_before[1] + recovery_text.encode(), "recovery checkpoint did not contain the edited bytes")
                wait(lambda: acknowledged(pending[0], target_id) and (notes / "Test Folder" / "Target.md").read_bytes() == pending[1], seconds=45)
                print("Unscoped recovery passed: failed checkpoint blocked the existing outbox until the recovery draft became durable.", flush=True)
                return
            if not (args.measure or args.recovery_dialogs or args.boundary_25m or args.verify_5m):
                def type_edit(value):
                    return js("const view=document.querySelector('.cm-content').cmTile.root.view; view.dispatch({changes:{from:view.state.doc.length,insert:arguments[0]},userEvent:'input.type'}); return Date.now()", value)

                def type_continuously(result, failures):
                    try:
                        result.append(js("""
                            const done=arguments[arguments.length-1];
                            const view=document.querySelector('.cm-content').cmTile.root.view;
                            let count=0, previous=performance.now(), maxGap=0, firstWall=0;
                            const timer=setInterval(()=>{
                                const now=performance.now(); maxGap=Math.max(maxGap,now-previous); previous=now;
                                if (count===0) firstWall=Date.now();
                                view.dispatch({changes:{from:0,to:1,insert:count%2?'b':'a'},userEvent:'input.type'});
                                if (++count===100) { clearInterval(timer); done({count,maxGap,firstWall}); }
                            },100);
                        """, asynchronous=True))
                    except Exception as error:
                        failures.append(error)

                first_edit_ms = type_edit("policy-a\n")
                time.sleep(0.4)
                last_edit_ms = type_edit("policy-b\n")
                time.sleep(0.35)
                before_continuous = checkpoint()
                operations_before_continuous = operation_count()
                source_before_continuous = (notes / "Test Folder" / "Example.md").read_bytes()
                require(last_edit_ms - first_edit_ms < 900 and time.time() * 1000 - last_edit_ms < 900, "idle timer test could not sample before the deadline")
                require(before_continuous == saved and operations_before_continuous == 1, "automatic save or transmission started before one second of inactivity")
                require(source_before_continuous == synced_bytes, "server changed before one second of inactivity")
                continuous_result, continuous_failures = [], []
                worker = threading.Thread(target=type_continuously, args=(continuous_result, continuous_failures), daemon=True)
                worker.start()
                saved_while_typing = False
                sent_while_typing = False
                while worker.is_alive():
                    row = checkpoint()
                    if row is not None and row[0] > saved[0]:
                        saved_while_typing = True
                    if operation_count() != 1 or (notes / "Test Folder" / "Example.md").read_bytes() != synced_bytes:
                        sent_while_typing = True
                    time.sleep(0.1)
                worker.join()
                require(not continuous_failures and len(continuous_result) == 1, "continuous typing did not finish")
                require(continuous_result[0]["firstWall"] - last_edit_ms < 1000, "continuous typing started after an idle interval")
                require(continuous_result[0]["count"] == 100 and continuous_result[0]["maxGap"] < 1000, "continuous typing was interrupted by an idle interval")
                require(saved_while_typing, "continuous typing was not checkpointed before it stopped")
                require(not sent_while_typing, "continuous typing caused automatic server transmission")
                final_generation = saved[0] + 102
                saved = wait(lambda: (row if (row := checkpoint()) and row[0] == final_generation else None), seconds=45)
                wait(lambda: acknowledged(final_generation) and (notes / "Test Folder" / "Example.md").read_bytes() == saved[1], seconds=45)
                saved = wait(lambda: (row if (row := checkpoint()) and row[0] == final_generation and row[2] == row[1] else None), seconds=45)
                require(operation_count() == 2, "one idle interval did not produce exactly one server operation")
                synced_bytes = saved[1]
                handled = js("""
                    const view=document.querySelector('.cm-content').cmTile.root.view;
                    view.dispatch({changes:{from:view.state.doc.length,insert:'manual-save\\n'},userEvent:'input.type'});
                    view.contentDOM.focus();
                    const key=new KeyboardEvent('keydown',{key:'s',code:'KeyS',ctrlKey:true,bubbles:true,cancelable:true});
                    view.contentDOM.dispatchEvent(key);
                    return key.defaultPrevented;
                """)
                require(handled, "Ctrl+S was not handled by the editor")
                final_generation += 1
                saved = wait(lambda: (row if (row := checkpoint()) and row[0] == final_generation else None), seconds=45)
                wait(lambda: acknowledged(final_generation) and (notes / "Test Folder" / "Example.md").read_bytes() == saved[1], seconds=45)
                saved = wait(lambda: (row if (row := checkpoint()) and row[0] == final_generation and row[2] == row[1] else None), seconds=45)
                require(operation_count() == 3, "Ctrl+S did not preserve and send one operation")
                synced_bytes = saved[1]
                print("Autosave policy passed: one-second idle, continuous local checkpoint, deferred transmission and Ctrl+S.", flush=True)
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
            if verify_5m_id is not None:
                js("document.querySelector('[data-document=\"' + arguments[0] + '\"]').click()", verify_5m_id)
                wait(lambda: js("return document.querySelector('[aria-current=page]')?.dataset.document === arguments[0] && !!document.querySelector('.cm-content[contenteditable=true]')", verify_5m_id), seconds=120)
                require(checkpoint(verify_5m_id) is None and operation_count(verify_5m_id) == 0, "5 MiB fixture already has local edits")
                js("const view=document.querySelector('.cm-content').cmTile.root.view; view.dispatch({changes:{from:view.state.doc.length,insert:'!'},userEvent:'input.type'})")
                wait(lambda: durable_generation(verify_5m_id) >= 1, seconds=120)
                verify_5m_saved = checkpoint(verify_5m_id)
                require(verify_5m_saved is not None and verify_5m_saved[0] == 1 and verify_5m_saved[1] == verify_5m_expected and verify_5m_saved[2] in (verify_5m_source, verify_5m_expected), "5 MiB checkpoint or base differs from the expected bytes")
                js("document.querySelector('#save').click()")
                verify_5m_path = notes / "Test Folder" / "Boundary 5 MiB.md"
                wait(lambda: verify_5m_path.stat().st_size == verify_5m_size and acknowledged(verify_5m_saved[0], verify_5m_id), seconds=120)
                with verify_5m_path.open("rb") as source:
                    require(hashlib.file_digest(source, "sha256").digest() == verify_5m_hash, "5 MiB server source differs from the saved checkpoint")
                require(operation_count(verify_5m_id) == 1 and checkpoint(verify_5m_id)[1:] == (verify_5m_expected, verify_5m_expected), "5 MiB acknowledgement changed the checkpoint or base")
                wd("POST", f"/session/{session_id}/refresh", {})
                wait(lambda: js("return !!document.querySelector('[data-document=\"' + arguments[0] + '\"]:not(:disabled)')", verify_5m_id), seconds=120)
                js("document.querySelector('[data-document=\"' + arguments[0] + '\"]').click()", verify_5m_id)
                wait(lambda: js("return document.querySelector('[aria-current=page]')?.dataset.document === arguments[0] && !!document.querySelector('.cm-content[contenteditable=true]')", verify_5m_id), seconds=120)
                require(js("""
                    const view=document.querySelector('.cm-content').cmTile.root.view;
                    const size=arguments[1];
                    const line='x'.repeat(63)+'\\n';
                    const expected=arguments[0]+line.repeat(Math.floor(size/64))+line.slice(0,size%64)+'!';
                    return view.state.doc.toString() === expected;
                """, verify_5m_prefix_text, verify_5m_body_size), "5 MiB document did not reopen with the saved text")
                require(checkpoint(verify_5m_id)[1:] == (verify_5m_expected, verify_5m_expected) and operation_count(verify_5m_id) == 1, "5 MiB reload changed the saved checkpoint, base or outbox")
                js("document.querySelector('[data-document=\"' + arguments[0] + '\"]').click()", document_id)
                wait(lambda: js("return document.querySelector('[aria-current=page]')?.dataset.document === arguments[0] && !!document.querySelector('.cm-content[contenteditable=true]')", document_id), seconds=120)
                require(checkpoint() == saved, "5 MiB verification changed the existing document draft")
                print("5 MiB WebView checkpoint passed: BOM, Korean and emoji bytes persisted, reached the server and reopened.", flush=True)
            if boundary_id is not None:
                before_outbox = outbox_snapshot()
                require(checkpoint(boundary_id) is None and operation_count(boundary_id) == 0, "25 MiB fixture already has local edits")
                js("document.querySelector('[data-document=\"' + arguments[0] + '\"]').click()", boundary_id)
                wait(lambda: js("return document.querySelector('[aria-current=page]')?.dataset.document === arguments[0] && !!document.querySelector('.cm-content[contenteditable=false]')", boundary_id), seconds=120)
                boundary_result = js("""
                    const content=document.querySelector('.cm-content');
                    const view=content.cmTile.root.view;
                    const doc=view.state.doc;
                    const beforeLength=doc.length;
                    const prefix=doc.sliceString(0,63);
                    const firstLine=content.querySelector('.cm-line')?.textContent;
                    content.focus();
                    document.execCommand('insertText',false,'unexpected-edit');
                    return {beforeLength,prefix,firstLine,afterLength:view.state.doc.length,unchanged:doc.eq(view.state.doc)};
                """)
                require(boundary_result == {"beforeLength": boundary_body_size, "prefix": boundary_prefix,
                                             "firstLine": boundary_prefix, "afterLength": boundary_body_size, "unchanged": True},
                        "25 MiB Markdown was not displayed read-only with the expected source")
                shown_digest = js("""
                    const done=arguments[arguments.length-1];
                    const view=document.querySelector('.cm-content').cmTile.root.view;
                    const source=view.state.doc.toString();
                    const expected=('r'.repeat(63)+'\\n').repeat(Math.floor(arguments[0]/64))+'r'.repeat(arguments[0]%64);
                    if (source !== expected) { done({exact:false,hash:null}); return; }
                    if (!globalThis.crypto?.subtle) { done({exact:true,hash:null}); return; }
                    const bytes=new TextEncoder().encode('\ufeff' + source);
                    crypto.subtle.digest('SHA-256',bytes).then(hash=>done({exact:true,hash:[...new Uint8Array(hash)].map(byte=>byte.toString(16).padStart(2,'0')).join('')})).catch(()=>done({exact:true,hash:'digest_failed'}));
                """, boundary_body_size, asynchronous=True)
                require(shown_digest["exact"] and shown_digest["hash"] in (None, boundary_hash.hex()), "25 MiB displayed document differs from the source")
                js("const view=document.querySelector('.cm-content').cmTile.root.view; view.dispatch({selection:{anchor:view.state.doc.length},scrollIntoView:true})")
                wait(lambda: js("""
                    const content=document.querySelector('.cm-content');
                    const view=content.cmTile.root.view;
                    const lines=content.querySelectorAll('.cm-line');
                    return view.viewport.to === view.state.doc.length && lines.length > 0 && lines[lines.length-1].textContent === arguments[0];
                """, boundary_last_line), seconds=120)
                js("document.querySelector('#save').click()")
                time.sleep(0.35)
                require(checkpoint(boundary_id) is None and operation_count(boundary_id) == 0, "25 MiB read-only document created a draft or operation")
                with (notes / "Test Folder" / "Boundary 25 MiB.md").open("rb") as source:
                    require(hashlib.file_digest(source, "sha256").digest() == boundary_hash, "25 MiB read-only source changed")
                js("document.querySelector('[data-document=\"' + arguments[0] + '\"]').click()", document_id)
                wait(lambda: js("return document.querySelector('[aria-current=page]')?.dataset.document === arguments[0] && !!document.querySelector('.cm-content[contenteditable=true]')", document_id), seconds=120)
                require(checkpoint() == saved and outbox_snapshot() == before_outbox, "25 MiB read-only document changed an existing draft or outbox")
                print("25 MiB Markdown WebView boundary passed: read-only source, existing draft and outbox preserved.", flush=True)
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
            if boundary_id is not None:
                js("document.querySelector('[data-document=\"' + arguments[0] + '\"]').click()", boundary_draft_id)
                wait(lambda: js("return document.querySelector('[aria-current=page]')?.dataset.document === arguments[0] && !!document.querySelector('.cm-content[contenteditable=true]')", boundary_draft_id), seconds=120)
                with sqlite3.connect(f"file:{client_data / 'drafts.sqlite3'}?mode=ro", uri=True) as db:
                    ui_device_id = db.execute("SELECT device_id FROM client_registrations WHERE status = 'active'").fetchone()[0]
                admin("revoke-device", ui_device_id)
                js("const view=document.querySelector('.cm-content').cmTile.root.view; view.dispatch({changes:{from:view.state.doc.length,insert:'unsent local draft\\n'}})")
                retained_draft = wait(lambda: (row if (row := checkpoint(boundary_draft_id)) and b'unsent local draft' in row[1] else None), seconds=45)
                wait(lambda: js("return [...document.querySelectorAll('[role=alert]')].some(e=>e.textContent.includes('기기 인증이'))"), seconds=45)
                js("document.querySelector('[data-document=\"' + arguments[0] + '\"]').click()", document_id)
                wait(lambda: js("return document.querySelector('[aria-current=page]')?.dataset.document === arguments[0] && !!document.querySelector('.cm-content[contenteditable=true]')", document_id), seconds=45)
                retained_outbox = outbox_snapshot()
                require(operation_count(boundary_draft_id) == 1, "unsent boundary draft was not queued")
                connection = http.client.HTTPConnection("127.0.0.1", port, timeout=90)
                try:
                    connection.request("GET", f"/v1/nodes/{boundary_draft_id}/content", headers=authorization)
                    response = connection.getresponse()
                    current_source = response.read()
                    current_etag = response.getheader("ETag")
                    require(response.status == 200 and current_source == boundary_draft_source and current_etag, "boundary draft server source changed unexpectedly")
                finally:
                    connection.close()
                boundary_source = (notes / "Test Folder" / "Boundary 25 MiB.md").read_bytes()
                server_request("PUT", f"/v1/nodes/{boundary_draft_id}/content", data=boundary_source, headers={
                    **authorization, "Content-Type": "application/octet-stream", "Content-Digest": "sha-256=:" + base64.b64encode(boundary_hash).decode() + ":",
                    "If-Match": current_etag, "X-Workspace-Epoch": info["workspace_epoch"],
                    "X-Device-Stream": pair["device_stream_id"], "X-Operation-Seq": "8", "Idempotency-Key": operation_id(),
                })
                del boundary_source
                js("document.querySelector('[data-document=\"' + arguments[0] + '\"]').click()", boundary_draft_id)
                wait(lambda: js("return document.querySelector('[aria-current=page]')?.dataset.document === arguments[0] && !!document.querySelector('.cm-content[contenteditable=true]')", boundary_draft_id), seconds=45)
                draft_text = js("return document.querySelector('.cm-content').cmTile.root.view.state.doc.toString()")
                require(draft_text == retained_draft[1].decode("utf-8") and checkpoint(boundary_draft_id) == retained_draft, "25 MiB server source replaced the existing local draft")
                with (notes / "Test Folder" / "Boundary Draft.md").open("rb") as source:
                    require(hashlib.file_digest(source, "sha256").digest() == boundary_hash, "reopening an existing draft changed the 25 MiB server source")
                js("document.querySelector('[data-document=\"' + arguments[0] + '\"]').click()", document_id)
                wait(lambda: js("return document.querySelector('[aria-current=page]')?.dataset.document === arguments[0] && !!document.querySelector('.cm-content[contenteditable=true]')", document_id), seconds=45)
                require(checkpoint(boundary_draft_id) == retained_draft and outbox_snapshot() == retained_outbox, "reopening the draft changed its checkpoint, base or outbox")
                print("25 MiB existing-draft boundary passed: unsent draft, base, outbox and server source preserved.", flush=True)
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
            if args.resync_race and resync_proxy:
                print("M3 resync timing:", json.dumps(resync_proxy.diagnostics(), sort_keys=True), flush=True)
            if peer_session and peer_wd:
                try:
                    peer_wd("DELETE", f"/session/{peer_session}")
                except Exception:
                    pass
            if remote_focus:
                remote_focus[0].XCloseDisplay(remote_focus[1])
            try:
                stop(peer_driver)
            finally:
                try:
                    restore_open_race_focus(race_focus)
                except (OSError, ValueError):
                    print("Isolated focus restoration failed.", file=sys.stderr, flush=True)
                if session_id:
                    try:
                        wd("DELETE", f"/session/{session_id}")
                    except Exception:
                        pass
                try:
                    stop(driver)
                finally:
                    try:
                        if resync_proxy:
                            resync_proxy.close()
                    finally:
                        try:
                            stop(server)
                        finally:
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
