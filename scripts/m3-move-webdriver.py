#!/usr/bin/env python3
"""Verify online Markdown rename and move with synthetic data in an isolated WebView."""
import argparse
import importlib.util
import json
import os
from pathlib import Path
import re
import select
import shutil
import signal
import sqlite3
import stat
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
import uuid


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--repo", default=str(Path(__file__).resolve().parents[1]))
    parser.add_argument("--driver", default="WebKitWebDriver")
    parser.add_argument("--binary", default="target/debug/notryx-client")
    parser.add_argument("--server", default="target/debug/notryx-server")
    args = parser.parse_args()
    repo = Path(args.repo).resolve()
    spec = importlib.util.spec_from_file_location("m2_helpers", repo / "scripts/m2-workspace-webdriver.py")
    if spec is None or spec.loader is None:
        raise RuntimeError("WebDriver helpers missing")
    helpers = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(helpers)
    perf_spec = importlib.util.spec_from_file_location("m3_perf_helpers", repo / "scripts/m3-search-perf-webdriver.py")
    if perf_spec is None or perf_spec.loader is None:
        raise RuntimeError("resource watchdog missing")
    perf = importlib.util.module_from_spec(perf_spec)
    perf_spec.loader.exec_module(perf)
    require, TestFailure = helpers.require, helpers.TestFailure
    require(helpers.xvfb_on_current_display(), "isolated Xvfb required")
    prefix = Path(os.environ.get("NOTRYX_TEST_SECRET_BUS_PREFIX", ""))
    address = os.environ.get("DBUS_SESSION_BUS_ADDRESS", "")
    require(prefix.parent == Path("/tmp") and re.fullmatch(r"notryx-secret-test\.[A-Za-z0-9]{8}", prefix.name) is not None
        and not prefix.is_symlink() and prefix.is_dir() and prefix.stat().st_uid == os.getuid()
        and stat.S_IMODE(prefix.stat().st_mode) == 0o700 and not (prefix / "bus.sock").is_symlink()
        and (prefix / "bus.sock").is_socket()
        and re.fullmatch(r"unix:path=" + re.escape(str(prefix / "bus.sock")) + r"(?:,guid=[0-9a-fA-F]{32})?", address) is not None
        and not os.environ.get("DBUS_STARTER_ADDRESS") and not os.environ.get("DBUS_STARTER_BUS_TYPE"),
        "scripts/m3-secret-no-service.sh isolated session bus required")
    client_binary = str((repo / args.binary).resolve())
    server_binary = str((repo / args.server).resolve())
    driver = server = None
    session_id = None
    wd = None
    fixture = Path(tempfile.mkdtemp(prefix="notryx-m3-move-webdriver-"))
    cleanup_safe = False
    watchdog = None
    server_fd = None
    server_identity = None

    def wait(check, description, timeout=60):
        until = time.monotonic() + timeout
        while time.monotonic() < until:
            if watchdog is not None:
                watchdog.check()
            result = check()
            if result:
                return result
            time.sleep(0.1)
        raise TestFailure("timed out waiting for " + description)

    def stop_driver():
        if driver is None:
            return
        if watchdog.driver_ref is None:
            if driver.poll() is None:
                driver.terminate()
            driver.wait(timeout=10)
            raise TestFailure("driver identity unavailable; fixture retained")
        identities = set(watchdog.observed_children.get(driver.pid, set()).copy())
        if watchdog.driver_identity:
            identities.add(watchdog.driver_identity)
        for child in perf.descendants(driver.pid):
            item = perf.process_stat(child)
            if item is not None:
                identities.add((child, item["starttime"]))
        watchdog.observed_children.setdefault(driver.pid, set()).update(identities)
        result = watchdog.signal_safe_target("driver", signal.SIGTERM)
        require(result in ("sent", "already_exited"), "driver identity could not be confirmed; fixture retained")
        require(perf.wait_group_clear(watchdog, driver.pid, identities, "move_browser_wait"), "browser process termination unconfirmed; fixture retained")
        driver.wait(timeout=2)

    def stop_server():
        if server is None:
            return
        if server_fd is None or server_identity is None:
            if server_fd is not None:
                perf.signal_pinned(server_fd, signal.SIGTERM)
            elif server.poll() is None:
                server.terminate()
            server.wait(timeout=10)
            raise TestFailure("server identity unavailable; fixture retained")
        result = perf.signal_pinned(server_fd, signal.SIGTERM)
        require(result in ("sent", "already_exited"), "server identity could not be confirmed; fixture retained")
        server.wait(timeout=10)
        observation = perf.group_observation(server.pid, {server_identity})
        require(perf.observation_clear(observation), "server process termination unconfirmed; fixture retained")

    try:
        require(hasattr(os, "pidfd_open") and hasattr(signal, "pidfd_send_signal"), "pidfd support is required")
        watchdog = perf.Watchdog(fixture, 240)
        watchdog.start()
        notes, state, run, client, xdg_data, xdg_cache, xdg_config = (
            fixture / name for name in ("notes", "state", "run", "client", "xdg-data", "xdg-cache", "xdg-config")
        )
        for path in (notes, state, run, client, xdg_data, xdg_cache, xdg_config):
            path.mkdir(mode=0o700)
        port = helpers.free_port()
        config = fixture / "server.toml"
        config.write_text(f'[server]\nlisten = "127.0.0.1:{port}"\nadmin_socket = "{run}/admin.sock"\n[workspace]\nnotes_root = "{notes}"\nstate_root = "{state}"\n', encoding="utf-8")
        config.chmod(0o600)
        database, server_database = client / "drafts.sqlite3", state / "state.db"
        source = b"\xef\xbb\xbf# Synthetic target\r\n[sibling](Sibling.md)\r\n"
        sibling_bytes = b"# Synthetic sibling\n"

        def cli(*arguments):
            try:
                return subprocess.run([server_binary, *arguments], stdout=subprocess.PIPE,
                    stderr=subprocess.DEVNULL, text=True, timeout=30, check=True).stdout
            except (OSError, subprocess.SubprocessError):
                raise TestFailure("synthetic server command failed") from None

        def admin(*arguments):
            try:
                return json.loads(cli("admin", "--config", str(config), *arguments))
            except (ValueError, KeyError):
                raise TestFailure("invalid synthetic admin response") from None

        def cached_nodes():
            if not database.exists():
                return []
            with sqlite3.connect(f"file:{database}?mode=ro", uri=True, timeout=5) as db:
                rows = db.execute("SELECT n.state FROM remote_nodes n JOIN client_registrations r ON r.id=n.registration_id WHERE r.status='active'").fetchall()
            return [json.loads(raw) for (raw,) in rows]

        def find_node(name, kind):
            matches = [node for node in cached_nodes() if node.get("name") == name and node.get("kind") == kind and not node.get("deleted")]
            return matches[0] if len(matches) == 1 else None

        def find_node_id(node_id, name, parent):
            return next((node for node in cached_nodes() if node.get("node_id") == node_id and node.get("name") == name and node.get("parent_id") == parent and not node.get("deleted")), None)

        def move_rows():
            if not database.exists():
                return []
            with sqlite3.connect(f"file:{database}?mode=ro", uri=True, timeout=5) as db:
                return db.execute("""
                    SELECT o.operation_id,o.operation_seq,o.document_id,o.expected_revision,o.status,
                           o.terminal_kind,o.receipt_json,o.bytes,s.ack_through,s.confirmed_through,r.workspace_epoch
                    FROM client_outbox o JOIN client_registrations r ON r.id=o.registration_id
                    JOIN outbox_streams s ON s.registration_id=r.id
                    WHERE r.status='active' AND o.operation_kind='move' ORDER BY o.operation_seq
                """).fetchall()

        def server_node(node_id):
            with sqlite3.connect(f"file:{server_database}?mode=ro", uri=True, timeout=5) as db:
                return db.execute("SELECT parent_id,name,kind,content_rev,meta_rev,content_hash,size,deleted FROM nodes WHERE node_id=?", (node_id,)).fetchone()

        def committed(count):
            found = move_rows()
            if len(found) != count:
                return None
            row = found[-1]
            return row if row[4] == "committed" and row[6] and row[8] >= row[1] and row[9] >= row[1] else None

        def check_receipt(row, node, old_parent, new_parent, new_name):
            receipt = json.loads(row[6])
            payload = json.loads(row[7])
            require(uuid.UUID(row[0]).version == 7 and row[2] == node["node_id"] and row[3] == node["meta_etag"], "move identity or confirmed metadata ETag changed")
            require(payload == {"expected_parent_id": old_parent, "parent_id": new_parent, "name": new_name}, "move request differs from confirmation")
            require(receipt.get("status") == "moved" and receipt.get("operation_id") == row[0] and int(receipt.get("operation_seq", -1)) == row[1]
                and receipt.get("workspace_epoch") == row[10], "move receipt identity differs from request")
            result = receipt.get("node", {})
            meta_revision = result.get("meta_rev")
            require(isinstance(meta_revision, str) and bool(meta_revision), "move receipt metadata revision is invalid")
            require(result.get("node_id") == node["node_id"] and result.get("parent_id") == new_parent and result.get("name") == new_name
                and result.get("kind") == "file" and result.get("deleted") is False
                and result.get("content_rev") == node["content_rev"] and result.get("content_hash") == node["content_hash"]
                and result.get("meta_etag") == '"' + meta_revision + '"'
                and int(receipt.get("commit_seq", -1)) == int(result.get("last_change_seq", -2)), "move receipt changed identity or content")
            require(row[5] is None and row[8] >= row[1] and row[9] >= row[1], "move receipt ACK was not confirmed")

        try:
            cli("init", "--config", str(config))
            (notes / "Target.md").write_bytes(source)
            (notes / "Sibling.md").write_bytes(sibling_bytes)
            (notes / "Destination").mkdir()
            server = subprocess.Popen([server_binary, "serve", "--config", str(config)],
                stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True, start_new_session=True)
            server_fd = os.pidfd_open(server.pid)
            watchdog.pidfds.append(server_fd)
            server_stat = perf.process_stat(server.pid)
            require(server_stat is not None, "server identity unavailable")
            server_identity = (server.pid, server_stat["starttime"])
            output = bytearray()
            startup_deadline = time.monotonic() + 30
            while b"\n" not in output and time.monotonic() < startup_deadline:
                watchdog.check()
                require(helpers.leader_running(server), "synthetic server stopped before startup")
                ready, _, _ = select.select([server.stdout], [], [], 0.1)
                if ready:
                    chunk = os.read(server.stdout.fileno(), 128)
                    require(bool(chunk), "synthetic server startup stream closed")
                    output.extend(chunk)
                    require(len(output) <= 128, "invalid synthetic server startup response")
            require(b"\n" in output and output.partition(b"\n")[0] == b"server_started" and helpers.leader_running(server), "synthetic server did not start")
            status = admin("status")
            grant = admin("issue-pairing", status["owner_principal_id"])
            driver_port = helpers.free_port()
            origin = f"http://127.0.0.1:{driver_port}"
            opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
            environment = os.environ.copy()
            environment.update(TAURI_WEBVIEW_AUTOMATION="true", NOTRYX_M0_DATA_DIR=str(client),
                XDG_DATA_HOME=str(xdg_data), XDG_CACHE_HOME=str(xdg_cache), XDG_CONFIG_HOME=str(xdg_config))
            driver = subprocess.Popen([args.driver, f"--port={driver_port}", "--host=127.0.0.1"],
                env=environment, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True)
            driver_fd = os.pidfd_open(driver.pid)
            watchdog.pidfds.append(driver_fd)
            driver_stat = perf.process_stat(driver.pid)
            require(driver_stat is not None, "driver identity unavailable")
            watchdog.driver = driver
            watchdog.driver_fd = driver_fd
            watchdog.driver_identity = (driver.pid, driver_stat["starttime"])
            watchdog.driver_ref = (driver, driver_fd, watchdog.driver_identity)

            def wd(method, path, payload=None):
                watchdog.check()
                data = None if payload is None else json.dumps(payload).encode()
                request = urllib.request.Request(origin + path, data=data, method=method, headers={"Content-Type": "application/json"})
                try:
                    with opener.open(request, timeout=35) as response:
                        result = json.load(response)["value"]
                    if isinstance(result, dict) and "error" in result:
                        raise TestFailure("WebDriver returned an error")
                    return result
                except (OSError, ValueError, KeyError, urllib.error.HTTPError):
                    raise TestFailure("WebDriver request failed") from None

            wait(lambda: helpers.leader_running(driver) and helpers.driver_owns_port(driver.pid, driver_port), "WebDriver startup")
            def driver_ready():
                try:
                    return wd("GET", "/status")
                except TestFailure:
                    return False
            wait(driver_ready, "WebDriver status")
            capabilities = {"capabilities": {"alwaysMatch": {"browserName": "wry", "webkitgtk:browserOptions": {"binary": client_binary}}}}
            session_id = wd("POST", "/session", capabilities)["sessionId"]
            wd("POST", f"/session/{session_id}/timeouts", {"script": 30000})
            def js(script, *values, asynchronous=False):
                return wd("POST", f"/session/{session_id}/execute/" + ("async" if asynchronous else "sync"), {"script": script, "args": list(values)})
            wait(lambda: js("return !!document.querySelector('.workspace-connect button:not(:disabled)')"), "pairing form")
            js("""const form=document.querySelector('.workspace-connect');
              form.elements.namedItem('origin').value=arguments[0];
              form.elements.namedItem('workspace_id').value=arguments[1];
              form.elements.namedItem('pairing_secret').value=arguments[2]; form.requestSubmit();""",
              f"http://127.0.0.1:{port}", grant["workspace_id"], grant["pairing_secret"])
            require(js("return document.querySelector('[name=pairing_secret]').value === ''"), "pairing secret remained in form")
            wait(lambda: js("""const done=arguments[arguments.length-1];
              window.__TAURI_INTERNALS__.invoke('m0_renderer_session')
              .then(owner=>window.__TAURI_INTERNALS__.invoke('workspace_sync_status',{request:{renderer_session_id:owner}}))
              .then(value=>done(value.status==='ready')).catch(()=>done(false));""", asynchronous=True), "initial sync")
            target = wait(lambda: find_node("Target.md", "file"), "target metadata")
            sibling = wait(lambda: find_node("Sibling.md", "file"), "sibling metadata")
            folder = wait(lambda: find_node("Destination", "folder"), "destination metadata")
            root = target["parent_id"]
            require(root and sibling["parent_id"] == root and folder["parent_id"] == root, "synthetic tree differs from fixture")
            original = server_node(target["node_id"])
            require(original is not None and original[0:3] == (root, "Target.md", "file") and original[3] == target["content_rev"], "server source identity differs from cached metadata")
            wait(lambda: js("return !!document.querySelector('[data-document=\"'+arguments[0]+'\"]:not(:disabled)')", target["node_id"]), "target document")
            js("document.querySelector('[data-document=\"'+arguments[0]+'\"]').click()", target["node_id"])
            wait(lambda: js("return document.querySelector('[aria-current=page]')?.dataset.document===arguments[0] && !!document.querySelector('.cm-content[contenteditable=true]')", target["node_id"]), "opened target document")
            require((notes / "Target.md").read_bytes() == source, "opening the source changed bytes")

            def prepare_move(node_id, parent, name, affected):
                wait(lambda: js("return !!document.querySelector('[data-move-node=\"'+arguments[0]+'\"]:not(:disabled)')", node_id), "move control")
                js("document.querySelector('[data-move-node=\"'+arguments[0]+'\"]').click()", node_id)
                wait(lambda: js("return !!document.querySelector('[data-testid=move-parent] option[value=\"'+arguments[0]+'\"]')", parent), "destination option")
                js("""const select=document.querySelector('[data-testid=move-parent]');
                  Object.getOwnPropertyDescriptor(HTMLSelectElement.prototype,'value').set.call(select,arguments[0]); select.dispatchEvent(new Event('change',{bubbles:true}));
                  const input=document.querySelector('[data-testid=move-name]');
                  Object.getOwnPropertyDescriptor(HTMLInputElement.prototype,'value').set.call(input,arguments[1]); input.dispatchEvent(new Event('input',{bubbles:true}));""", parent, name)
                wait(lambda: js("return !!document.querySelector('[data-testid=move-prepare]:not(:disabled)')"), "move review")
                js("document.querySelector('[data-testid=move-prepare]').click()")
                wait(lambda: js("return !!document.querySelector('[data-testid=move-confirm]:not(:disabled)')"), "move confirmation")
                panel = js("return document.querySelector('[aria-label=\"문서 이름 변경 또는 이동\"]')?.textContent || ''")
                require("열린 문서의 미리보기 링크 검사" in panel and f"영향 가능 {affected}건" in panel
                    and "다른 문서와 지원하지 않는 구문은 검사하지 않았습니다" in panel, "link impact or uninspected scope was not shown")
                require(name in panel, "confirmation did not show proposed name")

            prepare_move(target["node_id"], root, "Renamed.md", 0)
            js("document.querySelector('[data-testid=move-confirm-cancel]').click()")
            wait(lambda: js("return !!document.querySelector('[data-testid=move-prepare]')"), "cancelled move review")
            require(not move_rows() and (notes / "Target.md").read_bytes() == source and server_node(target["node_id"]) == original, "cancelling review changed data")
            js("document.querySelector('[data-testid=move-prepare]').click()")
            wait(lambda: js("return !!document.querySelector('[data-testid=move-confirm]:not(:disabled)')"), "rename confirmation")
            js("document.querySelector('[data-testid=move-confirm]').click()")
            first = wait(lambda: committed(1), "rename receipt and ACK")
            check_receipt(first, target, root, root, "Renamed.md")
            wait(lambda: js("return document.querySelector('[aria-label=\"문서 이름 변경 또는 이동\"]')?.textContent.includes('이동 완료')"), "rename completed status")
            require(not (notes / "Target.md").exists() and (notes / "Renamed.md").read_bytes() == source, "rename did not preserve original bytes")
            first_server = server_node(target["node_id"])
            require(first_server is not None and first_server[0:4] == (root, "Renamed.md", "file", original[3]) and first_server[4] != original[4] and first_server[5:] == original[5:], "rename changed server identity or content")
            first_receipt_node = json.loads(first[6])["node"]
            require(first_receipt_node["meta_rev"] == first_server[4] and first_receipt_node["size"] == str(first_server[6]), "rename receipt differs from server state")
            renamed = wait(lambda: find_node_id(target["node_id"], "Renamed.md", root), "renamed metadata catch-up")
            require(renamed["content_rev"] == target["content_rev"] and renamed["meta_etag"] != target["meta_etag"], "rename cache revision is incorrect")
            wait(lambda: js("return document.querySelector('[data-document=\"'+arguments[0]+'\"]')?.textContent.includes('Renamed.md')", target["node_id"]), "renamed document row")
            js("[...document.querySelectorAll('[aria-label=\"문서 이름 변경 또는 이동\"] button')].find(x=>x.textContent.trim()==='닫기').click()")
            wait(lambda: js("return !document.querySelector('[aria-label=\"문서 이름 변경 또는 이동\"]')"), "rename panel closed")

            prepare_move(target["node_id"], folder["node_id"], "Renamed.md", 1)
            js("document.querySelector('[data-testid=move-confirm]').click()")
            second = wait(lambda: committed(2), "move receipt and ACK")
            require(second[1] == first[1] + 1 and second[0] != first[0], "move did not receive a distinct ordered operation")
            check_receipt(second, renamed, root, folder["node_id"], "Renamed.md")
            wait(lambda: js("return (document.querySelector('[aria-label=\"문서 이름 변경 또는 이동\"]')?.textContent.match(/이동 완료/g)||[]).length >= 2"), "move completed status")
            require(not (notes / "Renamed.md").exists() and (notes / "Destination" / "Renamed.md").read_bytes() == source, "move did not preserve original bytes")
            moved_server = server_node(target["node_id"])
            require(moved_server is not None and moved_server[0:4] == (folder["node_id"], "Renamed.md", "file", original[3]) and moved_server[4] != first_server[4] and moved_server[5:] == original[5:], "move changed server identity or content")
            moved_receipt_node = json.loads(second[6])["node"]
            require(moved_receipt_node["meta_rev"] == moved_server[4] and moved_receipt_node["size"] == str(moved_server[6]), "move receipt differs from server state")
            moved = wait(lambda: find_node_id(target["node_id"], "Renamed.md", folder["node_id"]), "moved metadata catch-up")
            require(moved["content_rev"] == original[3] and moved["meta_etag"] != renamed["meta_etag"], "moved cache revision is incorrect")
            wait(lambda: js("return document.querySelector('[data-document=\"'+arguments[0]+'\"]')?.textContent.includes('Destination/Renamed.md')", target["node_id"]), "moved document row")
            require((notes / "Sibling.md").read_bytes() == sibling_bytes, "moving target changed sibling bytes")
        finally:
            if session_id and wd is not None:
                try:
                    wd("DELETE", f"/session/{session_id}")
                except Exception:
                    pass
            try:
                stop_driver()
            finally:
                stop_server()
        cleanup_safe = True
    finally:
        watchdog_failure = None
        if watchdog is not None:
            watchdog.finish()
            if watchdog.worker.is_alive():
                cleanup_safe = False
                watchdog_failure = "watchdog shutdown unconfirmed"
            elif watchdog.abort.is_set():
                cleanup_safe = False
                watchdog_failure = watchdog.abort_reason or "resource watchdog aborted"
        if cleanup_safe:
            shutil.rmtree(fixture)
        else:
            print(f"Synthetic fixture retained for inspection: {fixture}", file=sys.stderr)
        if watchdog_failure:
            raise TestFailure(watchdog_failure)
    print("M3 move WebView passed: rename, cross-folder move, link impact scope, exact bytes, node identity, receipts and confirmed ACK")


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        print(f"M3 move WebView failed: {error}", file=sys.stderr)
        raise SystemExit(1)
