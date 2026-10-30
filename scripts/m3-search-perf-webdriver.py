#!/usr/bin/env python3
"""Measure isolated synthetic local search in the real Linux Tauri WebView.

Fixture rows are written only while the isolated app is stopped. They do not
exercise server bootstrap or content download. No real workspace is accessed.
"""
import argparse
import base64
import contextlib
import ctypes
import datetime
import fcntl
import hashlib
import importlib.util
import json
import math
import multiprocessing
import os
from pathlib import Path
import platform
import shutil
import signal
import sqlite3
import subprocess
import sys
import tempfile
import threading
import time
import unicodedata
import urllib.error
import urllib.request
import uuid

MIB = 1024 * 1024
COUNT = 10_000
SMALL = 32 * 1024
LARGE = 5 * MIB
READONLY = 25 * MIB
PREFLIGHT_FREE_BYTES = 2 * 1024 * MIB
RUNTIME_FREE_BYTES = 1024 * MIB
CPU_NICE_INCREMENT = 10
IO_PSI_THRESHOLD = 45.0
IO_PSI_DURATION_SECONDS = 10.0
ROOT_ID = "019a1000-0000-7000-8000-000000000001"
QUERIES = {
    "korean_2": "가나",
    "korean_3": "가나다",
    "nfd": "가나",
    "ascii_match": "Synthetic document 12",
    "literal": "%_",
    "absent": "not-present-zyxw",
    "large": "대형전용표식",
    "one_character": "가",
}


def helpers_module():
    path = Path(__file__).with_name("m2-workspace-webdriver.py")
    spec = importlib.util.spec_from_file_location("notryx_m2_search_perf_helpers", path)
    if spec is None or spec.loader is None:
        raise RuntimeError("WebDriver helper unavailable")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


helpers = helpers_module()


def ensure(condition, message):
    if not condition:
        raise RuntimeError(message)


def doc_id(index):
    return f"019a1000-0000-7000-8001-{index + 1:012x}"


def content(index):
    size = LARGE if index == 0 else READONLY if index == 1 else SMALL
    if index == 3:
        return b"\xff" + b"x" * (size - 1)
    text = f"# Synthetic document {index}\n"
    if index % 37 == 0:
        text += "가나\n"
    if index % 53 == 0:
        text += "가나다\n"
    if index % 71 == 0:
        text += "가나\n"
    if index % 83 == 0:
        text += "%_\n"
    if index == 1:
        text += "대형전용표식\n" + "a" + "\u0301" * 256 + "\n"
    data = text.encode("utf-8")
    ensure(len(data) < size, "invalid synthetic content size")
    return data + b"x" * (size - len(data))


def dirty_content(base):
    prefix = "기기초안가나\n".encode("utf-8")
    return prefix + base[len(prefix):]


def state(index, data):
    revision = f"perf-content-{index}"
    return {
        "node_id": doc_id(index), "parent_id": ROOT_ID,
        "name": f"synthetic-{index:05}.md", "kind": "file", "deleted": False,
        "content_rev": revision, "content_etag": f'"{revision}"',
        "meta_rev": f"perf-meta-{index}", "meta_etag": f'"perf-meta-{index}"',
        "node_etag": f'"perf-node-{index}"', "content_hash": "sha256:" + hashlib.sha256(data).hexdigest(),
        "size": str(len(data)), "children_generation": "0", "last_change_seq": "1",
    }


def root_state():
    return {"node_id": ROOT_ID, "parent_id": None, "name": "", "kind": "folder", "deleted": False,
            "content_rev": None, "content_etag": None, "content_hash": None, "size": None,
            "meta_rev": "root-meta", "meta_etag": '"root-meta"', "node_etag": '"root-node"',
            "children_generation": "1", "last_change_seq": "1"}


def percentile(values, fraction):
    if not values:
        return None
    ordered = sorted(values)
    index = max(0, min(len(ordered) - 1, int((len(ordered) * fraction + 0.999999999)) - 1))
    return ordered[index]


def summary(values):
    return {"count": len(values), "p50_ms": percentile(values, .5),
            "p95_ms": percentile(values, .95), "max_ms": max(values) if values else None,
            "first_sample_ms": values[0] if values else None, "samples_ms": values}


def meminfo():
    return helpers.proc_fields("/proc/meminfo")


def io_psi_some_avg10():
    try:
        for line in Path("/proc/pressure/io").read_text().splitlines():
            fields = line.split()
            if fields and fields[0] == "some":
                values = dict(field.split("=", 1) for field in fields[1:])
                value = float(values["avg10"])
                if math.isfinite(value) and 0 <= value <= 100:
                    return value
                return None
    except (OSError, ValueError, KeyError):
        return None
    return None


def descendants(root):
    todo, seen = [root], set()
    while todo:
        pid = todo.pop()
        if pid in seen:
            continue
        seen.add(pid)
        try:
            for task in Path(f"/proc/{pid}/task").iterdir():
                todo.extend(int(item) for item in (task / "children").read_text().split())
        except (OSError, ValueError):
            pass
    return seen


def process_stat(pid):
    try:
        fields = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
        return {"ticks": int(fields[11]) + int(fields[12]), "starttime": int(fields[19]), "state": fields[0]}
    except (OSError, IndexError, ValueError):
        return None


def signal_pinned(pidfd, signum):
    if pidfd is None:
        return "identity_unavailable"
    try:
        signal.pidfd_send_signal(pidfd, signum)
        return "sent"
    except ProcessLookupError:
        return "already_exited"
    except OSError as error:
        return f"error_{error.errno}"


def group_observation(pgid, identities):
    members = []
    errors = []
    known = set(identities)
    known_pids = {pid for pid, _ in identities}
    try:
        proc_mount = next(line.split() for line in Path("/proc/mounts").read_text().splitlines()
                          if line.split()[1] == "/proc")
        if any(item.startswith("hidepid=") and item != "hidepid=0" for item in proc_mount[3].split(",")):
            errors.append("proc_visibility_restricted")
        paths = list(Path("/proc").iterdir())
    except (OSError, StopIteration, IndexError):
        return {"members": [], "observed_children": [], "errors": ["proc_scan_unavailable"]}
    seen = {}
    for path in paths:
        if not path.name.isdigit():
            continue
        try:
            fields = (path / "stat").read_text().rsplit(")", 1)[1].split()
            pid = int(path.name)
            item = {"pid": pid, "state": fields[0], "pgid": int(fields[2]), "starttime": int(fields[19])}
        except FileNotFoundError:
            continue
        except (OSError, IndexError, ValueError):
            errors.append("proc_entry_unreadable")
            continue
        if pid in known_pids:
            seen[pid] = item
        if item["pgid"] == pgid:
            item["known_identity"] = (pid, item["starttime"]) in known
            members.append(item)
    children = []
    for pid, started in identities:
        item = seen.get(pid)
        if item is None:
            children.append({"pid": pid, "starttime": started, "status": "absent"})
        elif item["starttime"] != started:
            children.append({"pid": pid, "starttime": started, "status": "pid_reused"})
        else:
            children.append({**item, "status": "same_identity"})
    return {"members": members, "observed_children": children, "errors": sorted(set(errors))}


def observation_clear(observation):
    if observation["errors"]:
        return False
    if any(item["state"] != "Z" for item in observation["members"]):
        return False
    return not any(item["status"] == "same_identity" and item["state"] != "Z"
                   for item in observation["observed_children"])


def wait_group_clear(watchdog, pgid, identities, phase):
    deadline = time.monotonic() + 2
    clear_count = 0
    while time.monotonic() < deadline:
        identities.update(watchdog.observed_children.get(pgid, set()).copy())
        observation = group_observation(pgid, identities)
        watchdog.cleanup_observations.append({"phase": phase, "pgid": pgid, **observation})
        clear_count = clear_count + 1 if observation_clear(observation) else 0
        if clear_count >= 2:
            return True
        time.sleep(.1)
    return False


