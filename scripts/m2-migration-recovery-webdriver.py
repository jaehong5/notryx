#!/usr/bin/env python3
"""Exercise migration-failure recovery with an isolated WAL database and WebView."""

import argparse
import fcntl
import json
import os
from pathlib import Path
import runpy
import sqlite3
import subprocess
import tempfile
import threading
import time
import urllib.error
import urllib.request


helpers = runpy.run_path(str(Path(__file__).with_name("m2-workspace-webdriver.py")))
TestFailure = helpers["TestFailure"]
require = helpers["require"]
free_port = helpers["free_port"]
stop = helpers["stop"]
driver_owns_port = helpers["driver_owns_port"]
respond_to_recovery_dialog = helpers["respond_to_recovery_dialog"]
xvfb_on_current_display = helpers["xvfb_on_current_display"]


def fixture(path):
    db = sqlite3.connect(path)
    require(db.execute("PRAGMA journal_mode = WAL").fetchone()[0] == "wal", "fixture did not enter WAL mode")
    db.execute("PRAGMA wal_autocheckpoint = 0")
    db.executescript("""
      CREATE TABLE draft_checkpoints (document_id TEXT PRIMARY KEY NOT NULL, generation INTEGER NOT NULL, format_version INTEGER NOT NULL, bytes BLOB NOT NULL);
      CREATE TABLE draft_bases (document_id TEXT PRIMARY KEY NOT NULL, bytes BLOB NOT NULL, revision TEXT NOT NULL);
      CREATE TABLE client_registrations (id INTEGER PRIMARY KEY, server_origin TEXT NOT NULL, workspace_id TEXT NOT NULL, workspace_epoch TEXT NOT NULL, device_id TEXT NOT NULL, device_stream_id TEXT NOT NULL, status TEXT NOT NULL);
      CREATE TABLE registered_documents (registration_id INTEGER NOT NULL, document_id TEXT NOT NULL, storage_id TEXT NOT NULL UNIQUE, PRIMARY KEY(registration_id, document_id));
      CREATE TABLE outbox_streams (registration_id INTEGER PRIMARY KEY, next_seq INTEGER NOT NULL, ack_through INTEGER NOT NULL);
      CREATE TABLE client_outbox (operation_id TEXT PRIMARY KEY, registration_id INTEGER NOT NULL, operation_seq INTEGER NOT NULL, document_id TEXT NOT NULL, storage_id TEXT NOT NULL, generation INTEGER NOT NULL, expected_revision TEXT, dependency TEXT, bytes BLOB NOT NULL, payload_hash TEXT NOT NULL, status TEXT NOT NULL, committed_revision TEXT, conflict_revision TEXT, conflict_etag TEXT, abandon_state TEXT NOT NULL DEFAULT 'none');
      PRAGMA application_id = 1314148952;
      PRAGMA user_version = 3;
    """)
    # v3 cannot contain the v4 column: the collision makes the real migration fail.
    db.execute("INSERT INTO client_registrations VALUES (1, ?, ?, ?, ?, ?, 'frozen')", (
        "https://example.invalid", "11111111-1111-4111-8111-111111111111",
        "22222222-2222-4222-8222-222222222222", "33333333-3333-4333-8333-333333333333",
        "44444444-4444-4444-8444-444444444444"))
    db.execute("INSERT INTO registered_documents VALUES (1, 'same-document', 'storage-a')")
    db.execute("INSERT INTO draft_checkpoints VALUES ('storage-a', 7, 1, ?)", (b"\xef\xbb\xbfdraft\r\n",))
    db.execute("INSERT INTO draft_bases VALUES ('storage-a', ?, 'base-revision')", (b"base\r\n",))
    db.execute("INSERT INTO outbox_streams VALUES (1, 2, 0)")
    db.execute("INSERT INTO client_outbox VALUES ('operation-a', 1, 1, 'same-document', 'storage-a', 6, 'base-revision', NULL, ?, 'synthetic-hash', 'sent', NULL, NULL, NULL, 'none')", (b"sent payload\n",))
    db.commit()
    require(Path(str(path) + "-wal").stat().st_size > 0, "fixture has no WAL data")
    return db


def contents(db):
    return {
        "version": db.execute("PRAGMA user_version").fetchone()[0],
        "draft": db.execute("SELECT generation, format_version, bytes FROM draft_checkpoints WHERE document_id='storage-a'").fetchone(),
        "base": db.execute("SELECT bytes, revision FROM draft_bases WHERE document_id='storage-a'").fetchone(),
        "registration": db.execute("SELECT server_origin, workspace_id, workspace_epoch, device_id, device_stream_id, status FROM client_registrations WHERE id=1").fetchone(),
        "document": db.execute("SELECT registration_id, document_id, storage_id FROM registered_documents").fetchone(),
        "outbox": db.execute("SELECT operation_id, registration_id, operation_seq, document_id, storage_id, generation, bytes, status FROM client_outbox").fetchone(),
    }


