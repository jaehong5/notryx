#!/usr/bin/env python3
"""Isolated WebDriver sessions with independent process and resource monitoring."""
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request


class Browser:
    def __init__(self, perf, helpers, fixture, binary, driver_path, environment):
        self.perf = perf
        self.helpers = helpers
        self.fixture = Path(fixture)
        self.binary = str(Path(binary).resolve())
        self.driver_path = str(driver_path)
        self.environment = dict(environment)
        self.watchdog = None
        self.driver = None
        self.session = None
        self.origin = None
        self.opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
        self.closed = False
        self.cleanup_confirmed = False
        self.peer = None

    def link(self, peer):
        if peer is self or self.closed or peer.closed:
            raise RuntimeError("invalid browser monitor peer")
        self.peer = peer
        peer.peer = self

    def _check_monitor(self):
        if self.watchdog is None:
            raise RuntimeError("browser watchdog not started")
        self.watchdog.check()
        if not self.watchdog.worker.is_alive():
            raise RuntimeError("browser watchdog stopped unexpectedly")

    def _run_monitor(self):
        try:
            self.watchdog.run()
        except BaseException:
            self.watchdog.abort_reason = "browser resource watchdog failed"
            self.watchdog.abort.set()
            try:
                self.watchdog.signal_safe_target("driver", signal.SIGTERM)
            except Exception:
                pass
        finally:
            if not self.watchdog.stop.is_set() and not self.watchdog.abort.is_set():
                self.watchdog.abort_reason = "browser watchdog stopped unexpectedly"
                self.watchdog.abort.set()

    def check(self):
        self._check_monitor()
        if self.peer is not None:
            self.peer._check_monitor()

    def wd(self, method, path, payload=None):
        self.check()
        data = None if payload is None else json.dumps(payload).encode()
        request = urllib.request.Request(
            self.origin + path, data=data, method=method,
            headers={"Content-Type": "application/json"},
        )
        try:
            with self.opener.open(request, timeout=35) as response:
                result = json.load(response)["value"]
            if isinstance(result, dict) and "error" in result:
                raise RuntimeError("WebDriver returned an error")
            self.check()
            return result
        except (OSError, ValueError, KeyError, urllib.error.HTTPError):
            raise RuntimeError("WebDriver request failed") from None

    def js(self, script, *values, asynchronous=False):
        if self.session is None:
            raise RuntimeError("WebDriver session unavailable")
        mode = "async" if asynchronous else "sync"
        return self.wd("POST", f"/session/{self.session}/execute/{mode}",
                       {"script": script, "args": list(values)})

    def _wait(self, predicate, description, timeout=45):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            self.check()
            result = predicate()
            if result:
                return result
            time.sleep(0.1)
        raise RuntimeError("timed out waiting for " + description)

    def start(self):
        if self.watchdog is not None or self.closed:
            raise RuntimeError("browser already started or closed")
        if not hasattr(os, "pidfd_open") or not hasattr(signal, "pidfd_send_signal"):
            raise RuntimeError("pidfd support is required")
        ionice_tool, nice_tool = shutil.which("ionice"), shutil.which("nice")
        if not ionice_tool or not nice_tool:
            raise RuntimeError("idle priority tools are required")
        self.watchdog = self.perf.Watchdog(self.fixture, 300)
        self.watchdog.worker = threading.Thread(target=self._run_monitor, daemon=True)
        self.watchdog.start()
        self.origin = f"http://127.0.0.1:{self.helpers.free_port()}"
        port = int(self.origin.rsplit(":", 1)[1])
        environment = self.environment.copy()
        environment["TAURI_WEBVIEW_AUTOMATION"] = "true"
        read_fd, write_fd = os.pipe()
        launcher = [sys.executable, str(Path(self.perf.__file__).resolve()),
                    "--driver-launcher", str(os.getpid()), str(read_fd),
                    ionice_tool, "-c", "3", nice_tool, "-n", "10",
                    self.driver_path, f"--port={port}", "--host=127.0.0.1"]
        released = False
        try:
            try:
                self.driver = subprocess.Popen(
                    launcher, pass_fds=(read_fd,), env=environment,
                    stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                    start_new_session=True,
                )
            finally:
                os.close(read_fd)
            driver_fd = os.pidfd_open(self.driver.pid)
            self.watchdog.pidfds.append(driver_fd)
            stat = self.perf.process_stat(self.driver.pid)
            if stat is None:
                raise RuntimeError("driver identity unavailable")
            identity = (self.driver.pid, stat["starttime"])
            self.watchdog.driver = self.driver
            self.watchdog.driver_fd = driver_fd
            self.watchdog.driver_identity = identity
            self.watchdog.driver_ref = (self.driver, driver_fd, identity)
            self.check()
            if os.write(write_fd, b"1") != 1:
                raise RuntimeError("driver launcher could not be released")
            released = True
        finally:
            os.close(write_fd)
            if not released and self.driver is not None:
                try:
                    self.driver.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    if self.watchdog.driver_ref is not None:
                        self.watchdog.signal_safe_target("driver", signal.SIGTERM)
                    else:
                        self.driver.terminate()
                    self.driver.wait(timeout=5)
        def driver_ready():
            try:
                return (self.helpers.leader_running(self.driver)
                        and self.helpers.driver_owns_port(self.driver.pid, port)
                        and self.wd("GET", "/status"))
            except RuntimeError:
                return False
        self._wait(driver_ready, "WebDriver startup")
        capabilities = {"capabilities": {"alwaysMatch": {
            "browserName": "wry", "webkitgtk:browserOptions": {"binary": self.binary},
        }}}
        self.session = self.wd("POST", "/session", capabilities)["sessionId"]
        self.wd("POST", f"/session/{self.session}/timeouts", {"script": 30000})
        self._wait(lambda: self.js("return !!document.querySelector('.workspace-connect button')"),
                   "client UI")
        app_pid = self._wait(lambda: self.helpers.client_pid(self.driver.pid, self.binary),
                             "client process")
        self.perf.verify_process_priority(self.driver.pid, ionice_tool)
        self.perf.verify_process_priority(app_pid, ionice_tool)
        return self

    def close(self):
        if self.closed:
            if not self.cleanup_confirmed:
                raise RuntimeError("browser cleanup was not confirmed")
            return
        if self.peer is not None:
            peer = self.peer
            self.peer = None
            if peer.peer is self:
                peer.peer = None
        self.closed = True
        failure = None
        if self.watchdog is None:
            self.cleanup_confirmed = self.driver is None
            if not self.cleanup_confirmed:
                raise RuntimeError("browser identity unavailable; fixture retained")
            return
        try:
            identities = set()
            if self.driver is not None:
                identities.update(self.watchdog.observed_children.get(self.driver.pid, set()).copy())
                if self.watchdog.driver_identity:
                    identities.add(self.watchdog.driver_identity)
                for child in self.perf.descendants(self.driver.pid):
                    stat = self.perf.process_stat(child)
                    if stat is not None:
                        identities.add((child, stat["starttime"]))
                self.watchdog.observed_children.setdefault(self.driver.pid, set()).update(identities)
            if self.session is not None:
                try:
                    self.wd("DELETE", f"/session/{self.session}")
                except Exception:
                    pass
            if self.driver is not None:
                result = self.watchdog.signal_safe_target("driver", signal.SIGTERM)
                if result not in ("sent", "already_exited"):
                    raise RuntimeError("driver identity could not be confirmed; fixture retained")
                if not self.perf.wait_group_clear(self.watchdog, self.driver.pid, identities, "two_client_browser_wait"):
                    raise RuntimeError("browser process termination unconfirmed; fixture retained")
                self.driver.wait(timeout=2)
            self.cleanup_confirmed = True
        except Exception as error:
            failure = error
        finally:
            monitoring_alive = self.watchdog.worker.is_alive()
            self.watchdog.finish()
            if self.watchdog.worker.is_alive():
                self.cleanup_confirmed = False
                failure = RuntimeError("browser watchdog shutdown unconfirmed; fixture retained")
            elif self.watchdog.abort.is_set():
                self.cleanup_confirmed = False
                failure = RuntimeError(self.watchdog.abort_reason or "browser resource watchdog aborted")
            elif not monitoring_alive:
                self.cleanup_confirmed = False
                failure = RuntimeError("browser watchdog stopped unexpectedly; fixture retained")
        if failure is not None:
            raise failure