class Watchdog:
    def __init__(self, scratch_parent, max_runtime):
        memory = meminfo()
        ensure(memory.get("MemAvailable", 0) >= 3 * 1024 * 1024, "insufficient host available memory")
        self.initial_swap = memory.get("SwapTotal", 0) - memory.get("SwapFree", 0)
        self.scratch_parent = scratch_parent
        self.started = time.monotonic()
        self.max_runtime = max_runtime
        self.abort_reason = None
        self.io_psi_high_since = None
        self.io_psi_previous_sample = None
        self.io_psi_longest_high_seconds = 0.0
        self.observed_children = {}
        self.stop = threading.Event()
        self.abort = threading.Event()
        self.driver = None
        self.driver_fd = None
        self.driver_identity = None
        self.driver_ref = None
        self.dbpath = None
        self.fixture = None
        self.fixture_fd = None
        self.fixture_identity = None
        self.fixture_ref = None
        self.fixture_abort = None
        self.pidfds = []
        self.cleanup_observations = []
        self.signal_observations = []
        self.samples = []
        self.worker = threading.Thread(target=self.run, daemon=True)

    def start(self):
        self.worker.start()

    def signal_safe_target(self, target, signum):
        if target == "driver":
            reference = self.driver_ref
            if reference is None:
                return "identity_unavailable"
            process, fd, identity = reference
            identities = set(self.observed_children.get(process.pid, set())) if process else set()
        else:
            reference = self.fixture_ref
            if reference is None:
                return "identity_unavailable"
            process, fd, identity = reference
            return signal_pinned(fd, signum)
        if process is None or identity is None or fd is None:
            return "identity_unavailable"
        identities.add(identity)
        observation = group_observation(process.pid, identities)
        self.cleanup_observations.append({"phase": "before_signal", "target": target, "pgid": process.pid, **observation})
        if observation["errors"] or any(item["state"] != "Z" and not item["known_identity"]
                                        for item in observation["members"]):
            return "unconfirmed_group_no_signal"
        same = next((item for item in observation["observed_children"]
                     if item["pid"] == identity[0] and item["starttime"] == identity[1]), None)
        if same is None or same["status"] != "same_identity" or same["pgid"] != process.pid:
            return "target_identity_unconfirmed_no_signal"
        return signal_pinned(fd, signum)

    def run(self):
        while not self.stop.is_set():
            memory = meminfo()
            available = memory.get("MemAvailable", 0)
            swap = memory.get("SwapTotal", 0) - memory.get("SwapFree", 0)
            pid = self.driver.pid if self.driver and self.driver.poll() is None else None
            rows = []
            if pid:
                for child in descendants(pid):
                    fields = helpers.proc_fields(f"/proc/{child}/status")
                    process = process_stat(child)
                    if fields and process is not None:
                        try:
                            name = Path(f"/proc/{child}/comm").read_text().strip()
                        except OSError:
                            name = "unavailable"
                        rows.append({"pid": child, "name": name, "rss_kib": fields.get("VmRSS", 0),
                                     "ticks": process["ticks"], "starttime": process["starttime"]})
                        if child != pid:
                            self.observed_children.setdefault(pid, set()).add((child, process["starttime"]))
            if self.fixture and self.fixture.is_alive():
                for child in descendants(self.fixture.pid):
                    if child != self.fixture.pid:
                        process = process_stat(child)
                        if process is not None:
                            self.observed_children.setdefault(self.fixture.pid, set()).add((child, process["starttime"]))
            try:
                wal_bytes = Path(str(self.dbpath) + "-wal").stat().st_size if self.dbpath else 0
            except OSError:
                wal_bytes = 0
            try:
                scratch_free = shutil.disk_usage(self.scratch_parent).free
            except OSError:
                scratch_free = None
            now = time.monotonic()
            io_psi = io_psi_some_avg10()
            if io_psi is None or io_psi < IO_PSI_THRESHOLD:
                self.io_psi_high_since = None
            elif self.io_psi_high_since is None or (self.io_psi_previous_sample is not None and now - self.io_psi_previous_sample > .5):
                self.io_psi_high_since = now
            high_seconds = now - self.io_psi_high_since if self.io_psi_high_since is not None else 0.0
            self.io_psi_longest_high_seconds = max(self.io_psi_longest_high_seconds, high_seconds)
            self.io_psi_previous_sample = now
            self.samples.append({"time": now, "available_kib": available, "swap_kib": swap,
                                 "processes": rows, "wal_bytes": wal_bytes, "scratch_free_bytes": scratch_free,
                                 "io_psi_some_avg10_percent": io_psi})
            if scratch_free is None:
                self.abort_reason = "scratch free-space lookup failed"
            elif scratch_free < RUNTIME_FREE_BYTES:
                self.abort_reason = "scratch free space fell below 1 GiB"
            elif now - self.started >= self.max_runtime:
                self.abort_reason = "maximum run time exceeded"
            elif high_seconds >= IO_PSI_DURATION_SECONDS:
                self.abort_reason = "host I/O PSI some avg10 at or above 45 percent continuously for 10 seconds"
            elif available < 3 * 1024 * 1024 or swap - self.initial_swap >= 256 * 1024:
                self.abort_reason = "host memory or swap limit exceeded"
            if self.abort_reason:
                self.abort.set()
                if self.fixture_abort:
                    self.fixture_abort.set()
                if self.driver:
                    self.signal_observations.append({"target": "driver", "action": "SIGTERM", "result": self.signal_safe_target("driver", signal.SIGTERM)})
                if self.fixture:
                    self.signal_observations.append({"target": "fixture", "action": "SIGTERM", "result": self.signal_safe_target("fixture", signal.SIGTERM)})
                return
            self.stop.wait(.2)

    def check(self):
        ensure(not self.abort.is_set(), self.abort_reason or "isolated run stopped")

    def finish(self):
        self.stop.set()
        self.worker.join(timeout=5)
        if not self.worker.is_alive():
            for fd in self.pidfds:
                os.close(fd)

    def result(self, start=0):
        samples = self.samples[start:]
        psi_values = [item["io_psi_some_avg10_percent"] for item in samples if item["io_psi_some_avg10_percent"] is not None]
        psi = {"status": "unconfirmed" if not psi_values else ("partially_observed" if len(psi_values) < len(samples) else "observed"),
               "some_avg10_threshold_percent": IO_PSI_THRESHOLD, "continuous_duration_threshold_seconds": IO_PSI_DURATION_SECONDS,
               "sampling_interval_ms": 200, "max_gap_ms_before_reset": 500,
               "valid_samples": len(psi_values), "unconfirmed_samples": len(samples) - len(psi_values),
               "max_some_avg10_percent": max(psi_values, default=None),
               "longest_continuous_high_seconds_overall": self.io_psi_longest_high_seconds,
               "abort_reason": self.abort_reason}
        if len(samples) < 2:
            return {"status": "unmeasured", "scratch_runtime_free_limit_bytes": RUNTIME_FREE_BYTES,
                    "scratch_min_free_bytes": min((item["scratch_free_bytes"] for item in samples if item["scratch_free_bytes"] is not None), default=None),
                    "scratch_free_lookup_failures": sum(item["scratch_free_bytes"] is None for item in samples), "io_psi": psi}
        per_pid = {}
        for sample in samples:
            for process in sample["processes"]:
                data = per_pid.setdefault(process["pid"], {"name": process["name"], "first": process["ticks"],
                                                            "last": process["ticks"], "first_rss_kib": process["rss_kib"], "peak_rss_kib": 0})
                data["last"] = process["ticks"]
                data["last_rss_kib"] = process["rss_kib"]
                data["peak_rss_kib"] = max(data["peak_rss_kib"], process["rss_kib"])
        elapsed = max(.001, samples[-1]["time"] - samples[0]["time"])
        ticks = os.sysconf("SC_CLK_TCK")
        return {"sample_interval_ms": 200, "host_min_available_kib": min(item["available_kib"] for item in samples),
                "host_peak_swap_growth_kib": max(0, max(item["swap_kib"] for item in samples) - self.initial_swap),
                "scratch_runtime_free_limit_bytes": RUNTIME_FREE_BYTES,
                "scratch_min_free_bytes": min((item["scratch_free_bytes"] for item in samples if item["scratch_free_bytes"] is not None), default=None),
                "scratch_free_lookup_failures": sum(item["scratch_free_bytes"] is None for item in samples),
                "io_psi": psi,
                "peak_wal_bytes": max(item["wal_bytes"] for item in samples),
                "processes": [{"name": item["name"], "peak_rss_kib": item["peak_rss_kib"],
                               "first_rss_kib": item["first_rss_kib"], "last_rss_kib": item["last_rss_kib"],
                               "rss_growth_from_first_sample_kib": max(0, item["peak_rss_kib"] - item["first_rss_kib"]),
                               "cpu_core_percent_over_sampled_run": max(0, item["last"] - item["first"]) / ticks / elapsed * 100}
                              for item in per_pid.values()],
                "rss_note": "per-process RSS, shared pages may overlap; 200 ms samples can miss peaks"}


WEBDRIVER_ERROR_CODES = frozenset({"invalid argument", "invalid session id", "javascript error", "no such element",
                                   "no such window", "script timeout", "session not created", "stale element reference",
                                   "timeout", "unexpected alert open", "unknown command", "unknown error",
                                   "unsupported operation", "element not interactable"})


def webdriver_command(method, path):
    if method == "POST" and path.endswith("/execute/sync"):
        return "execute_sync"
    if method == "POST" and path.endswith("/execute/async"):
        return "execute_async"
    if method == "GET" and path == "/status":
        return "status"
    if method == "POST" and path == "/session":
        return "create_session"
    if method == "POST" and path.endswith("/timeouts"):
        return "set_timeouts"
    if method == "DELETE" and path.startswith("/session/"):
        return "delete_session"
    return "other"


def safe_webdriver_code(value):
    code = value.get("error") if isinstance(value, dict) else None
    return code if isinstance(code, str) and code in WEBDRIVER_ERROR_CODES else "other"


class WebDriverFailure(RuntimeError):
    def __init__(self, kind, status, code, command):
        super().__init__("WebDriver command failed")
        self.diagnostic = {"kind": kind, "http_status": status, "error_code": code, "command": command}