def wait(check, seconds=20, description="recovery state"):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        result = check()
        if result:
            return result
        time.sleep(0.1)
    raise TestFailure(f"timed out waiting for {description}")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--driver", default="WebKitWebDriver")
    parser.add_argument("--binary", default="target/debug/notryx-client")
    args = parser.parse_args()
    require(xvfb_on_current_display(), "recovery verification requires isolated Xvfb")
    driver = None
    session_id = None
    with tempfile.TemporaryDirectory(prefix="notryx-migration-recovery-") as temporary:
        root = Path(temporary)
        data = root / "client"
        data.mkdir(mode=0o700)
        path = data / "drafts.sqlite3"
        db = fixture(path)
        expected = contents(db)
        port = free_port()
        origin = f"http://127.0.0.1:{port}"
        environment = os.environ.copy()
        environment.update(TAURI_WEBVIEW_AUTOMATION="true", GTK_USE_PORTAL="0", NOTRYX_M0_DATA_DIR=str(data))
        try:
            driver = subprocess.Popen([args.driver, f"--port={port}", "--host=127.0.0.1"], env=environment,
                                      stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True)

            def wd(method, path, payload=None):
                body = None if payload is None else json.dumps(payload).encode()
                request = urllib.request.Request(origin + path, data=body, method=method, headers={"Content-Type": "application/json"})
                try:
                    with urllib.request.urlopen(request, timeout=35) as response:
                        value = json.load(response)["value"]
                    if isinstance(value, dict) and "error" in value:
                        raise TestFailure("WebDriver rejected the recovery app session or request")
                    return value
                except (OSError, ValueError, KeyError, urllib.error.HTTPError):
                    raise TestFailure("WebDriver request failed") from None

            wait(lambda: driver.poll() is None and driver_owns_port(driver.pid, port), description="driver port")
            def ready():
                try:
                    return wd("GET", "/status")
                except TestFailure:
                    return False

            wait(ready, description="driver status")
            capabilities = {"capabilities": {"alwaysMatch": {"browserName": "wry", "webkitgtk:browserOptions": {"binary": str(Path(args.binary).resolve())}}}}
            session_id = wd("POST", "/session", capabilities)["sessionId"]

            def js(script, *values):
                return wd("POST", f"/session/{session_id}/execute/sync", {"script": script, "args": list(values)})

            try:
                wait(lambda: js("return !!document.querySelector('[data-testid=migration-recovery]')"))
            except TestFailure:
                observed = js("return {locked: document.body.textContent.includes('다른 Notryx 인스턴스'), unavailable: document.body.textContent.includes('저장소를 읽을 수 없습니다'), startupError: document.body.textContent.includes('시작 상태 확인 실패'), loading: document.body.textContent.includes('확인하고 있습니다')}")
                raise TestFailure(f"recovery screen not shown: {observed}") from None
            require(not js("return !!document.querySelector('.cm-content[contenteditable=true]')"), "editing was enabled during migration recovery")
            with path.open("r+b") as lock_probe:
                try:
                    fcntl.flock(lock_probe, fcntl.LOCK_EX | fcntl.LOCK_NB)
                except BlockingIOError:
                    pass
                else:
                    fcntl.flock(lock_probe, fcntl.LOCK_UN)
                    raise TestFailure("recovery did not hold the client store writer lock")

            def export(selector, destination):
                failures = []

                def choose():
                    try:
                        respond_to_recovery_dialog(destination)
                    except Exception:
                        failures.append(True)

                worker = threading.Thread(target=choose, daemon=True)
                worker.start()
                js("document.querySelector(arguments[0]).click()", selector)
                worker.join(timeout=20)
                require(not worker.is_alive() and not failures, "recovery dialog did not finish")
                wait(lambda: js("return !document.querySelector(arguments[0]).disabled", selector), seconds=30, description="export completion")
                if destination is not None and not destination.exists():
                    status = js("return [...document.querySelectorAll('[role=status]')].map(e=>e.textContent)")
                    raise TestFailure(f"export file not created; recovery statuses: {status}")

            snapshot = root / "snapshot.sqlite3"
            export("[data-testid=migration-export-store]", snapshot)
            with sqlite3.connect(f"file:{snapshot}?mode=ro", uri=True) as copied:
                require(contents(copied) == expected, "snapshot omitted WAL-backed draft, base, registration, or outbox")
            wait(lambda: js("return !!document.querySelector('[data-testid=migration-export-draft]') && !!document.querySelector('[data-testid=migration-export-outbox]')"), description="recovery item list")
            require(js("return document.body.textContent.includes('22222222-2222-4222-8222-222222222222') && document.body.textContent.includes('frozen')"), "recovery did not distinguish epoch and frozen registration")
            for selector, filename, expected_bytes in (
                ("[data-testid=migration-export-draft]", "draft.md", b"\xef\xbb\xbfdraft\r\n"),
                ("[data-testid=migration-export-base]", "base.md", b"base\r\n"),
                ("[data-testid=migration-export-outbox]", "outbox.md", b"sent payload\n"),
            ):
                destination = root / filename
                export(selector, destination)
                require(destination.read_bytes() == expected_bytes, "an individual recovery export changed the source bytes")
            require(contents(db) == expected, "migration recovery changed the source records")
            print("Migration recovery kept the source, held the writer lock, and exported a WAL-complete snapshot and individual draft, base and outbox bytes")
        finally:
            if session_id:
                try:
                    wd("DELETE", f"/session/{session_id}")
                except TestFailure:
                    pass
            stop(driver)
            db.close()


if __name__ == "__main__":
    try:
        main()
    except TestFailure as error:
        print(f"Migration recovery test failed: {error}")
        raise SystemExit(1) from None
    except Exception as error:
        print(f"Migration recovery test failed unexpectedly: {type(error).__name__}")
        raise SystemExit(1) from None
