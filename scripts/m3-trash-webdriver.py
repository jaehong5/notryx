#!/usr/bin/env python3
"""Exercise normal and permission-denied restores in an isolated Linux WebView."""

import argparse
import base64
import hashlib
import http.client
import importlib.util
import json
import os
from pathlib import Path
import select
import sqlite3
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request

from m3_trash_proxy import TrashProxy


def load_helpers():
    path = Path(__file__).with_name("m2-workspace-webdriver.py")
    spec = importlib.util.spec_from_file_location("notryx_m2_webdriver_helpers", path)
    if spec is None or spec.loader is None:
        raise RuntimeError("could not load WebDriver helpers")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


helpers = load_helpers()
TestFailure = helpers.TestFailure
require = helpers.require


def wait(check, description, seconds=60):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        result = check()
        if result:
            return result
        time.sleep(0.1)
    raise TestFailure(f"timed out waiting for {description}")


def readonly(path):
    return sqlite3.connect(f"file:{path}?mode=ro", uri=True, timeout=5)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--driver", default="WebKitWebDriver")
    parser.add_argument("--binary", default="target/debug/notryx-client")
    parser.add_argument("--server", default="target/debug/notryx-server")
    scenarios = parser.add_mutually_exclusive_group()
    scenarios.add_argument("--retry-after-promotion", action="store_true")
    scenarios.add_argument("--normal-restore", action="store_true")
    args = parser.parse_args()
    require(helpers.xvfb_on_current_display(), "trash restore test requires isolated Xvfb")
    client_binary = str(Path(args.binary).resolve())
    server_binary = str(Path(args.server).resolve())
    server = driver = proxy = None
    session_id = None
    wd = None

    with tempfile.TemporaryDirectory(prefix="notryx-m3-trash-webdriver-") as temporary:
        root = Path(temporary)
        notes, state, run, client_data, xdg_data, xdg_cache, xdg_config = (
            root / name for name in ("notes", "state", "run", "client", "xdg-data", "xdg-cache", "xdg-config")
        )
        for directory in (notes, state, run, client_data, xdg_data, xdg_cache, xdg_config):
            directory.mkdir(mode=0o700)
        port = helpers.free_port()
        config = root / "server.toml"
        config.write_text(
            f'[server]\nlisten = "127.0.0.1:{port}"\nadmin_socket = "{run}/admin.sock"\n'
            f'[workspace]\nnotes_root = "{notes}"\nstate_root = "{state}"\n', encoding="utf-8"
        )
        config.chmod(0o600)

        def cli(*arguments):
            try:
                return subprocess.run(
                    [server_binary, *arguments], stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                    text=True, timeout=30, check=True,
                ).stdout
            except (OSError, subprocess.SubprocessError):
                raise TestFailure("synthetic server command failed") from None

        def admin(*arguments):
            try:
                return json.loads(cli("admin", "--config", str(config), *arguments))
            except (ValueError, KeyError):
                raise TestFailure("invalid synthetic admin response") from None

        def server_request(method, path, *, body=None, headers=None, expected=200):
            connection = http.client.HTTPConnection("127.0.0.1", port, timeout=20)
            try:
                connection.request(method, path, body, headers or {})
                response = connection.getresponse()
                data = response.read(1024 * 1024 + 1)
                require(response.status == expected and len(data) <= 1024 * 1024, "unexpected synthetic server response")
                return json.loads(data) if data else None
            except (OSError, http.client.HTTPException, ValueError):
                raise TestFailure("synthetic server request failed") from None
            finally:
                connection.close()

        def server_snapshot(node_id):
            with readonly(state / "state.db") as db:
                head = db.execute("SELECT head_seq FROM workspaces WHERE singleton=1").fetchone()
                node = db.execute(
                    "SELECT node_id, parent_id, name, kind, content_rev, meta_rev, content_hash, size, deleted, last_change_seq FROM nodes WHERE node_id=?",
                    (node_id,),
                ).fetchone()
                changes = db.execute("SELECT seq, operation_id FROM change_sets ORDER BY CAST(seq AS INTEGER)").fetchall()
                node_changes = db.execute("SELECT seq, node_id, post_state FROM node_changes ORDER BY CAST(seq AS INTEGER), node_id").fetchall()
            return head, node, changes, node_changes

        def active_registration():
            path = client_data / "drafts.sqlite3"
            if not path.exists():
                return None
            with readonly(path) as db:
                return db.execute("SELECT id, device_stream_id, workspace_epoch FROM client_registrations WHERE status='active'").fetchone()

        def restore_row(document_id=None):
            path = client_data / "drafts.sqlite3"
            if not path.exists():
                return None
            with readonly(path) as db:
                rows = db.execute("""
                    SELECT o.operation_id, o.operation_seq, o.document_id, o.status, o.abandon_state,
                           o.terminal_kind, o.receipt_json, o.bytes, r.id, r.device_stream_id,
                           r.workspace_epoch, s.ack_through, s.confirmed_through
                    FROM client_outbox o JOIN client_registrations r ON r.id=o.registration_id
                    JOIN outbox_streams s ON s.registration_id=r.id
                    WHERE r.status='active' AND o.operation_kind='restore'
                      AND (? IS NULL OR o.document_id=?)
                """, (document_id, document_id)).fetchall()
            require(len(rows) <= 1, "duplicate restore requests were queued")
            return rows[0] if rows else None

        def no_open_workspace_document():
            with readonly(client_data / "drafts.sqlite3") as db:
                return db.execute("""
                    SELECT COUNT(*) FROM registered_documents d
                    JOIN client_registrations r ON r.id=d.registration_id WHERE r.status='active'
                """).fetchone()[0] == 0

        try:
            cli("init", "--config", str(config))
            server = subprocess.Popen(
                [server_binary, "serve", "--config", str(config)], stdout=subprocess.PIPE,
                stderr=subprocess.DEVNULL, text=True, start_new_session=True,
            )
            ready, _, _ = select.select([server.stdout], [], [], 30)
            require(bool(ready) and server.stdout.readline().strip() == "server_started" and helpers.leader_running(server), "synthetic server did not start")
            status = admin("status")
            owner_grant = admin("issue-pairing", status["owner_principal_id"])
            owner = server_request("POST", "/v1/pair", body=json.dumps({
                "protocol": "mdw-sync/1.2", "workspace_id": owner_grant["workspace_id"],
                "pairing_secret": owner_grant["pairing_secret"],
            }).encode(), headers={"Content-Type": "application/json"})
            authorization = {"Authorization": f"Bearer {owner['device_token']}"}
            info = server_request("GET", "/v1/server-info", headers=authorization)
            node_id = helpers.operation_id()
            source = b"# Synthetic recovery source\n"
            if args.normal_restore:
                source = b"\xef\xbb\xbf---\r\ntitle: Recovery\r\n---\n# Synthetic recovery source\r\nUnknown:: [[syntax]]\n"
            digest = hashlib.sha256(source).digest()
            upload = server_request("POST", "/v1/uploads", body=source, expected=201, headers={
                **authorization, "Content-Type": "application/octet-stream",
                "Content-Digest": "sha-256=:" + base64.b64encode(digest).decode() + ":",
            })
            headers = {
                **authorization, "X-Workspace-Epoch": info["workspace_epoch"],
                "X-Device-Stream": owner["device_stream_id"],
            }
            server_request("POST", "/v1/nodes", body=json.dumps({
                "node_id": node_id, "parent_id": info["root_id"], "name": "Deleted.md",
                "kind": "file", "upload_id": upload["upload_id"],
            }).encode(), headers={**headers, "Content-Type": "application/json", "X-Operation-Seq": "1", "Idempotency-Key": helpers.operation_id()})
            node = server_request("GET", f"/v1/nodes/{node_id}", headers=authorization)
            server_request("DELETE", f"/v1/nodes/{node_id}", headers={
                **headers, "If-Match": node["node_etag"], "X-Operation-Seq": "2", "Idempotency-Key": helpers.operation_id(),
            })
            require(not (notes / "Deleted.md").exists(), "synthetic deletion did not remove the file")
            object_path = state / "objects" / "sha256" / digest.hex()
            require(object_path.read_bytes() == source, "deleted source object was not preserved")
            before = server_snapshot(node_id)
            require(before[1] is not None and before[1][8] == 1, "synthetic tombstone was not created")
            if args.normal_restore:
                folder_id = helpers.operation_id()
                server_request("POST", "/v1/nodes", body=json.dumps({
                    "node_id": folder_id, "parent_id": info["root_id"], "name": "Deleted folder", "kind": "folder",
                }).encode(), headers={**headers, "Content-Type": "application/json", "X-Operation-Seq": "3", "Idempotency-Key": helpers.operation_id()})
                folder = server_request("GET", f"/v1/nodes/{folder_id}", headers=authorization)
                server_request("DELETE", f"/v1/nodes/{folder_id}", headers={
                    **headers, "If-Match": folder["node_etag"], "X-Operation-Seq": "4", "Idempotency-Key": helpers.operation_id(),
                })
                require(not (notes / "Deleted folder").exists(), "synthetic deletion did not remove the folder")
                require(server_snapshot(folder_id)[1][8] == 1, "synthetic folder tombstone was not created")
                require(not (client_data / "drafts.sqlite3").exists(), "client store existed before fixture deletion")
                grant = admin("issue-pairing", status["owner_principal_id"])
                pairing_port = port
            else:
                reader = admin("create-principal", "reader")
                reader_id = reader["principal_id"]
                grant = admin("issue-pairing", reader_id)
                proxy = TrashProxy(port, node_id)
                proxy.start()
                pairing_port = proxy.server_address[1]

            driver_port = helpers.free_port()
            origin = f"http://127.0.0.1:{driver_port}"
            opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
            environment = os.environ.copy()
            environment.update(
                TAURI_WEBVIEW_AUTOMATION="true", NOTRYX_M0_DATA_DIR=str(client_data),
                XDG_DATA_HOME=str(xdg_data), XDG_CACHE_HOME=str(xdg_cache), XDG_CONFIG_HOME=str(xdg_config),
            )
            driver = subprocess.Popen(
                [args.driver, f"--port={driver_port}", "--host=127.0.0.1"], env=environment,
                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True,
            )

            def wd(method, path, payload=None):
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
                mode = "async" if asynchronous else "sync"
                return wd("POST", f"/session/{session_id}/execute/{mode}", {"script": script, "args": list(values)})

            def pair_in_form(pairing):
                wait(lambda: js("const button=document.querySelector('.workspace-connect button'); return !!button && !button.disabled"), "pairing form")
                js("""
                    const form=document.querySelector('.workspace-connect');
                    form.elements.namedItem('origin').value=arguments[0];
                    form.elements.namedItem('workspace_id').value=arguments[1];
                    form.elements.namedItem('pairing_secret').value=arguments[2];
                    form.requestSubmit();
                """, f"http://127.0.0.1:{pairing_port}", pairing["workspace_id"], pairing["pairing_secret"])
                require(js("return document.querySelector('[name=pairing_secret]').value === ''"), "pairing secret remained in the form")

            def sync_ready():
                return js("""
                    const done=arguments[arguments.length-1];
                    window.__TAURI_INTERNALS__.invoke('m0_renderer_session')
                      .then(owner=>window.__TAURI_INTERNALS__.invoke('workspace_sync_status', {request:{renderer_session_id:owner}}))
                      .then(value=>done(value.status==='ready')).catch(()=>done(false));
                """, asynchronous=True)

            def open_trash():
                wait(lambda: js("return !!document.querySelector('[aria-label=휴지통]') || [...document.querySelectorAll('button')].some(button=>button.textContent.trim()==='휴지통' && !button.disabled)"), "trash control")
                js("const panel=document.querySelector('[aria-label=휴지통]'); if(!panel) [...document.querySelectorAll('button')].find(button=>button.textContent.trim()==='휴지통').click()")
                wait(lambda: js("return !!document.querySelector('[aria-label=휴지통]')"), "trash panel")

            if args.normal_restore:
                pair_in_form(grant)
                registration = wait(active_registration, "owner registration")
                wait(sync_ready, "owner initial sync")
                require(no_open_workspace_document(), "new client already had an open workspace document")
                live = server_request("GET", "/v1/nodes", headers=authorization)
                require({node["node_id"] for node in live["nodes"]} == {info["root_id"]}, "deleted nodes appeared in live inventory")
                wait(lambda: js("return document.querySelectorAll('[data-document]').length === 0"), "empty workspace list")
                open_trash()
                for restored_id in (node_id, folder_id):
                    wait(lambda: js("return !!document.querySelector('[data-trash-node=\"'+arguments[0]+'\"]:not(:disabled)')", restored_id), "pre-bootstrap deletion in trash")
                print("AT-19: fresh client discovered the deleted document and empty folder.", flush=True)

                for restored_id, name, kind in ((node_id, "Deleted.md", "file"), (folder_id, "Deleted folder", "folder")):
                    before_restore = server_snapshot(restored_id)
                    js("document.querySelector('[data-trash-node=\"'+arguments[0]+'\"]').click()", restored_id)
                    wait(lambda: js("return !!document.querySelector('[data-testid=restore-parent] option[value=\"'+arguments[0]+'\"]')", info["root_id"]), "root destination")
                    if kind == "folder":
                        require(js("return document.querySelector('[aria-label=휴지통]').textContent.includes('빈 폴더만 복원하며 자식은 복원하지 않습니다.')"), "empty-folder restore notice was missing")
                    js("const select=document.querySelector('[data-testid=restore-parent]'); select.value=arguments[0]; select.dispatchEvent(new Event('change',{bubbles:true}));", info["root_id"])
                    wait(lambda: js("const button=document.querySelector('[data-testid=restore-submit]'); return !!button && !button.disabled"), "restore submit")
                    js("document.querySelector('[data-testid=restore-submit]').click()")

                    def confirmed_restore():
                        row = restore_row(restored_id)
                        if not row or row[3:6] != ("committed", "none", None) or row[11] < row[1] or row[12] < row[1]:
                            return None
                        return row

                    completed = wait(confirmed_restore, "restore receipt and confirmed ACK")
                    require(completed[8:11] == registration, "restore changed the owner registration")
                    receipt = json.loads(completed[6])
                    require(receipt.get("status") == "restored" and receipt.get("operation_id") == completed[0] and receipt.get("operation_seq") == str(completed[1]) and receipt.get("workspace_epoch") == registration[2], "restore receipt identity disagreed")
                    restored = receipt.get("node", {})
                    require(restored.get("node_id") == restored_id and restored.get("parent_id") == info["root_id"] and restored.get("name") == name and restored.get("kind") == kind and restored.get("deleted") is False, "restore receipt changed node identity")
                    after_restore = server_snapshot(restored_id)
                    commit_seq = receipt.get("commit_seq")
                    require(after_restore[1][0:4] == (restored_id, info["root_id"], name, kind) and after_restore[1][8] == 0 and after_restore[1][9] == commit_seq, "server node was not restored")
                    require(int(after_restore[0][0]) == int(before_restore[0][0]) + 1 and after_restore[2] == before_restore[2] + [(commit_seq, completed[0])], "restore did not produce exactly one change set")
                    with readonly(state / "state.db") as db:
                        stream = db.execute("SELECT last_seq, retired_through FROM operation_streams WHERE stream_id=?", (registration[1],)).fetchone()
                        retired = db.execute("SELECT stream_id, operation_seq FROM retired_operations WHERE operation_id=?", (completed[0],)).fetchone()
                        remaining = db.execute("SELECT COUNT(*) FROM operation_receipts WHERE operation_id=?", (completed[0],)).fetchone()[0]
                    require(stream is not None and int(stream[0]) == completed[1] and int(stream[1]) >= completed[1], "server did not confirm the restore ACK")
                    require(retired == (registration[1], str(completed[1])) and remaining == 0, "server did not retire the acknowledged receipt")
                    if kind == "file":
                        require((notes / name).read_bytes() == source and object_path.read_bytes() == source, "restore changed the original source bytes")
                        wait(lambda: js("return !!document.querySelector('[data-document=\"'+arguments[0]+'\"]:not(:disabled)')", restored_id), "restored document in workspace list")
                    else:
                        require((notes / name).is_dir() and not list((notes / name).iterdir()), "restored folder was not empty")
                    wait(lambda: js("return [...document.querySelectorAll('[aria-label=휴지통] p')].some(item=>item.textContent.includes(arguments[0]+': 복원 완료'))", name), "restore completion notice")
                    wait(lambda: js("return [...document.querySelectorAll('[aria-label=휴지통] button')].some(button=>button.textContent.trim()==='휴지통 새로고침' && !button.disabled)"), "trash refresh")
                    js("[...document.querySelectorAll('[aria-label=휴지통] button')].find(button=>button.textContent.trim()==='휴지통 새로고침').click()")
                    wait(lambda: js("return !document.querySelector('[data-trash-node=\"'+arguments[0]+'\"]')", restored_id), "restored node absent from refreshed trash")
                    print(f"AT-19: {kind} restored with its original ID and confirmed server ACK.", flush=True)

                live = server_request("GET", "/v1/nodes", headers=authorization)
                require({node["node_id"] for node in live["nodes"]} == {info["root_id"], node_id, folder_id}, "restored live inventory disagreed")
                with readonly(client_data / "drafts.sqlite3") as db:
                    count = db.execute("SELECT COUNT(*) FROM client_outbox WHERE registration_id=? AND operation_kind='restore'", (registration[0],)).fetchone()[0]
                require(count == 2 and no_open_workspace_document(), "normal restore queued extra requests or opened a document")
                require((notes / "Deleted.md").read_bytes() == source, "folder restore changed the restored document")
                print("M3 normal trash WebView passed: pre-bootstrap discovery, document bytes, empty folder, original IDs, inventory and ACK.", flush=True)
                return

            pair_in_form(grant)
            first_registration = wait(active_registration, "reader registration")
            wait(sync_ready, "reader initial sync")
            require(no_open_workspace_document(), "reader opened a workspace document")
            wait(lambda: js("return document.querySelectorAll('[data-document]').length === 0"), "empty workspace list")
            open_trash()
            wait(lambda: js("return !!document.querySelector('[data-trash-node=\"'+arguments[0]+'\"]:not(:disabled)')", node_id), "deleted document in trash")
            js("document.querySelector('[data-trash-node=\"'+arguments[0]+'\"]').click()", node_id)
            wait(lambda: js("return !!document.querySelector('[data-testid=restore-parent] option[value=\"'+arguments[0]+'\"]')", info["root_id"]), "root destination")
            js("const select=document.querySelector('[data-testid=restore-parent]'); select.value=arguments[0]; select.dispatchEvent(new Event('change',{bubbles:true}));", info["root_id"])
            wait(lambda: js("const button=document.querySelector('[data-testid=restore-submit]'); return !!button && !button.disabled"), "restore submit")
            js("document.querySelector('[data-testid=restore-submit]').click()")
            wait(lambda: proxy.count("restore", 403) > 0, "real server permission denial")
            pending = wait(lambda: (row if (row := restore_row()) and row[3] == "sent" else None), "durable restore request")
            operation_id, sequence = pending[0], pending[1]
            with proxy.lock:
                require(proxy.restore_operation == operation_id, "restore operation identity disagreed")
                require(not proxy.abandon_operations, "restore was abandoned before the user's request")
            require(pending[2] == node_id and pending[4:7] == ("none", None, None), "pending restore changed before cancellation")
            require((pending[8], pending[9], pending[10]) == first_registration, "restore belongs to a different registration")
            require(pending[11:] == (0, 0), "receipt watermark changed before cancellation")
            require(no_open_workspace_document(), "reader opened a workspace document")
            wait(lambda: js("return [...document.querySelectorAll('[aria-label=휴지통] p')].some(item=>item.textContent.includes('복원 권한이 없습니다'))"), "permission notice")
            require(server_snapshot(node_id) == before, "permission denial changed server data")
            if args.retry_after_promotion:
                wait(lambda: js("return !!document.querySelector('[data-restore-retry=\"'+arguments[0]+'\"]:not(:disabled)')", operation_id), "restore retry control")
                with proxy.lock:
                    request_count_before_promotion = len(proxy.restore_requests)
                admin("set-role", reader_id, "editor")
                time.sleep(2)
                require(active_registration() == first_registration and restore_row() == pending, "promotion changed the pending restore or registration")
                require(server_snapshot(node_id) == before, "promotion changed the server journal or tombstone")
                with proxy.lock:
                    require(len(proxy.restore_requests) == request_count_before_promotion and not proxy.abandon_operations, "promotion sent a restore or abandon without user action")
                js("document.querySelector('[data-restore-retry=\"'+arguments[0]+'\"]').click()", operation_id)
                wait(lambda: proxy.count("restore", 200) > 0 and proxy.count("ack", 200) > 0, "server restore and acknowledgement")
            else:
                wait(lambda: js("return !!document.querySelector('[data-restore-cancel=\"'+arguments[0]+'\"]:not(:disabled)')", operation_id), "restore cancel control")
                with proxy.lock:
                    require(not proxy.abandon_operations, "restore was abandoned before the user's request")
                js("document.querySelector('[data-restore-cancel=\"'+arguments[0]+'\"]').click()", operation_id)
                wait(lambda: proxy.count("abandon", 200) > 0 and proxy.count("ack", 200) > 0, "server abandon and acknowledgement")

            def completed_restore():
                row = restore_row()
                expected_state = ("committed", "none", None) if args.retry_after_promotion else ("committed", "sent", "abandoned")
                if not row or row[0] != operation_id or row[3:6] != expected_state or row[11] < sequence or row[12] < sequence:
                    return None
                try:
                    receipt = json.loads(row[6])
                except (TypeError, ValueError):
                    return None
                expected_status = "restored" if args.retry_after_promotion else "abandoned"
                if receipt.get("status") != expected_status or receipt.get("operation_id") != operation_id or receipt.get("operation_seq") != str(sequence) or receipt.get("workspace_epoch") != row[10]:
                    return None
                return row

            completed = wait(completed_restore, "durable result and confirmed acknowledgement")
            require(completed[1:3] == pending[1:3] and completed[7:11] == pending[7:11], "completed restore changed its request or binding")
            with proxy.lock:
                require(str(sequence) in proxy.acknowledgements, "acknowledgement sequence disagreed")
                if args.retry_after_promotion:
                    require(not proxy.abandon_operations, "retry sent an abandon request")
                    expected_request = (operation_id, str(sequence), pending[9], hashlib.sha256(pending[7]).digest())
                    require(len(proxy.restore_requests) >= 2 and all(request == expected_request for request in proxy.restore_requests), "restore requests changed identity or body")
                else:
                    require((operation_id, str(sequence)) in proxy.abandon_operations, "abandon request identity disagreed")
            with readonly(state / "state.db") as db:
                stream = db.execute("SELECT last_seq, retired_through FROM operation_streams WHERE stream_id=?", (completed[9],)).fetchone()
                retired = db.execute("SELECT stream_id, operation_seq FROM retired_operations WHERE operation_id=?", (operation_id,)).fetchone()
                remaining = db.execute("SELECT COUNT(*) FROM operation_receipts WHERE operation_id=?", (operation_id,)).fetchone()[0]
            require(stream is not None and int(stream[0]) == sequence and int(stream[1]) >= sequence, "server watermark did not advance")
            require(retired == (completed[9], str(sequence)) and remaining == 0, "server did not retire the confirmed receipt")
            if args.retry_after_promotion:
                receipt = json.loads(completed[6])
                restored = receipt.get("node", {})
                after = server_snapshot(node_id)
                commit_seq = receipt.get("commit_seq")
                require(restored.get("node_id") == node_id and restored.get("parent_id") == info["root_id"] and restored.get("name") == "Deleted.md" and restored.get("deleted") is False and restored.get("last_change_seq") == commit_seq, "restore receipt has unexpected node state")
                require(after[0] is not None and before[0] is not None and int(after[0][0]) == int(before[0][0]) + 1 and str(after[0][0]) == commit_seq, "restore advanced the journal unexpectedly")
                require(after[2] == before[2] + [(commit_seq, operation_id)], "restore did not create exactly one change set")
                require(after[3][:len(before[3])] == before[3] and len(after[3]) == len(before[3]) + 2 and {row[1] for row in after[3][len(before[3]):]} == {node_id, info["root_id"]} and all(row[0] == commit_seq for row in after[3][len(before[3]):]), "restore node journal was unexpected")
                require(after[1] is not None and after[1][0:4] == (node_id, info["root_id"], "Deleted.md", "file") and after[1][8] == 0 and after[1][9] == commit_seq, "server node was not restored")
                require((notes / "Deleted.md").read_bytes() == source and object_path.read_bytes() == source, "restore changed the original source")
                wait(lambda: js("return !!document.querySelector('[data-document=\"'+arguments[0]+'\"]')", node_id), "restored document in workspace list")
                expected_snapshot = after
            else:
                wait(lambda: js("return [...document.querySelectorAll('[aria-label=휴지통] p')].some(item=>item.textContent.includes('취소 완료'))"), "cancellation completion notice")
                require(server_snapshot(node_id) == before, "cancellation changed the server journal or tombstone")
                require(not (notes / "Deleted.md").exists() and object_path.read_bytes() == source, "cancellation changed the original source")
                expected_snapshot = before
            require(no_open_workspace_document(), "reader opened a workspace document")

            old_registration_id = completed[8]
            old_request = completed[7]
            grant = admin("issue-pairing", reader_id)
            pair_in_form(grant)
            replacement = wait(lambda: (row if (row := active_registration()) and row[0] != old_registration_id else None), "replacement reader registration")
            wait(sync_ready, "replacement reader initial sync")
            require(replacement[1] != completed[9], "replacement reused the previous device stream")
            with readonly(client_data / "drafts.sqlite3") as db:
                old = db.execute("SELECT r.status, o.status, o.terminal_kind, o.receipt_json, o.bytes FROM client_registrations r JOIN client_outbox o ON o.registration_id=r.id WHERE r.id=? AND o.operation_id=?", (old_registration_id, operation_id)).fetchone()
                old_count = db.execute("SELECT COUNT(*) FROM client_outbox WHERE registration_id=? AND operation_kind='restore'", (old_registration_id,)).fetchone()[0]
                active_count = db.execute("SELECT COUNT(*) FROM client_outbox o JOIN client_registrations r ON r.id=o.registration_id WHERE r.status='active'").fetchone()[0]
            expected_terminal = None if args.retry_after_promotion else "abandoned"
            require(old == ("frozen", "committed", expected_terminal, completed[6], old_request), "replacement changed the previous receipt or request")
            require(old_count == 1 and active_count == 0, "replacement changed the prior outbox or inherited its requests")
            open_trash()
            previous_trash_responses = proxy.count("trash", 200)
            wait(lambda: js("return [...document.querySelectorAll('[aria-label=휴지통] button')].some(button=>button.textContent.trim()==='휴지통 새로고침' && !button.disabled)"), "replacement trash refresh")
            js("[...document.querySelectorAll('[aria-label=휴지통] button')].find(button=>button.textContent.trim()==='휴지통 새로고침').click()")
            wait(lambda: proxy.count("trash", 200) > previous_trash_responses, "replacement trash response")
            if args.retry_after_promotion:
                wait(lambda: js("return !document.querySelector('[data-trash-node=\"'+arguments[0]+'\"]')", node_id), "restored node absent from replacement trash")
                wait(lambda: js("return !!document.querySelector('[data-document=\"'+arguments[0]+'\"]')", node_id), "restored node in replacement workspace")
                require(no_open_workspace_document() and (notes / "Deleted.md").read_bytes() == source, "replacement changed or opened the restored document")
            else:
                wait(lambda: js("return !!document.querySelector('[data-trash-node=\"'+arguments[0]+'\"]')", node_id), "replacement trash tombstone")
            wait(lambda: js("return !document.querySelector('[data-restore-cancel=\"'+arguments[0]+'\"]')", operation_id), "old restore hidden after replacement")
            wait(lambda: js("return !document.querySelector('[data-restore-retry=\"'+arguments[0]+'\"]')", operation_id), "old retry hidden after replacement")
            wait(lambda: js("const panel=document.querySelector('[aria-label=휴지통]'); return !!panel && !panel.textContent.includes('취소 완료') && !panel.textContent.includes('복원 권한이 없습니다')"), "old restore status hidden after replacement")
            require(server_snapshot(node_id) == expected_snapshot and object_path.read_bytes() == source, "replacement changed server data")
            if args.retry_after_promotion:
                with proxy.lock:
                    require(not proxy.abandon_operations and all(request == expected_request for request in proxy.restore_requests), "late request changed identity or sent an abandon")
            require(not proxy.failure.is_set(), "isolated proxy reported a failure")
        finally:
            if session_id and wd is not None:
                try:
                    wd("DELETE", f"/session/{session_id}")
                except Exception:
                    pass
            try:
                helpers.stop(driver)
            finally:
                try:
                    if proxy:
                        proxy.close()
                finally:
                    helpers.stop(server)

    scenario = "explicit retry after promotion" if args.retry_after_promotion else "explicit abandon"
    print(f"M3 reader trash WebView passed: 403, {scenario}, ACK, re-registration and source preservation.", flush=True)


if __name__ == "__main__":
    try:
        main()
    except TestFailure as failure:
        print(f"M3 trash WebView failed: {failure}", file=sys.stderr)
        raise SystemExit(1) from None
