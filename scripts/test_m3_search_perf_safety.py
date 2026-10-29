#!/usr/bin/env python3
"""Synthetic safety tests; no real processes, signals, or workload are used."""
import importlib.util
import io
import json
from pathlib import Path
import signal
import types
import unittest
import urllib.error
from unittest import mock


SCRIPT = Path(__file__).with_name("m3-search-perf-webdriver.py")
spec = importlib.util.spec_from_file_location("m3_search_perf_safety", SCRIPT)
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


def snapshot(members=(), children=(), errors=()):
    return {"members": list(members), "observed_children": list(children), "errors": list(errors)}


def member(pid, started, state, known=True):
    return {"pid": pid, "starttime": started, "state": state, "pgid": 100, "known_identity": known}


def child(pid, started, state, status="same_identity"):
    return {"pid": pid, "starttime": started, "state": state, "pgid": 100, "status": status}


class CleanupSafety(unittest.TestCase):
    def setUp(self):
        self.watchdog = object.__new__(module.Watchdog)
        process = types.SimpleNamespace(pid=100)
        self.watchdog.driver_ref = (process, 8, (100, 700))
        self.watchdog.fixture_ref = None
        self.watchdog.observed_children = {100: {(101, 701)}}
        self.watchdog.cleanup_observations = []

    def test_group_scan_identifies_zombie_and_starttime(self):
        fields = ["Z", "1", "100"] + ["0"] * 16 + ["701"]
        def read(path, *args, **kwargs):
            if str(path) == "/proc/mounts":
                return "proc /proc proc rw,relatime 0 0\n"
            if str(path) == "/proc/101/stat":
                return "101 (synthetic) " + " ".join(fields)
            raise FileNotFoundError
        with mock.patch.object(Path, "read_text", autospec=True, side_effect=read), \
             mock.patch.object(Path, "iterdir", autospec=True, return_value=iter([Path("/proc/101")])):
            observation = module.group_observation(100, {(101, 701)})
        self.assertEqual(observation["members"], [member(101, 701, "Z")])
        self.assertTrue(module.observation_clear(observation))

    def test_group_scan_reports_unreadable_proc_entry(self):
        def read(path, *args, **kwargs):
            if str(path) == "/proc/mounts":
                return "proc /proc proc rw,relatime 0 0\n"
            raise PermissionError
        with mock.patch.object(Path, "read_text", autospec=True, side_effect=read), \
             mock.patch.object(Path, "iterdir", autospec=True, return_value=iter([Path("/proc/101")])):
            observation = module.group_observation(100, {(101, 701)})
        self.assertEqual(observation["errors"], ["proc_entry_unreadable"])
        self.assertFalse(module.observation_clear(observation))

    def test_zombie_only_and_observed_detached_zombie_are_clear(self):
        observation = snapshot([member(101, 701, "Z")], [child(101, 701, "Z")])
        self.assertTrue(module.observation_clear(observation))

    def test_unknown_live_group_member_prevents_signal_and_cleanup(self):
        observation = snapshot([member(100, 700, "S"), member(202, 900, "S", False)],
                               [child(100, 700, "S")])
        self.assertFalse(module.observation_clear(observation))
        with mock.patch.object(module, "group_observation", return_value=observation), \
             mock.patch.object(module, "signal_pinned") as send:
            self.assertEqual(self.watchdog.signal_safe_target("driver", signal.SIGTERM),
                             "unconfirmed_group_no_signal")
            send.assert_not_called()

    def test_reused_pid_prevents_signal_to_replacement(self):
        observation = snapshot([member(100, 999, "S", False)],
                               [child(100, 700, "S", "pid_reused")])
        with mock.patch.object(module, "group_observation", return_value=observation), \
             mock.patch.object(module, "signal_pinned") as send:
            self.assertEqual(self.watchdog.signal_safe_target("driver", signal.SIGTERM),
                             "unconfirmed_group_no_signal")
            send.assert_not_called()

    def test_unreadable_proc_blocks_signal_and_cleanup(self):
        observation = snapshot(errors=["proc_entry_unreadable"])
        self.assertFalse(module.observation_clear(observation))
        with mock.patch.object(module, "group_observation", return_value=observation), \
             mock.patch.object(module, "signal_pinned") as send:
            self.assertEqual(self.watchdog.signal_safe_target("driver", signal.SIGTERM),
                             "unconfirmed_group_no_signal")
            send.assert_not_called()

    def test_group_wait_is_bounded_and_does_not_signal(self):
        observation = snapshot([member(202, 900, "S", False)])
        now = [0.0]
        def clock():
            return now[0]
        def sleep(seconds):
            now[0] += seconds
        with mock.patch.object(module, "group_observation", return_value=observation), \
             mock.patch.object(module.time, "monotonic", side_effect=clock), \
             mock.patch.object(module.time, "sleep", side_effect=sleep), \
             mock.patch.object(module, "signal_pinned") as send:
            self.assertFalse(module.wait_group_clear(self.watchdog, 100, {(100, 700)}, "test"))
            self.assertGreaterEqual(now[0], 2)
            self.assertLess(now[0], 2.2)
            send.assert_not_called()

    def test_group_wait_requires_two_clear_observations(self):
        now = [0.0]
        def sleep(seconds):
            now[0] += seconds
        with mock.patch.object(module, "group_observation", return_value=snapshot()) as observe, \
             mock.patch.object(module.time, "monotonic", side_effect=lambda: now[0]), \
             mock.patch.object(module.time, "sleep", side_effect=sleep):
            self.assertTrue(module.wait_group_clear(self.watchdog, 100, {(100, 700)}, "test"))
            self.assertEqual(observe.call_count, 2)

    def test_pidfd_unavailable_never_signals(self):
        with mock.patch.object(module.signal, "pidfd_send_signal") as send:
            self.assertEqual(module.signal_pinned(None, signal.SIGTERM), "identity_unavailable")
            send.assert_not_called()
        self.watchdog.driver_ref = None
        with mock.patch.object(module, "signal_pinned") as send:
            self.assertEqual(self.watchdog.signal_safe_target("driver", signal.SIGTERM), "identity_unavailable")
            send.assert_not_called()

    def test_pinned_signal_error_is_reported(self):
        with mock.patch.object(module.signal, "pidfd_send_signal", side_effect=PermissionError(1, "denied")):
            self.assertEqual(module.signal_pinned(8, signal.SIGTERM), "error_1")

    def test_unconfirmed_fixture_preserves_directory_even_when_leader_exited(self):
        fake_watchdog = types.SimpleNamespace(fixture=object(), driver=None)
        report = {}
        with mock.patch.object(module.tempfile, "mkdtemp", return_value="/synthetic/test"), \
             mock.patch.object(module.shutil, "rmtree") as remove:
            with module.isolated_directory(Path("/synthetic"), fake_watchdog, report):
                pass
            remove.assert_not_called()
        self.assertEqual(report["retained_synthetic_directory"], "/synthetic/test")

    def test_fixture_does_not_seed_without_identity_confirmation(self):
        sender = mock.Mock()
        event = mock.Mock()
        event.wait.return_value = False
        with mock.patch.object(module, "bind_parent_lifetime"), \
             mock.patch.object(module.os, "setsid"), \
             mock.patch.object(module, "seed_fixture") as seed, \
             mock.patch.object(module.os, "nice") as nice:
            module.fixture_worker(sender, "seed", Path("/synthetic"), None, event, mock.Mock(), 123)
        seed.assert_not_called()
        nice.assert_not_called()
        sender.send.assert_called_once_with((False, "fixture identity was not confirmed"))
        sender.close.assert_called_once()

    def test_fixture_rejects_parent_death_before_setup(self):
        sender = mock.Mock()
        with mock.patch.object(module, "bind_parent_lifetime", side_effect=RuntimeError("fixture parent exited")), \
             mock.patch.object(module.os, "setsid") as setsid, \
             mock.patch.object(module, "seed_fixture") as seed:
            module.fixture_worker(sender, "seed", Path("/synthetic"), None, mock.Mock(), mock.Mock(), 123)
        setsid.assert_not_called()
        seed.assert_not_called()
        sender.send.assert_called_once_with((False, "fixture parent exited"))

    def test_parent_death_binding_checks_prctl_and_parent_identity(self):
        libc = mock.Mock()
        libc.prctl.return_value = 0
        with mock.patch.object(module.ctypes, "CDLL", return_value=libc), \
             mock.patch.object(module.os, "getppid", return_value=124):
            with self.assertRaisesRegex(RuntimeError, "fixture parent exited"):
                module.bind_parent_lifetime(123)
        libc.prctl.assert_called_once_with(1, signal.SIGTERM, 0, 0, 0)
        libc.prctl.return_value = -1
        with mock.patch.object(module.ctypes, "CDLL", return_value=libc), \
             mock.patch.object(module.os, "getppid", return_value=123):
            with self.assertRaisesRegex(RuntimeError, "parent-death signal unavailable"):
                module.bind_parent_lifetime(123)

    def test_fixture_abort_after_release_prevents_seed(self):
        sender = mock.Mock()
        ready = mock.Mock()
        ready.wait.return_value = True
        abort = mock.Mock()
        abort.is_set.return_value = True
        with mock.patch.object(module, "bind_parent_lifetime"), \
             mock.patch.object(module.os, "setsid"), \
             mock.patch.object(module, "seed_fixture") as seed:
            module.fixture_worker(sender, "seed", Path("/synthetic"), None, ready, abort, 123)
        seed.assert_not_called()
        sender.send.assert_called_once_with((False, "fixture aborted"))

    def test_fixture_signal_does_not_depend_on_group_setup(self):
        self.watchdog.fixture_ref = (types.SimpleNamespace(pid=102), 9, (102, 702))
        with mock.patch.object(module, "group_observation") as observe, \
             mock.patch.object(module, "signal_pinned", return_value="sent") as send:
            self.assertEqual(self.watchdog.signal_safe_target("fixture", signal.SIGTERM), "sent")
            send.assert_called_once_with(9, signal.SIGTERM)
            observe.assert_not_called()

    def test_driver_launcher_never_executes_without_confirmation(self):
        with mock.patch.object(module, "bind_parent_lifetime"), \
             mock.patch.object(module.os, "read", return_value=b""), \
             mock.patch.object(module.os, "execvp") as execute:
            with self.assertRaises(SystemExit):
                module.driver_launcher(123, 9, ["synthetic-driver"])
            execute.assert_not_called()

    def test_idle_priority_targets_only_current_process(self):
        libc = mock.Mock()
        libc.syscall.return_value = 0
        with mock.patch.object(module.platform, "machine", return_value="x86_64"), \
             mock.patch.object(module.ctypes, "CDLL", return_value=libc):
            module.set_own_idle_io_priority()
        libc.syscall.assert_called_once_with(251, 1, 0, 3 << 13)

    def test_idle_priority_rejects_unknown_architecture_or_syscall_error(self):
        libc = mock.Mock()
        libc.syscall.return_value = -1
        with mock.patch.object(module.platform, "machine", return_value="other"), \
             mock.patch.object(module.ctypes, "CDLL", return_value=libc):
            with self.assertRaisesRegex(RuntimeError, "unsupported"):
                module.set_own_idle_io_priority()
        libc.syscall.assert_not_called()
        with mock.patch.object(module.platform, "machine", return_value="x86_64"), \
             mock.patch.object(module.ctypes, "CDLL", return_value=libc):
            with self.assertRaisesRegex(RuntimeError, "could not be set"):
                module.set_own_idle_io_priority()

    def test_report_close_failure_with_live_fixture_exits_without_join(self):
        context = mock.MagicMock()
        context.__exit__.side_effect = OSError("synthetic close failure")
        output = mock.Mock()
        output.open.return_value = context
        fixture = mock.Mock()
        fixture.is_alive.return_value = True
        watchdog = types.SimpleNamespace(fixture=fixture)
        with mock.patch.object(module.os, "_exit", side_effect=SystemExit(1)) as immediate_exit:
            with self.assertRaises(SystemExit):
                with module.guarded_report(output, lambda: watchdog):
                    pass
        immediate_exit.assert_called_once_with(1)
        fixture.join.assert_not_called()

    def test_report_close_failure_without_live_fixture_is_reported(self):
        context = mock.MagicMock()
        context.__exit__.side_effect = OSError("synthetic close failure")
        output = mock.Mock()
        output.open.return_value = context
        watchdog = types.SimpleNamespace(fixture=None)
        with mock.patch.object(module.os, "_exit") as immediate_exit:
            with self.assertRaisesRegex(OSError, "synthetic close failure"):
                with module.guarded_report(output, lambda: watchdog):
                    pass
        immediate_exit.assert_not_called()

    def test_search_panel_waits_for_enabled_and_clicks_only_once(self):
        states = iter([
            {"button_present": True, "button_disabled": True, "panel_present": False, "clicked": False},
            {"button_present": True, "button_disabled": False, "panel_present": False, "clicked": True},
            {"button_present": True, "button_disabled": False, "panel_present": False, "clicked": False},
            {"button_present": True, "button_disabled": False, "panel_present": True, "clicked": False},
        ])
        requested = []
        def observe(_browser, click=False):
            requested.append(click)
            return next(states)
        now = [0.0]
        def sleep(seconds):
            now[0] += seconds
        diagnostics = {}
        with mock.patch.object(module, "search_dom_diagnostic", side_effect=observe), \
             mock.patch.object(module.time, "monotonic", side_effect=lambda: now[0]), \
             mock.patch.object(module.time, "sleep", side_effect=sleep):
            module.open_search_panel(mock.Mock(), mock.Mock(), diagnostics)
        self.assertEqual(requested, [True, True, False, False])
        self.assertEqual(diagnostics["panel_observations"], {"polls": 4, "disabled_polls": 1, "absent_polls": 0, "clicks": 1})

    def test_search_panel_accepts_existing_panel_without_click(self):
        value = {"button_present": True, "button_disabled": False, "panel_present": True, "clicked": False}
        diagnostics = {}
        with mock.patch.object(module, "search_dom_diagnostic", return_value=value):
            module.open_search_panel(mock.Mock(), mock.Mock(), diagnostics)
        self.assertEqual(diagnostics["panel_observations"]["clicks"], 0)

    def test_search_panel_timeout_keeps_last_sanitized_observation(self):
        value = {"button_present": True, "button_disabled": True, "panel_present": False,
                 "clicked": False, "document_button_count": 0, "fixture_marker": False,
                 "workspace_marker": True}
        def time_out(probe, *_args):
            probe()
            raise RuntimeError("timeout: synthetic")
        diagnostics = {}
        with mock.patch.object(module, "search_dom_diagnostic", return_value=value), \
             mock.patch.object(module, "wait", side_effect=time_out):
            with self.assertRaisesRegex(RuntimeError, "timeout"):
                module.open_search_panel(mock.Mock(), mock.Mock(), diagnostics)
        self.assertEqual(diagnostics["last_dom"], value)
        self.assertEqual(diagnostics["panel_observations"]["disabled_polls"], 1)

    def test_setup_rejects_binding_mismatch_without_inventory(self):
        browser = mock.Mock()
        browser.js.side_effect = [True, {"offline": True, "binding_matches": False}]
        dom = {"button_present": True, "button_disabled": False}
        diagnostics = {}
        with mock.patch.object(module, "search_dom_diagnostic", return_value=dom):
            with self.assertRaisesRegex(RuntimeError, "registration"):
                module.initialise_context(browser, {"synthetic": True}, mock.Mock(), diagnostics)
        self.assertEqual(diagnostics["registration"], {"outcome": "ok", "offline": True, "binding_matches": False})
        self.assertNotIn("inventory", diagnostics)

    def test_setup_sync_diagnostic_classifies_ipc_failure_without_detail(self):
        browser = mock.Mock()
        browser.js.side_effect = RuntimeError("synthetic private text")
        value = module.sync_setup_diagnostic(browser, {"synthetic": True})
        self.assertEqual(value, {"outcome": "webdriver_error", "offline": None, "binding_matches": None})

    def test_setup_failure_does_not_observe_after_abort(self):
        browser = mock.Mock()
        watchdog = mock.Mock()
        watchdog.abort.is_set.return_value = True
        setup = {}
        module.record_setup_failure(browser, {}, watchdog, setup, RuntimeError("timeout: synthetic"))
        browser.js.assert_not_called()
        self.assertEqual(setup["sync_diagnostic"], {"outcome": "skipped_after_abort", "offline": None, "binding_matches": None})
        self.assertEqual(setup["dom_observation"], "skipped_after_abort")

    def test_setup_failure_skips_dom_if_abort_occurs_during_sync(self):
        watchdog = mock.Mock()
        watchdog.abort.is_set.side_effect = [False, True]
        setup = {}
        with mock.patch.object(module, "sync_setup_diagnostic", return_value={"outcome": "webdriver_error", "offline": None, "binding_matches": None}), \
             mock.patch.object(module, "search_dom_diagnostic") as dom:
            module.record_setup_failure(mock.Mock(), {}, watchdog, setup, RuntimeError("synthetic"))
        dom.assert_not_called()
        self.assertEqual(setup["dom_observation"], "skipped_after_abort")

    def test_dom_probe_uses_fixed_markers_and_no_output_text(self):
        browser = mock.Mock()
        browser.js.return_value = {"fixture_marker": True, "workspace_marker": False}
        self.assertEqual(module.search_dom_diagnostic(browser), {"fixture_marker": True, "workspace_marker": False})
        script = browser.js.call_args.args[0]
        self.assertIn("!button.disabled", script)
        self.assertIn("markers.includes('합성 문서')", script)
        self.assertIn("markers.includes('작업 공간 문서')", script)
        self.assertNotIn("textContent:", script)

    def make_browser(self, value):
        browser = object.__new__(module.Browser)
        browser.watchdog = mock.Mock()
        browser.origin = "http://127.0.0.1:1"
        browser.last_wd_failure = None
        browser.opener = mock.Mock()
        response = mock.MagicMock()
        response.__enter__.return_value = io.BytesIO(json.dumps({"value": value}).encode())
        browser.opener.open.return_value = response
        return browser

    def test_webdriver_success_preserves_error_false_in_ui_or_task_result(self):
        value = {"error": False, "done": True, "first_page": 20}
        browser = self.make_browser(value)
        self.assertEqual(browser.wd("POST", "/session/synthetic/execute/sync"), value)
        self.assertIsNone(browser.last_wd_failure)

    def test_webdriver_success_preserves_arbitrary_error_string(self):
        value = {"error": "synthetic application value"}
        browser = self.make_browser(value)
        self.assertEqual(browser.wd("POST", "/session/synthetic/execute/sync"), value)

    def test_webdriver_http_error_records_only_allowlisted_metadata(self):
        browser = self.make_browser(None)
        body = json.dumps({"value": {"error": "javascript error", "message": "synthetic secret"}}).encode()
        browser.opener.open.side_effect = urllib.error.HTTPError("http://synthetic", 500, "synthetic secret", {}, io.BytesIO(body))
        with self.assertRaises(module.WebDriverFailure) as raised:
            browser.wd("POST", "/session/sensitive-id/execute/sync")
        self.assertEqual(raised.exception.diagnostic, {"kind": "http_error", "http_status": 500,
                                                     "error_code": "javascript error", "command": "execute_sync"})
        self.assertNotIn("synthetic secret", repr(raised.exception.diagnostic))
        self.assertNotIn("sensitive-id", repr(raised.exception.diagnostic))

    def test_webdriver_http_error_unknown_code_is_redacted(self):
        browser = self.make_browser(None)
        body = json.dumps({"value": {"error": "synthetic private detail"}}).encode()
        browser.opener.open.side_effect = urllib.error.HTTPError("http://synthetic", 400, "private", {}, io.BytesIO(body))
        with self.assertRaises(module.WebDriverFailure) as raised:
            browser.wd("POST", "/session/sensitive-id/execute/async")
        self.assertEqual(raised.exception.diagnostic["error_code"], "other")
        self.assertEqual(raised.exception.diagnostic["command"], "execute_async")

    def test_webdriver_transport_timeout_has_fixed_classification(self):
        browser = self.make_browser(None)
        browser.opener.open.side_effect = TimeoutError("synthetic private detail")
        with self.assertRaises(module.WebDriverFailure) as raised:
            browser.wd("POST", "/session/sensitive-id/execute/sync")
        self.assertEqual(raised.exception.diagnostic, {"kind": "transport_timeout", "http_status": None,
                                                     "error_code": "unavailable", "command": "execute_sync"})

    def test_setup_failure_preserves_original_webdriver_error_before_diagnostics(self):
        error = module.WebDriverFailure("http_error", 404, "invalid session id", "execute_sync")
        watchdog = mock.Mock()
        watchdog.abort.is_set.return_value = False
        setup = {}
        with mock.patch.object(module, "sync_setup_diagnostic", return_value={"outcome": "webdriver_error", "offline": None, "binding_matches": None}), \
             mock.patch.object(module, "search_dom_diagnostic", side_effect=RuntimeError("synthetic")):
            module.record_setup_failure(mock.Mock(), {}, watchdog, setup, error)
        self.assertEqual(setup["webdriver_failure"], error.diagnostic)
        self.assertEqual(setup["dom_observation"], "webdriver_error")

    def test_ui_trial_preserves_primary_failure_if_cleanup_also_fails(self):
        primary = module.WebDriverFailure("http_error", 500, "javascript error", "execute_sync")
        cleanup = module.WebDriverFailure("http_error", 404, "invalid session id", "execute_sync")
        browser = mock.Mock()
        browser.js.side_effect = [None, primary, cleanup]
        watchdog = mock.Mock()
        watchdog.abort.is_set.return_value = False
        diagnostics = {}
        with mock.patch.object(module, "wait", return_value=True):
            with self.assertRaises(module.WebDriverFailure) as raised:
                module.ui_trial(browser, watchdog, "synthetic query", 1, diagnostics)
        self.assertIs(raised.exception, primary)
        self.assertEqual(diagnostics["failure_substage"], "submit_observation")
        self.assertEqual(diagnostics["substage"], "cleanup")
        self.assertEqual(diagnostics["webdriver_failure"], primary.diagnostic)
        self.assertEqual(diagnostics["cleanup_webdriver_failure"], cleanup.diagnostic)
        self.assertNotIn("synthetic query", repr(diagnostics))

    def test_ui_trial_skips_cleanup_after_watchdog_abort(self):
        primary = module.WebDriverFailure("http_error", 500, "javascript error", "execute_sync")
        browser = mock.Mock()
        browser.js.side_effect = [None, primary]
        watchdog = mock.Mock()
        watchdog.abort.is_set.return_value = True
        diagnostics = {}
        with mock.patch.object(module, "wait", return_value=True):
            with self.assertRaises(module.WebDriverFailure):
                module.ui_trial(browser, watchdog, "synthetic query", 1, diagnostics)
        self.assertEqual(browser.js.call_count, 2)
        self.assertEqual(diagnostics["cleanup"], "skipped_after_abort")

    def test_ui_trial_reports_cleanup_button_unavailable(self):
        primary = module.WebDriverFailure("http_error", 500, "javascript error", "execute_sync")
        browser = mock.Mock()
        browser.js.side_effect = [None, primary, False]
        watchdog = mock.Mock()
        watchdog.abort.is_set.return_value = False
        diagnostics = {}
        with mock.patch.object(module, "wait", return_value=True):
            with self.assertRaises(module.WebDriverFailure):
                module.ui_trial(browser, watchdog, "synthetic query", 1, diagnostics)
        self.assertEqual(diagnostics["cleanup"], "not_available")


if __name__ == "__main__":
    unittest.main()