class Browser:
    def __init__(self, args, directories, watchdog):
        self.args, self.directories, self.watchdog = args, directories, watchdog
        self.driver = None
        self.session = None
        self.origin = None
        self.last_wd_failure = None
        self.opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))

    def wd(self, method, path, payload=None):
        self.watchdog.check()
        self.last_wd_failure = None
        command = webdriver_command(method, path)
        body = None if payload is None else json.dumps(payload).encode("utf-8")
        request = urllib.request.Request(self.origin + path, data=body, method=method,
                                         headers={"Content-Type": "application/json"})
        try:
            with self.opener.open(request, timeout=15) as response:
                value = json.load(response)["value"]
            return value
        except urllib.error.HTTPError as error:
            self.watchdog.check()
            try:
                response = json.loads(error.read(16384))
                code = safe_webdriver_code(response.get("value") if isinstance(response, dict) else None)
            except (OSError, ValueError):
                code = "unparseable"
            status = error.code if isinstance(error.code, int) and 100 <= error.code <= 599 else None
            failure = WebDriverFailure("http_error", status, code, command)
            self.last_wd_failure = failure.diagnostic
            raise failure from None
        except (OSError, ValueError, KeyError) as error:
            self.watchdog.check()
            kind = "transport_timeout" if isinstance(error, TimeoutError) or isinstance(getattr(error, "reason", None), TimeoutError) else "transport_or_response_error"
            failure = WebDriverFailure(kind, None, "unavailable", command)
            self.last_wd_failure = failure.diagnostic
            raise failure from None

    def js(self, script, *values, asynchronous=False):
        endpoint = "async" if asynchronous else "sync"
        return self.wd("POST", f"/session/{self.session}/execute/{endpoint}", {"script": script, "args": list(values)})

    def start(self):
        port = helpers.free_port()
        self.origin = f"http://127.0.0.1:{port}"
        environment = os.environ.copy()
        environment.update(TAURI_WEBVIEW_AUTOMATION="true", NOTRYX_M0_DATA_DIR=str(self.directories["data"]),
                           XDG_DATA_HOME=str(self.directories["xdg_data"]), XDG_CACHE_HOME=str(self.directories["xdg_cache"]),
                           XDG_CONFIG_HOME=str(self.directories["xdg_config"]))
        read_fd, write_fd = os.pipe()
        launcher = [sys.executable, str(Path(__file__).resolve()), "--driver-launcher", str(os.getpid()), str(read_fd),
                    self.args.ionice_tool, "-c", "3", self.args.nice_tool, "-n", "10",
                    self.args.driver, f"--port={port}", "--host=127.0.0.1"]
        try:
            self.driver = subprocess.Popen(launcher, pass_fds=(read_fd,), env=environment,
                                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                                           start_new_session=True)
        except BaseException:
            os.close(write_fd)
            raise
        finally:
            os.close(read_fd)
        try:
            driver_fd = os.pidfd_open(self.driver.pid)
            self.watchdog.pidfds.append(driver_fd)
            self.watchdog.driver_fd = driver_fd
            leader = process_stat(self.driver.pid)
            ensure(leader is not None, "driver identity unavailable")
            self.watchdog.driver_identity = (self.driver.pid, leader["starttime"])
            self.watchdog.driver_ref = (self.driver, driver_fd, self.watchdog.driver_identity)
            self.watchdog.driver = self.driver
            self.watchdog.check()
            ensure(os.write(write_fd, b"1") == 1, "driver launcher could not be released")
        except (OSError, AttributeError):
            raise RuntimeError("driver pidfd unavailable; isolated data retained") from None
        finally:
            os.close(write_fd)
            self.watchdog.driver = self.driver
        wait(lambda: helpers.leader_running(self.driver) and helpers.driver_owns_port(self.driver.pid, port),
             "driver startup", self.watchdog, 30)
        def driver_ready():
            try:
                return self.wd("GET", "/status")
            except RuntimeError:
                return False
        wait(driver_ready, "driver readiness", self.watchdog, 30)
        capabilities = {"capabilities": {"alwaysMatch": {"browserName": "wry",
                         "webkitgtk:browserOptions": {"binary": str(Path(self.args.binary).resolve())}}}}
        self.session = self.wd("POST", "/session", capabilities)["sessionId"]
        self.wd("POST", f"/session/{self.session}/timeouts", {"script": 60000})
        wait(lambda: self.js("return !!document.querySelector('.workspace-connect button')"),
             "client UI", self.watchdog, 60)
        app_pid = wait(lambda: helpers.client_pid(self.driver.pid, str(Path(self.args.binary).resolve())),
                       "client process", self.watchdog, 30)
        self.priority = {"driver": verify_process_priority(self.driver.pid, self.args.ionice_tool),
                         "client": verify_process_priority(app_pid, self.args.ionice_tool),
                         "descendants": "inherited at launch; later per-process policy changes unverified"}

    def close(self):
        identities = set()
        if self.driver:
            identities.update(self.watchdog.observed_children.get(self.driver.pid, set()).copy())
            if self.watchdog.driver_identity:
                identities.add(self.watchdog.driver_identity)
            for child in descendants(self.driver.pid):
                if child != self.driver.pid:
                    stat = process_stat(child)
                    if stat:
                        identities.add((child, stat["starttime"]))
            self.watchdog.observed_children.setdefault(self.driver.pid, set()).update(identities)
        if self.session:
            try:
                self.wd("DELETE", f"/session/{self.session}")
            except Exception:
                pass
        if self.driver:
            result = self.watchdog.signal_safe_target("driver", signal.SIGTERM)
            self.watchdog.signal_observations.append({"target": "driver", "action": "SIGTERM", "result": result})
            if not wait_group_clear(self.watchdog, self.driver.pid, identities, "browser_wait"):
                raise RuntimeError("isolated browser termination unconfirmed; synthetic data retained")
            self.driver.wait(timeout=1)
        self.watchdog.driver = None
        self.watchdog.driver_fd = None
        self.watchdog.driver_identity = None
        self.watchdog.driver_ref = None
        self.session = None
        self.driver = None


def wait(check, description, watchdog, timeout):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        watchdog.check()
        value = check()
        if value:
            return value
        time.sleep(.1)
    raise RuntimeError(f"timeout: {description}")


def verify_process_priority(pid, ionice_tool):
    try:
        nice = os.getpriority(os.PRIO_PROCESS, pid)
        result = subprocess.run([ionice_tool, "-p", str(pid)], check=True, capture_output=True, text=True, timeout=5)
    except (OSError, subprocess.SubprocessError):
        raise RuntimeError("isolated process priority could not be verified") from None
    ensure(nice >= CPU_NICE_INCREMENT and result.stdout.strip().lower() == "idle",
           "isolated process is not using requested CPU and I/O priority")
    return {"cpu_nice": nice, "io_class": "idle"}


def database_path(directories):
    return directories["data"] / "drafts.sqlite3"


def sqlite_snapshot(path):
    with sqlite3.connect(path.resolve().as_uri() + "?mode=ro", uri=True, timeout=10) as db:
        rows = db.execute("SELECT c.document_id,c.generation,c.bytes,b.bytes,b.revision FROM draft_checkpoints c JOIN draft_bases b USING(document_id) ORDER BY c.document_id").fetchall()
        outbox = db.execute("SELECT operation_id,operation_seq,status,payload_hash,payload_retired,bytes,expected_revision,dependency FROM client_outbox ORDER BY operation_seq").fetchall()
        details = [(ident, seq, status, digest, retired, hashlib.sha256(payload).hexdigest(), revision, dependency)
                   for ident, seq, status, digest, retired, payload, revision, dependency in outbox]
        return {"drafts": [(ident, gen, hashlib.sha256(data).hexdigest(), hashlib.sha256(base).hexdigest(), rev)
                           for ident, gen, data, base, rev in rows],
                "outbox_hash": hashlib.sha256(repr(details).encode()).hexdigest(), "outbox_rows": len(outbox)}


def seed_fixture(path, watchdog):
    oracle = {key: set() for key in QUERIES}
    binding = {"server_origin": "https://workspace.example", "workspace_id": str(uuid.uuid4()),
               "workspace_epoch": str(uuid.uuid4()), "device_id": str(uuid.uuid4()),
               "device_stream_id": str(uuid.uuid4())}
    total_bytes = 0
    recovery_id = doc_id(COUNT + 1)
    with sqlite3.connect(path, timeout=30) as db:
        db.execute("PRAGMA foreign_keys=ON")
        ensure(db.execute("PRAGMA application_id").fetchone()[0] == 1314148952, "unexpected ClientStore application id")
        ensure(db.execute("PRAGMA user_version").fetchone()[0] == 23, "unexpected ClientStore schema version")
        ensure(db.execute("SELECT count(*) FROM client_registrations").fetchone()[0] == 0, "fresh fixture store is not empty")
        db.execute("BEGIN IMMEDIATE")
        try:
            db.execute("INSERT INTO client_registrations(id,server_origin,workspace_id,workspace_epoch,device_id,device_stream_id,status) VALUES(1,?,?,?,?,?,'active')",
                       tuple(binding.values()))
            db.execute("INSERT INTO outbox_streams(registration_id,next_seq,ack_through,confirmed_through) VALUES(1,2,0,0)")
            db.execute("INSERT INTO remote_index(registration_id,root_id,cursor) VALUES(1,?,'1')", (ROOT_ID,))
            db.execute("INSERT INTO remote_nodes(registration_id,node_id,state,content_ready) VALUES(1,?,?,0)",
                       (ROOT_ID, json.dumps(root_state(), ensure_ascii=False)))
            for index in range(COUNT):
                if index % 50 == 0:
                    watchdog.check()
                raw = content(index)
                total_bytes += len(raw)
                node = state(index, raw)
                db.execute("INSERT INTO remote_nodes(registration_id,node_id,state,content_ready) VALUES(1,?,?,1)",
                           (node["node_id"], json.dumps(node, ensure_ascii=False)))
                db.execute("INSERT INTO cached_content(registration_id,node_id,content_rev,content_hash,size,bytes,last_access) VALUES(1,?,?,?,?,?,?)",
                           (node["node_id"], node["content_rev"], node["content_hash"], node["size"], raw, index + 1))
                if index == 2:
                    storage = "r_" + uuid.uuid4().hex
                    draft = dirty_content(raw)
                    db.execute("INSERT INTO registered_documents(registration_id,document_id,storage_id) VALUES(1,?,?)", (node["node_id"], storage))
                    db.execute("INSERT INTO draft_checkpoints(document_id,generation,format_version,bytes) VALUES(?,1,1,?)", (storage, draft))
                    db.execute("INSERT INTO draft_bases(document_id,bytes,revision) VALUES(?,?,?)", (storage, raw, node["content_rev"]))
                    db.execute("""INSERT INTO client_outbox(operation_id,registration_id,operation_seq,operation_kind,
                        document_id,storage_id,generation,expected_revision,bytes,payload_hash,status,payload_len)
                        VALUES(?,1,1,'content',?,?,1,?,?,?,'queued',?)""",
                        (doc_id(COUNT + 2), node["node_id"], storage, node["content_rev"], draft,
                         "sha256:" + hashlib.sha256(draft).hexdigest(), len(draft)))
                    searchable = draft
                else:
                    searchable = raw
                try:
                    text = unicodedata.normalize("NFC", searchable.decode("utf-8"))
                except UnicodeDecodeError:
                    continue
                for key, query in QUERIES.items():
                    if unicodedata.normalize("NFC", query) in text:
                        oracle[key].add(node["node_id"])
            recovery_base = b"synthetic recovery base\n"
            recovery_text = "복구 초안 가나\n".encode()
            deleted = state(COUNT + 1, recovery_base)
            deleted.update(node_id=recovery_id, name="synthetic-deleted.md", deleted=True)
            storage = "r_" + uuid.uuid4().hex
            db.execute("INSERT INTO remote_nodes(registration_id,node_id,state,content_ready) VALUES(1,?,?,0)",
                       (recovery_id, json.dumps(deleted, ensure_ascii=False)))
            db.execute("INSERT INTO registered_documents(registration_id,document_id,storage_id) VALUES(1,?,?)", (recovery_id, storage))
            db.execute("INSERT INTO draft_checkpoints(document_id,generation,format_version,bytes) VALUES(?,1,1,?)", (storage, recovery_text))
            db.execute("INSERT INTO draft_bases(document_id,bytes,revision) VALUES(?,?,?)", (storage, recovery_base, deleted["content_rev"]))
            db.execute("UPDATE cached_content_clock SET last_access=? WHERE singleton=1", (COUNT,))
            ensure(total_bytes <= 500 * MIB, "corpus exceeds 500 MiB")
            db.commit()
        except Exception:
            db.rollback()
            raise
    return binding, oracle, total_bytes, recovery_id


def set_coverage(path, percent, watchdog):
    count = COUNT * percent // 100
    ensure(count >= 4, "coverage omits required fixture documents")
    with sqlite3.connect(path, timeout=30) as db:
        db.execute("PRAGMA foreign_keys=ON")
        db.execute("BEGIN IMMEDIATE")
        try:
            for index in range(count, COUNT):
                if index % 100 == 0:
                    watchdog.check()
                db.execute("DELETE FROM cached_content WHERE registration_id=1 AND node_id=?", (doc_id(index),))
                db.execute("UPDATE remote_nodes SET content_ready=0 WHERE registration_id=1 AND node_id=?", (doc_id(index),))
            db.commit()
        except Exception:
            db.rollback()
            raise
    return count


def verify_fixture(path, expected_cached, watchdog):
    fingerprint = hashlib.sha256()
    with sqlite3.connect(path.resolve().as_uri() + "?mode=ro", uri=True, timeout=30) as db:
        ensure(db.execute("PRAGMA integrity_check").fetchall() == [("ok",)], "fixture integrity check failed")
        ensure(not db.execute("PRAGMA foreign_key_check").fetchall(), "fixture foreign key check failed")
        ensure(db.execute("SELECT count(*) FROM remote_nodes WHERE registration_id=1").fetchone()[0] == COUNT + 2,
               "fixture metadata count differs")
        ensure(db.execute("SELECT count(*) FROM cached_content WHERE registration_id=1").fetchone()[0] == expected_cached,
               "fixture cache count differs")
        for ident, encoded, ready in db.execute("SELECT node_id,state,content_ready FROM remote_nodes WHERE registration_id=1 ORDER BY node_id"):
            fingerprint.update(ident.encode())
            fingerprint.update(encoded.encode())
            fingerprint.update(bytes([ready]))
        for number, row in enumerate(db.execute("SELECT cc.node_id,cc.bytes,cc.content_hash,cc.size,cc.content_rev,n.state,n.content_ready FROM cached_content cc JOIN remote_nodes n ON n.registration_id=cc.registration_id AND n.node_id=cc.node_id WHERE cc.registration_id=1 ORDER BY cc.node_id")):
            if number % 100 == 0:
                watchdog.check()
            ident, raw, digest, size, rev, encoded, ready = row
            node = json.loads(encoded)
            actual = hashlib.sha256(raw).digest()
            ensure(node["node_id"] == ident and node["content_rev"] == rev and node["size"] == size and
                   node["content_hash"] == digest and ready == 1 and len(raw) == int(size) and
                   actual.hex() == digest.removeprefix("sha256:"), "fixture content verification failed")
            fingerprint.update(ident.encode())
            fingerprint.update(actual)
        mismatch = db.execute("SELECT count(*) FROM remote_nodes n LEFT JOIN cached_content c ON c.registration_id=n.registration_id AND c.node_id=n.node_id WHERE n.registration_id=1 AND n.content_ready=1 AND c.node_id IS NULL").fetchone()[0]
        ensure(mismatch == 0, "fixture ready flags disagree with cache")
    return fingerprint.hexdigest()


def sync_setup_diagnostic(browser, binding):
    try:
        return browser.js("""
          const done=arguments[arguments.length-1], expected=arguments[0];
          window.__TAURI_INTERNALS__.invoke('m0_renderer_session').then(owner=>
            window.__TAURI_INTERNALS__.invoke('workspace_sync_status',{request:{renderer_session_id:owner}}))
            .then(sync=>done({outcome:'ok',offline:sync.status==='offline',binding_matches:!!sync.binding&&Object.keys(sync.binding).length===Object.keys(expected).length&&Object.keys(expected).every(key=>sync.binding[key]===expected[key])}))
            .catch(()=>done({outcome:'ipc_rejected',offline:null,binding_matches:null}));
        """, binding, asynchronous=True)
    except Exception:
        return {"outcome": "webdriver_error", "offline": None, "binding_matches": None}


def record_setup_failure(browser, binding, watchdog, setup, error):
    setup["outcome"] = "failed"
    setup["failure_class"] = "timeout" if str(error).startswith("timeout:") else type(error).__name__
    if isinstance(error, WebDriverFailure):
        setup["webdriver_failure"] = error.diagnostic
    if watchdog.abort.is_set():
        setup["sync_diagnostic"] = {"outcome": "skipped_after_abort", "offline": None, "binding_matches": None}
        setup["dom_observation"] = "skipped_after_abort"
        return
    setup["sync_diagnostic"] = sync_setup_diagnostic(browser, binding)
    if watchdog.abort.is_set():
        setup["dom_observation"] = "skipped_after_abort"
        return
    try:
        setup["last_dom"] = search_dom_diagnostic(browser)
    except Exception:
        setup["dom_observation"] = "webdriver_error"


def search_dom_diagnostic(browser, click=False):
    return browser.js("""
      const button=[...document.querySelectorAll('button')].find(b=>b.textContent.trim()==='본문 검색');
      const panel=document.querySelector('[aria-label="본문 검색"]');
      const connect=document.querySelector('.workspace-connect button');
      const markers=[...document.querySelectorAll('nav p')].map(p=>p.textContent.trim());
      const wasExpanded=button?.getAttribute('aria-expanded')==='true';
      let clicked=false;
      if(arguments[0] && button && !button.disabled && !wasExpanded && !panel) {button.click();clicked=true;}
      return {button_present:!!button,button_disabled:!!button?.disabled,expanded:wasExpanded,
              panel_present:!!panel,clicked,document_button_count:document.querySelectorAll('button[data-document]').length,
              draft_button_count:document.querySelectorAll('button[data-draft-document]').length,
              alert_count:document.querySelectorAll('[role="alert"]').length,
              fixture_marker:markers.includes('합성 문서'),workspace_marker:markers.includes('작업 공간 문서'),
              connect_present:!!connect,connect_disabled:!!connect?.disabled};
    """, click)


def initialise_context(browser, binding, watchdog, diagnostics):
    diagnostics["stage"] = "workspace_button_present"
    ready = wait(lambda: browser.js("return [...document.querySelectorAll('button')].some(b=>b.textContent.trim()==='본문 검색')"),
                 "offline workspace UI", watchdog, 120)
    ensure(ready, "offline workspace unavailable")
    def button_ready():
        value = search_dom_diagnostic(browser)
        diagnostics["last_dom"] = value
        return value["button_present"] and not value["button_disabled"]
    diagnostics["stage"] = "workspace_button_enabled"
    wait(button_ready, "workspace loading", watchdog, 120)
    diagnostics["stage"] = "registration"
    value = browser.js("""
      const done=arguments[arguments.length-1], expected=arguments[0];
      window.__TAURI_INTERNALS__.invoke('m0_renderer_session').then(owner=>
        window.__TAURI_INTERNALS__.invoke('workspace_sync_status',{request:{renderer_session_id:owner}})
          .then(sync=>({owner,sync})))
        .then(value=>{window.__perfContext={owner:value.owner,binding:value.sync.binding,seq:1000};const actual=value.sync.binding;done({offline:value.sync.status==='offline',binding_matches:!!actual&&Object.keys(actual).length===Object.keys(expected).length&&Object.keys(expected).every(key=>actual[key]===expected[key])});})
        .catch(()=>done(null));
    """, binding, asynchronous=True)
    diagnostics["registration"] = {"outcome": "ok", **value} if value else {"outcome": "ipc_rejected"}
    ensure(value and value["offline"] and value["binding_matches"], "fixture registration did not open offline")
    diagnostics["stage"] = "inventory"
    inventory = browser.js("""
      const done=arguments[arguments.length-1], c=window.__perfContext;
      window.__TAURI_INTERNALS__.invoke('workspace_inventory',{request:{renderer_session_id:c.owner,after_id:null}})
        .then(page=>done({count:page.nodes.length,has_next:page.next_after_id!==null,root:page.nodes.some(n=>n.node_id===arguments[0])}))
        .catch(()=>done(null));
    """, ROOT_ID, asynchronous=True)
    diagnostics["inventory"] = inventory or {"outcome": "ipc_rejected"}
    ensure(inventory and inventory["root"] and inventory["has_next"], "actual offline inventory rejected fixture tree")


def open_search_panel(browser, watchdog, diagnostics):
    diagnostics["stage"] = "panel"
    diagnostics["panel_observations"] = {"polls": 0, "disabled_polls": 0, "absent_polls": 0, "clicks": 0}
    def probe():
        counts = diagnostics["panel_observations"]
        value = search_dom_diagnostic(browser, click=counts["clicks"] == 0)
        counts["polls"] += 1
        counts["disabled_polls"] += int(value["button_present"] and value["button_disabled"])
        counts["absent_polls"] += int(not value["button_present"])
        counts["clicks"] += int(value["clicked"])
        diagnostics["last_dom"] = value
        return value["panel_present"]
    wait(probe, "search panel enabled and open", watchdog, 120)


def ui_trial(browser, watchdog, query, timeout, diagnostics):
    diagnostics["substage"] = "input_setup"
    browser.js("""
      const panel=document.querySelector('[aria-label="본문 검색"]'), input=panel.querySelector('input'), select=panel.querySelector('select');
      Object.getOwnPropertyDescriptor(HTMLInputElement.prototype,'value').set.call(input,arguments[0]);
      input.dispatchEvent(new Event('input',{bubbles:true}));
      Object.getOwnPropertyDescriptor(HTMLSelectElement.prototype,'value').set.call(select,'workspace');
      select.dispatchEvent(new Event('change',{bubbles:true}));
    """, query)
    diagnostics["substage"] = "form_ready"
    wait(lambda: browser.js("const p=document.querySelector('[aria-label=\"본문 검색\"]');return !!p && !p.querySelector('button[type=submit]').disabled"),
         "search form", watchdog, 15)
    def submit():
        browser.js("""
      const panel=document.querySelector('[aria-label="본문 검색"]');
      const t=window.__uiPerf={started:performance.now(),progress:null,firstResult:null,done:false,error:false,firstPage:null};
      let changed=false, progressPending=false, resultPending=false, finishPending=false;
      const observer=new MutationObserver(()=>{changed=true});
      observer.observe(panel,{subtree:true,childList:true,characterData:true});
      function frame() {
        if(t.done)return;
        const p=document.querySelector('[aria-label="본문 검색"]');
        if(!p){t.error=true;t.done=true;observer.disconnect();return;}
        const busy=[...p.querySelectorAll('[role=status]')].some(e=>e.textContent.trim()==='검색 중');
        const failed=[...p.querySelectorAll('[role=status]')].some(e=>e.textContent.includes('검색을 완료하지 못했습니다'));
        const coverage=[...p.querySelectorAll('p')].find(e=>e.textContent.startsWith('이번 구간에서 검색 가능한 문서:'));
        const row=p.querySelector('[data-search-node]');
        if(busy && t.progress===null && !progressPending) {
          progressPending=true;
          requestAnimationFrame(()=>{
            const current=document.querySelector('[aria-label="본문 검색"]');
            if(current && [...current.querySelectorAll('[role=status]')].some(e=>e.textContent.trim()==='검색 중'))t.progress=performance.now()-t.started;
            progressPending=false;
          });
        }
        if(row && t.firstResult===null && !resultPending) {
          resultPending=true;
          requestAnimationFrame(()=>{
            if(document.querySelector('[aria-label="본문 검색"] [data-search-node]'))t.firstResult=performance.now()-t.started;
            resultPending=false;
          });
        }
        if(failed){t.error=true;t.done=true;observer.disconnect();return;}
        if(changed && !busy && coverage && !finishPending){
          finishPending=true;
          requestAnimationFrame(()=>{t.firstPage=performance.now()-t.started;t.done=true;observer.disconnect();});
        }
        requestAnimationFrame(frame);
      }
      panel.querySelector('form').requestSubmit();
      requestAnimationFrame(frame);
        """)
    try:
        diagnostics["substage"] = "submit_observation"
        submit()
        diagnostics["substage"] = "result_polling"
        result = wait(lambda: browser.js("const t=window.__uiPerf;return t?.done ? {progress:t.progress,first_result:t.firstResult,first_page:t.firstPage,error:t.error}:null"),
                      "UI search first page", watchdog, timeout)
    except RuntimeError as error:
        diagnostics["failure_substage"] = diagnostics["substage"]
        if isinstance(error, WebDriverFailure):
            diagnostics["webdriver_failure"] = error.diagnostic
        if watchdog.abort.is_set():
            diagnostics["cleanup"] = "skipped_after_abort"
        else:
            diagnostics["substage"] = "cleanup"
            try:
                clicked = browser.js("const p=document.querySelector('[aria-label=\"본문 검색\"]');const b=p&&[...p.querySelectorAll('button')].find(x=>x.textContent.trim()==='검색 취소');if(b&&!b.disabled){b.click();return true;}return false;")
                diagnostics["cleanup"] = "requested" if clicked is True else "not_available" if clicked is False else "unconfirmed"
            except Exception as cleanup_error:
                diagnostics["cleanup"] = "failed"
                if isinstance(cleanup_error, WebDriverFailure):
                    diagnostics["cleanup_webdriver_failure"] = cleanup_error.diagnostic
        raise
    diagnostics["substage"] = "result_validation"
    ensure(not result["error"] and result["first_page"] is not None, "UI search failed")
    diagnostics["substage"] = "completed"
    return result


def close_search_panel(browser):
    browser.js("""
      const button=[...document.querySelectorAll('button')].find(b=>b.textContent.trim()==='본문 검색');
      if(button && button.getAttribute('aria-expanded')==='true') button.click();
    """)


def start_page(browser, query, scope, after, request_id):
    browser.js("""
      const c=window.__perfContext, query=arguments[0], scope=arguments[1], after_id=arguments[2], request_id=arguments[3];
      const task=window.__perfTask={started:performance.now(),done:false,result:null,error:false,expectedInvalidation:false,settled:null};
      window.__TAURI_INTERNALS__.invoke('workspace_search',{request:{renderer_session_id:c.owner,binding:c.binding,request_id,query,scope,after_id}})
        .then(value=>{task.result=value},error=>{task.error=true;task.expectedInvalidation=String(error)==='search_invalidated'})
        .finally(()=>{task.settled=performance.now();task.done=true});
    """, query, scope, after, request_id)


def task_state(browser):
    return browser.js("const t=window.__perfTask;return t ? {done:t.done,error:t.error,expected_invalidation:t.expectedInvalidation,result:t.done?t.result:null,elapsed:t.done?t.settled-t.started:null}:null")


def cancel_task(browser, request_id):
    browser.js("""
      const c=window.__perfContext, id=arguments[0];
      const cancel=window.__perfCancel={started:performance.now(),done:false,ok:false,finished:null};
      window.__TAURI_INTERNALS__.invoke('workspace_cancel_search',{request:{renderer_session_id:c.owner,binding:c.binding,request_id:id}})
        .then(()=>{cancel.ok=true},()=>{}).finally(()=>{cancel.finished=performance.now();cancel.done=true});
    """, request_id)


def full_trial(browser, watchdog, query, scope, request_id, timeout):
    start = time.monotonic()
    after = None
    found, available, total, recovery_available, recovery_total = [], 0, 0, 0, 0
    first_ms, native_ms = None, 0.0
    for _ in range(COUNT + 2):
        start_page(browser, query, scope, after, request_id)
        try:
            task = wait(lambda: (value if (value := task_state(browser)) and value["done"] else None),
                        "native search future", watchdog, timeout)
        except RuntimeError:
            cancel_task(browser, request_id)
            raise
        ensure(not task["error"] and task["result"] is not None, "native search failed")
        page = task["result"]
        ensure(page["request_id"] == request_id and len(page["results"]) <= 200, "invalid search response")
        if first_ms is None:
            first_ms = task["elapsed"]
        native_ms += task["elapsed"]
        available += page["available_documents"]
        total += page["total_documents"]
        recovery_available += page["recovery_available_documents"]
        recovery_total += page["recovery_total_documents"]
        for result in page["results"]:
            ensure(result["scope"] == scope and result["source"] in ("draft", "cache"), "invalid result scope")
            found.append(result["node_id"])
        next_after = page["next_after_id"]
        if next_after is None:
            break
        ensure(after is None or next_after > after, "non-advancing search cursor")
        after = next_after
    else:
        raise RuntimeError("search exceeded pagination bound")
    ensure(len(found) == len(set(found)), "duplicate search results")
    return {"ids": set(found), "available": available, "total": total,
            "recovery_available": recovery_available, "recovery_total": recovery_total,
            "first_page_ms": first_ms, "complete_native_future_sum_ms": native_ms,
            "complete_wall_ms": (time.monotonic() - start) * 1000}


def cancellation_trial(browser, watchdog, request_id, timeout):
    start_page(browser, QUERIES["absent"], "workspace", None, request_id)
    time.sleep(.05)
    initial = task_state(browser)
    if not initial or initial["done"]:
        return {"valid": False, "reason": "search completed before cancellation"}
    cancel_task(browser, request_id)
    try:
        cancel = wait(lambda: browser.js("const c=window.__perfCancel;return c?.done?{ok:c.ok,started:c.started,finished:c.finished}:null"),
                      "cancel command", watchdog, timeout)
        task = wait(lambda: (value if (value := task_state(browser)) and value["done"] else None),
                    "cancelled native future", watchdog, timeout)
    except RuntimeError:
        raise RuntimeError("cancelled search future did not settle; isolated run stopped") from None
    raw = browser.js("return {cancel:window.__perfCancel,settled:window.__perfTask.settled}")
    if raw["settled"] <= cancel["started"]:
        return {"valid": False, "reason": "search completed before cancellation was requested"}
    ensure(cancel["ok"], "cancel command failed")
    if not task["error"]:
        return {"valid": False, "reason": "native completion versus cancellation ordering was not observed"}
    ensure(task["expected_invalidation"], "search ended with an unrelated error during cancellation")
    return {"valid": True, "cancel_command_ms": cancel["finished"] - cancel["started"],
            "native_future_ms": raw["settled"] - cancel["started"],
            "limitation": "future settled; SQLite worker connection teardown not directly observed"}


@contextlib.contextmanager
def exclusive_store(path):
    with path.open("rb") as stream:
        try:
            fcntl.flock(stream.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            raise RuntimeError("fixture store is still locked by an application") from None
        try:
            yield
        finally:
            fcntl.flock(stream.fileno(), fcntl.LOCK_UN)


@contextlib.contextmanager
def isolated_directory(parent, watchdog, report):
    path = Path(tempfile.mkdtemp(prefix="notryx-search-perf-", dir=parent))
    try:
        yield str(path)
    finally:
        fixture_running = watchdog.fixture is not None
        browser_running = watchdog.driver is not None
        if fixture_running or browser_running:
            report["retained_synthetic_directory"] = str(path)
        else:
            try:
                shutil.rmtree(path)
            except OSError:
                report["retained_synthetic_directory"] = str(path)
                raise


class FixtureWatchdog:
    def __init__(self, abort):
        self.abort = abort

    def check(self):
        ensure(not self.abort.is_set(), "fixture aborted")


def bind_parent_lifetime(parent_pid):
    libc = ctypes.CDLL(None, use_errno=True)
    ensure(libc.prctl(1, signal.SIGTERM, 0, 0, 0) == 0, "parent-death signal unavailable")
    ensure(os.getppid() == parent_pid, "fixture parent exited")


def set_own_idle_io_priority():
    ensure(platform.machine() == "x86_64" and sys.maxsize > 2**32,
           "fixture I/O priority unsupported on this architecture")
    libc = ctypes.CDLL(None, use_errno=True)
    ensure(libc.syscall(251, 1, 0, 3 << 13) == 0, "fixture I/O priority could not be set")


def driver_launcher(parent_pid, read_fd, command):
    try:
        bind_parent_lifetime(parent_pid)
        ensure(os.read(read_fd, 1) == b"1", "driver identity was not confirmed")
        os.close(read_fd)
        os.execvp(command[0], command)
    except (OSError, RuntimeError):
        raise SystemExit(1) from None


def fixture_worker(sender, operation, path, percent, identity_ready, abort, parent_pid):
    try:
        bind_parent_lifetime(parent_pid)
        os.setsid()
        ensure(identity_ready.wait(timeout=5), "fixture identity was not confirmed")
        watchdog = FixtureWatchdog(abort)
        watchdog.check()
        os.nice(CPU_NICE_INCREMENT)
        ionice_tool = shutil.which("ionice")
        ensure(ionice_tool is not None, "ionice unavailable")
        set_own_idle_io_priority()
        priority = verify_process_priority(os.getpid(), ionice_tool)
        watchdog.check()
        if operation == "seed":
            with exclusive_store(path):
                binding, oracle, total, recovery = seed_fixture(path, watchdog)
                fingerprint = verify_fixture(path, COUNT, watchdog)
            sender.send((True, (binding, oracle, total, recovery, fingerprint, priority)))
        elif operation == "coverage":
            with exclusive_store(path):
                count = set_coverage(path, percent, watchdog)
                fingerprint = verify_fixture(path, count, watchdog)
            sender.send((True, fingerprint))
        elif operation == "verify":
            fingerprint = verify_fixture(path, COUNT * percent // 100, watchdog)
            sender.send((True, fingerprint))
        else:
            sender.send((False, "unknown fixture operation"))
    except Exception as error:
        message = str(error) if isinstance(error, RuntimeError) else type(error).__name__
        try:
            sender.send((False, message))
        except (OSError, BrokenPipeError):
            pass
    finally:
        sender.close()


def run_fixture(operation, path, watchdog, percent=None):
    context = multiprocessing.get_context("spawn")
    receiver, sender = context.Pipe(duplex=False)
    identity_ready = context.Event()
    abort = context.Event()
    watchdog.fixture_abort = abort
    process = context.Process(target=fixture_worker, args=(sender, operation, path, percent, identity_ready, abort, os.getpid()))
    process.start()
    sender.close()
    try:
        fixture_fd = os.pidfd_open(process.pid)
        watchdog.pidfds.append(fixture_fd)
        watchdog.fixture_fd = fixture_fd
        fixture_stat = process_stat(process.pid)
        ensure(fixture_stat is not None, "fixture identity unavailable")
        watchdog.fixture_identity = (process.pid, fixture_stat["starttime"])
        watchdog.fixture_ref = (process, fixture_fd, watchdog.fixture_identity)
    except (OSError, AttributeError):
        raise RuntimeError("fixture pidfd unavailable; isolated data retained") from None
    finally:
        watchdog.fixture = process
    try:
        watchdog.check()
        identity_ready.set()
        while True:
            watchdog.check()
            if receiver.poll(.2):
                ok, value = receiver.recv()
                process.join(timeout=10)
                ensure(ok and process.exitcode == 0, "fixture preparation or verification failed")
                return value
            if not process.is_alive():
                raise RuntimeError("fixture worker exited without a result")
    finally:
        receiver.close()
        if process.is_alive():
            result = watchdog.signal_safe_target("fixture", signal.SIGTERM)
            watchdog.signal_observations.append({"target": "fixture", "action": "SIGTERM", "result": result})
            process.join(timeout=2)
        if process.is_alive():
            raise RuntimeError("fixture worker did not terminate; isolated data retained")
        identities = set(watchdog.observed_children.get(process.pid, set()))
        if watchdog.fixture_identity:
            identities.add(watchdog.fixture_identity)
        if not wait_group_clear(watchdog, process.pid, identities, "fixture_wait"):
            raise RuntimeError("fixture group termination unconfirmed; isolated data retained")
        watchdog.fixture = None
        watchdog.fixture_fd = None
        watchdog.fixture_identity = None
        watchdog.fixture_ref = None
        watchdog.fixture_abort = None


def start_save(browser, document_id, session, generation, encoded):
    browser.js("""
      const t=window.__saveTask={started:performance.now(),done:false,ok:false,finished:null};
      window.__TAURI_INTERNALS__.invoke('workspace_save',{request:{document_id:arguments[0],edit_session_id:arguments[1],generation:arguments[2],bytes_base64:arguments[3]}})
        .then(value=>{t.ok=value.generation===arguments[2]},()=>{}).finally(()=>{t.finished=performance.now();t.done=true});
    """, document_id, session, str(generation), encoded)


def checkpoint_trial(browser, watchdog, path, runs, next_request_id, timeout):
    raw = content(0)
    node = state(0, raw)
    opened = browser.js("""
      const done=arguments[arguments.length-1], c=window.__perfContext;
      window.__TAURI_INTERNALS__.invoke('workspace_open',{request:{renderer_session_id:c.owner,node:arguments[0]}})
        .then(value=>done({id:value.document_id,session:value.edit_session_id,generation:value.generation,readonly:value.readonly}))
        .catch(()=>done(null));
    """, node, asynchronous=True)
    ensure(opened and not opened["readonly"] and opened["id"] == doc_id(0), "5 MiB checkpoint fixture could not open")
    session = opened["session"]
    generation = int(opened["generation"])
    variants = [bytes([letter]) + raw[1:] for letter in (ord("A"), ord("B"))]
    encoded = [base64.b64encode(value).decode("ascii") for value in variants]
    expected_base = hashlib.sha256(raw).hexdigest()
    baseline, concurrent = [], []
    overlap_started = 0
    search_outcomes = {"invalidated": 0, "finished_before_checkpoint": 0,
                       "finished_without_invalidation": 0, "timed_out": 0}
    initial_outbox = sqlite_snapshot(path)["outbox_hash"]
    for phase in ("baseline", "concurrent"):
        for attempt in range(runs):
            watchdog.check()
            request_id = None
            if phase == "concurrent":
                request_id = str(next_request_id())
                start_page(browser, QUERIES["absent"], "workspace", None, request_id)
                time.sleep(.05)
                task = task_state(browser)
                if task and not task["done"]:
                    overlap_started += 1
                else:
                    search_outcomes["finished_before_checkpoint"] += 1
            generation += 1
            choice = (generation + 1) % 2
            start_save(browser, doc_id(0), session, generation, encoded[choice])
            saved = wait(lambda: browser.js("const t=window.__saveTask;return t?.done?{ok:t.ok,ms:t.finished-t.started}:null"),
                         "5 MiB checkpoint", watchdog, timeout)
            ensure(saved["ok"], "5 MiB checkpoint failed")
            (baseline if phase == "baseline" else concurrent).append(saved["ms"])
            with sqlite3.connect(path.resolve().as_uri() + "?mode=ro", uri=True, timeout=10) as db:
                row = db.execute("""SELECT c.generation,c.bytes,b.bytes FROM registered_documents d
                    JOIN draft_checkpoints c ON c.document_id=d.storage_id
                    JOIN draft_bases b ON b.document_id=d.storage_id
                    WHERE d.registration_id=1 AND d.document_id=?""", (doc_id(0),)).fetchone()
            ensure(row and row[0] == generation and hashlib.sha256(row[1]).digest() == hashlib.sha256(variants[choice]).digest()
                   and hashlib.sha256(row[2]).hexdigest() == expected_base, "5 MiB checkpoint or base differs")
            ensure(sqlite_snapshot(path)["outbox_hash"] == initial_outbox, "checkpoint changed the outbox")
            if request_id:
                try:
                    task = wait(lambda: (value if (value := task_state(browser)) and value["done"] else None),
                                "search after checkpoint", watchdog, timeout)
                    if task["error"] and task["expected_invalidation"]:
                        search_outcomes["invalidated"] += 1
                    elif task["error"]:
                        raise RuntimeError("search ended with an unrelated error during checkpoint")
                    else:
                        search_outcomes["finished_without_invalidation"] += 1
                except RuntimeError:
                    if not watchdog.abort.is_set():
                        try:
                            cancel_task(browser, request_id)
                            wait(lambda: (value if (value := task_state(browser)) and value["done"] else None),
                                 "search cancellation after checkpoint", watchdog, timeout)
                        except Exception:
                            # Preserve the measurement failure; outer cleanup stops the browser.
                            pass
                    raise
    closed = browser.js("""
      const done=arguments[arguments.length-1];
      window.__TAURI_INTERNALS__.invoke('workspace_close',{request:{document_id:arguments[0],edit_session_id:arguments[1]}})
        .then(()=>done(true)).catch(()=>done(false));
    """, doc_id(0), session, asynchronous=True)
    ensure(closed, "checkpoint edit session did not close")
    return {"baseline": summary(baseline), "concurrent": summary(concurrent),
            "search_promise_pending_at_checkpoint": overlap_started, "search_outcomes": search_outcomes,
            "limitation": "IPC overlap observed; exact native scan entry and lock wait were not instrumented"}


def executable_hash(path):
    digest = hashlib.sha256()
    with open(path, "rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def run_metadata(binary, driver, filesystem, parent):
    cpu_model = "unavailable"
    try:
        for line in Path("/proc/cpuinfo").read_text().splitlines():
            if line.startswith("model name"):
                cpu_model = line.split(":", 1)[1].strip()
                break
    except OSError:
        pass
    try:
        version = subprocess.run([driver, "--version"], capture_output=True, text=True, timeout=5, check=True).stdout.strip()
    except (OSError, subprocess.SubprocessError):
        version = "unavailable"
    try:
        head = subprocess.run(["git", "rev-parse", "HEAD"], capture_output=True, text=True, timeout=5, check=True).stdout.strip()
        dirty = bool(subprocess.run(["git", "status", "--porcelain"], capture_output=True, text=True, timeout=5, check=True).stdout)
    except (OSError, subprocess.SubprocessError):
        head, dirty = "unavailable", None
    return {"timestamp_utc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
            "kernel": platform.release(), "architecture": platform.machine(), "cpu_model": cpu_model,
            "logical_cpus": os.cpu_count(), "memory_total_kib": meminfo().get("MemTotal"),
            "filesystem_type": filesystem, "filesystem_device_id": os.stat(parent).st_dev,
            "client_binary_sha256": executable_hash(binary), "client_release_version": "unmeasured",
            "webdriver_version": version, "git_head": head, "git_worktree_dirty": dirty}


def assess_targets(report):
    failures = []
    missing_progress = []
    for condition in report["conditions"]:
        coverage = condition["cache_percent"]
        for key, measurement in condition["ui_render_opportunity"].items():
            progress = measurement["progress"]
            if measurement["missing_progress_opportunities"]:
                missing_progress.append({"cache_percent": coverage, "query": key,
                                         "missing": measurement["missing_progress_opportunities"]})
            if progress["count"] == 10 and progress["p95_ms"] > 1000:
                failures.append({"cache_percent": coverage, "query": key,
                                 "criterion": "double-rAF progress opportunity p95 > 1 s; physical paint unmeasured"})
        for key, measurement in condition["queries"].items():
            value = measurement["complete_native_future_sum"]["p95_ms"]
            if measurement["complete_native_future_sum"]["count"] == 10 and value > 30000:
                failures.append({"cache_percent": coverage, "query": key, "criterion": "native future sum p95 > 30 s"})
        cancellation = condition["cancellation"]["native_future"]
        if cancellation["max_ms"] is not None and cancellation["max_ms"] > 500:
            failures.append({"cache_percent": coverage, "criterion": "cancel to native future maximum > 500 ms"})
        if cancellation["count"] == 10 and cancellation["p95_ms"] > 250:
            failures.append({"cache_percent": coverage, "criterion": "cancel to native future p95 > 250 ms"})
        for process in condition.get("resources", {}).get("processes", []):
            if process["name"] == report["client_process_comm"] and process["rss_growth_from_first_sample_kib"] > 256 * 1024:
                failures.append({"cache_percent": coverage, "criterion": "sampled client RSS growth > 256 MiB"})
    report["observed_target_failures"] = failures
    report["unobserved_progress_opportunities"] = missing_progress
    report["gate"] = "failed: at least one measured proposed target exceeded" if failures else \
        "unassessed: slice, physical paint, worker teardown and exact checkpoint overlap remain unmeasured"


@contextlib.contextmanager
def guarded_report(output, current_watchdog):
    try:
        with output.open("x", encoding="utf-8") as report_file:
            yield report_file
    finally:
        watchdog = current_watchdog()
        if watchdog and watchdog.fixture is not None and watchdog.fixture.is_alive():
            os._exit(1)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--scratch-parent", required=True, help="existing local disk directory for isolated temporary data")
    parser.add_argument("--output", required=True, help="new JSON report path, never overwritten")
    parser.add_argument("--driver", default="WebKitWebDriver")
    parser.add_argument("--binary", default="target/debug/notryx-client")
    parser.add_argument("--runs", type=int, default=10)
    parser.add_argument("--timeout", type=int, default=120, help="maximum seconds for each search future")
    parser.add_argument("--max-runtime", type=int, default=1800, help="maximum seconds for the entire isolated run")
    args = parser.parse_args()
    ensure(1 <= args.runs <= 10 and 5 <= args.timeout <= 600 and 60 <= args.max_runtime <= 14400,
           "invalid run count or timeout")
    args.nice_tool = shutil.which("nice")
    args.ionice_tool = shutil.which("ionice")
    ensure(helpers.xvfb_on_current_display(), "an isolated Xvfb display is required")
    parent = Path(args.scratch_parent).resolve(strict=True)
    ensure(parent.is_dir() and os.access(parent, os.W_OK | os.X_OK), "scratch parent is not writable")
    try:
        fs_type = subprocess.run(["stat", "-f", "-c", "%T", str(parent)], check=True,
                                 capture_output=True, text=True, timeout=5).stdout.strip()
    except (OSError, subprocess.SubprocessError):
        raise RuntimeError("scratch filesystem could not be identified") from None
    ensure(fs_type not in ("tmpfs", "ramfs", "nfs", "nfs4", "cifs", "smb2", "fuse.sshfs"),
           "scratch parent must be an ordinary local disk filesystem")
    preflight_free = shutil.disk_usage(parent).free
    ensure(preflight_free >= PREFLIGHT_FREE_BYTES, "scratch filesystem requires at least 2 GiB free")
    binary = Path(args.binary).resolve(strict=True)
    ensure(binary.is_file() and os.access(binary, os.X_OK), "client binary unavailable")
    ensure(shutil.which(args.driver) or Path(args.driver).is_file(), "WebDriver unavailable")
    output = Path(args.output)
    ensure(output.parent.exists(), "report parent does not exist")
    report = {
        "status": "incomplete", "gate": "unassessed", "fixture": "synthetic direct SQLite, actual offline Tauri search",
        "design_contract": {"markdown_documents": COUNT, "max_total_bytes": 500 * MIB,
                            "slice_ms": 100, "max_results_per_page": 200},
        "proposed_targets": {"progress_p95_ms": 1000, "complete_p95_ms": 30000,
                             "cancel_p95_ms": 250, "cancel_max_ms": 500, "rss_growth_mib": 256},
        "measurement_notes": ["p50 and p95 use nearest-rank; ten samples do not establish population guarantees",
                              "native future sum excludes WebDriver inter-page polling; wall time includes it",
                              "first measured query follows fixture validation and is not a cold-cache run",
                              "checkpoint times measure native IPC only, not renderer input or encoding",
                              "exit code 0 means the measurement run completed, not that AT-54 passed"],
        "unmeasured": ["native SQL, BLOB, UTF-8 and NFC per-operation latency",
                       "actual SQLite worker teardown after returned future", "exact checkpoint versus native scan entry overlap",
                       "server bootstrap and cache download", "server source file preservation",
                       "CodeMirror input and renderer Base64 encoding time", "RSS peak between 200 ms samples",
                       "physical or compositor paint completion", "pathological combining sequences longer than 256 marks",
                       "short-lived detached children missed between process samples and their cleanup"],
        "safety": {"preflight_free_bytes": preflight_free, "preflight_free_limit_bytes": PREFLIGHT_FREE_BYTES,
                   "runtime_free_limit_bytes": RUNTIME_FREE_BYTES, "watchdog_interval_ms": 200,
                   "maximum_runtime_seconds": args.max_runtime,
                   "io_psi_some_avg10_limit_percent": IO_PSI_THRESHOLD,
                   "io_psi_continuous_limit_seconds": IO_PSI_DURATION_SECONDS,
                   "io_psi_note": "unknown samples or sampling gaps over 500 ms reset the continuous window; peaks do not accumulate",
                   "priority_note": "isolated processes use increased nice and idle I/O; timing reflects this condition; scheduler and writeback may still affect other workloads",
                   "free_space_note": "sampling and thresholds do not reserve space against concurrent writes"},
        "runs_requested": args.runs, "environment": run_metadata(binary, args.driver, fs_type, parent),
        "client_process_comm": binary.name[:15],
        "conditions": [],
    }
    watchdog = None
    with guarded_report(output, lambda: watchdog) as report_file:
        browser = None
        previous_sigterm = signal.getsignal(signal.SIGTERM)
        def interrupt_run(_signal, _frame):
            raise KeyboardInterrupt
        signal.signal(signal.SIGTERM, interrupt_run)
        try:
            ensure(args.nice_tool and args.ionice_tool, "nice and ionice are required for an isolated run")
            ensure(hasattr(os, "pidfd_open") and hasattr(signal, "pidfd_send_signal"), "pidfd support is required for an isolated run")
            watchdog = Watchdog(parent, args.max_runtime)
            watchdog.start()
            with isolated_directory(parent, watchdog, report) as temporary:
                try:
                    root = Path(temporary)
                    directories = {name: root / name for name in ("data", "xdg_data", "xdg_cache", "xdg_config")}
                    for directory in directories.values():
                        directory.mkdir(mode=0o700)
                    path = database_path(directories)
                    watchdog.dbpath = path
                    print("Preparing isolated ClientStore schema.", flush=True)
                    browser = Browser(args, directories, watchdog)
                    browser.start()
                    report["safety"]["initial_browser_priority"] = browser.priority
                    ensure(path.exists(), "real app did not create its store")
                    browser.close()
                    report["phase"] = {"stage": "fixture_seed"}
                    print("Preparing and verifying synthetic corpus.", flush=True)
                    binding, oracle, total_bytes, recovery_id, fingerprint, fixture_priority = run_fixture("seed", path, watchdog)
                    report["safety"]["fixture_priority"] = fixture_priority
                    report["corpus"] = {"live_markdown_count": COUNT, "deleted_recovery_count": 1,
                                        "raw_live_bytes": total_bytes, "initial_dirty_draft_count": 2,
                                        "non_utf8_count": 1, "editable_large_bytes": LARGE,
                                        "readonly_large_bytes": READONLY, "combining_mark_run": 256,
                                        "cache_conditions": "large documents always retained in all three conditions",
                                        "initial_cache_state": "fixture verification reads content; no cold-cache claim",
                                        "fixture_validation": "SQLite integrity, foreign keys, cached metadata, size and SHA-256"}
                    request_sequence = 1000
                    def next_request_id():
                        nonlocal request_sequence
                        request_sequence += 1
                        return request_sequence
                    for coverage in (100, 50, 10):
                        print(f"Measuring synthetic cache coverage {coverage}%.", flush=True)
                        report["phase"] = {"stage": "prepare_condition", "cache_percent": coverage}
                        if coverage != 100:
                            fingerprint = run_fixture("coverage", path, watchdog, coverage)
                        browser = Browser(args, directories, watchdog)
                        browser.start()
                        setup = {"cache_percent": coverage, "outcome": "in_progress"}
                        report.setdefault("search_setup", []).append(setup)
                        try:
                            initialise_context(browser, binding, watchdog, setup)
                            request_sequence = 1000
                            before = sqlite_snapshot(path)
                            open_search_panel(browser, watchdog, setup)
                            setup["outcome"] = "opened"
                        except Exception as error:
                            record_setup_failure(browser, binding, watchdog, setup, error)
                            raise
                        time.sleep(.4)
                        sample_start = len(watchdog.samples)
                        condition = {"cache_percent": coverage, "available_cache_rows": COUNT * coverage // 100,
                                     "queries": {}, "ui_render_opportunity": {}, "cancellation": {}, "resources": {},
                                     "resource_phases": {},
                                     "priority": browser.priority}
                        report["conditions"].append(condition)
                        phase_start = sample_start
                        for key, query in QUERIES.items():
                            print(f"UI render opportunities: {key}.", flush=True)
                            progress, first_result, first_page = [], [], []
                            for attempt in range(args.runs):
                                report["phase"] = {"stage": "ui", "cache_percent": coverage, "query": key, "run": attempt + 1}
                                try:
                                    measurement = ui_trial(browser, watchdog, query, args.timeout, report["phase"])
                                except WebDriverFailure as error:
                                    report["phase"].setdefault("webdriver_failure", error.diagnostic)
                                    raise
                                if measurement["progress"] is not None:
                                    progress.append(measurement["progress"])
                                if measurement["first_result"] is not None:
                                    first_result.append(measurement["first_result"])
                                first_page.append(measurement["first_page"])
                            condition["ui_render_opportunity"][key] = {"progress": summary(progress), "first_result": summary(first_result),
                                                          "first_page": summary(first_page),
                                                          "missing_progress_opportunities": args.runs-len(progress),
                                                          "limitation": "double requestAnimationFrame, not compositor paint confirmation"}
                        close_search_panel(browser)
                        condition["resource_phases"]["ui_search"] = watchdog.result(phase_start)
                        phase_start = len(watchdog.samples)
                        expected_cached = {doc_id(index) for index in range(COUNT * coverage // 100)}
                        for key, query in QUERIES.items():
                            print(f"Native search futures: {key}.", flush=True)
                            first, full, native = [], [], []
                            expected = oracle[key] & expected_cached
                            for attempt in range(args.runs):
                                report["phase"] = {"stage": "search", "cache_percent": coverage, "query": key, "run": attempt + 1}
                                trial = full_trial(browser, watchdog, query, "workspace", str(next_request_id()), args.timeout)
                                ensure(trial["ids"] == expected, "search results differ from independent NFC oracle")
                                ensure(trial["total"] == COUNT and trial["available"] == len(expected_cached) - 1,
                                       "workspace coverage differs from fixture")
                                ensure(trial["recovery_total"] == 1 and trial["recovery_available"] == 1,
                                       "recovery coverage differs from fixture")
                                first.append(trial["first_page_ms"])
                                full.append(trial["complete_wall_ms"])
                                native.append(trial["complete_native_future_sum_ms"])
                            condition["queries"][key] = {"first_page": summary(first), "complete_wall_including_polling": summary(full),
                                                           "complete_native_future_sum": summary(native),
                                                           "oracle_match_count": len(expected), "oracle_status": "matched"}
                        condition["resource_phases"]["native_search"] = watchdog.result(phase_start)
                        phase_start = len(watchdog.samples)
                        report["phase"] = {"stage": "recovery", "cache_percent": coverage}
                        recovery_times = []
                        for attempt in range(args.runs):
                            report["phase"]["run"] = attempt + 1
                            recovery = full_trial(browser, watchdog, "가나", "recovery", str(next_request_id()), args.timeout)
                            ensure(recovery["ids"] == {recovery_id}, "recovery search differs from oracle")
                            ensure(recovery["recovery_total"] == 1 and recovery["recovery_available"] == 1, "recovery coverage differs")
                            recovery_times.append(recovery["complete_native_future_sum_ms"])
                        condition["recovery"] = {"oracle_status": "matched", "complete_native_future_sum": summary(recovery_times)}
                        cancellations = []
                        for attempt in range(args.runs):
                            report["phase"] = {"stage": "cancellation", "cache_percent": coverage, "run": attempt + 1}
                            cancellations.append(cancellation_trial(browser, watchdog, str(next_request_id()), args.timeout))
                        valid = [item for item in cancellations if item["valid"]]
                        condition["cancellation"] = {"valid_samples": len(valid), "invalid_samples": len(cancellations)-len(valid),
                                                     "command": summary([item["cancel_command_ms"] for item in valid]),
                                                     "native_future": summary([item["native_future_ms"] for item in valid]),
                                                     "teardown": "unmeasured"}
                        ensure(sqlite_snapshot(path) == before, "search changed synthetic drafts, bases or outbox")
                        ensure(run_fixture("verify", path, watchdog, coverage) == fingerprint, "search changed cached bytes or metadata")
                        condition["search_preservation"] = "cache hashes and draft, base and pending outbox bytes unchanged"
                        condition["resource_phases"]["recovery_cancel_verify"] = watchdog.result(phase_start)
                        phase_start = len(watchdog.samples)
                        report["phase"] = {"stage": "checkpoint", "cache_percent": coverage}
                        condition["checkpoint"] = checkpoint_trial(browser, watchdog, path, args.runs, next_request_id, args.timeout)
                        condition["resource_phases"]["checkpoint"] = watchdog.result(phase_start)
                        condition["resources"] = watchdog.result(sample_start)
                        browser.close()
                        browser = None
                    watchdog.check()
                finally:
                    if browser:
                        browser.close()
                        browser = None
            watchdog.check()
            report["status"] = "measured_with_limitations"
            assess_targets(report)
            report["phase"] = {"stage": "completed"}
        except (Exception, KeyboardInterrupt) as error:
            report["status"] = "interrupted_or_failed"
            report["gate"] = "unassessed"
            report["failure"] = str(error) if isinstance(error, RuntimeError) else type(error).__name__
        finally:
            if browser:
                try:
                    browser.close()
                except Exception:
                    report["cleanup"] = "isolated browser did not terminate; synthetic directory retained"
            if watchdog:
                watchdog.finish()
                report["resources_overall"] = watchdog.result()
                report["cleanup_observations"] = watchdog.cleanup_observations
                report["signal_observations"] = watchdog.signal_observations
            else:
                report["resources_overall"] = {"status": "unmeasured"}
            signal.signal(signal.SIGTERM, previous_sigterm)
            json.dump(report, report_file, ensure_ascii=False, indent=2)
            report_file.write("\n")
    print(f"Search performance report status: {report['status']}; AT-54: {report['gate']}", flush=True)
    if report["status"] != "measured_with_limitations":
        raise SystemExit(1)


if __name__ == "__main__":
    if len(sys.argv) > 1 and sys.argv[1] == "--driver-launcher":
        driver_launcher(int(sys.argv[2]), int(sys.argv[3]), sys.argv[4:])
    else:
        main()
