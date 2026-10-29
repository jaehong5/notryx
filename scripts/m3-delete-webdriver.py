#!/usr/bin/env python3
"""Verify document deletion with synthetic data in an isolated WebView."""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import select
import sqlite3
import stat
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--repo", default=str(Path(__file__).resolve().parents[1]))
    parser.add_argument("--driver", default="WebKitWebDriver")
    parser.add_argument("--binary", default="target/debug/notryx-client")
    parser.add_argument("--server", default="target/debug/notryx-server")
    parser.add_argument("--reader", choices=("retry", "cancel"))
    parser.add_argument("--frozen", choices=("apply", "dismiss"))
    args = parser.parse_args()
    if args.reader and args.frozen:
        parser.error("--reader and --frozen must run separately")
    repo = Path(args.repo).resolve()
    spec = importlib.util.spec_from_file_location("m2_helpers", repo / "scripts/m2-workspace-webdriver.py")
    if spec is None or spec.loader is None:
        raise RuntimeError("WebDriver helpers missing")
    helpers = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(helpers)
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
    client_binary, server_binary = str((repo / args.binary).resolve()), str((repo / args.server).resolve())
    driver = server = None
    session_id = None
    wd = None

    def wait(check, description, timeout=60):
        until = time.monotonic() + timeout
        while time.monotonic() < until:
            result = check()
            if result:
                return result
            time.sleep(0.1)
        raise TestFailure("timed out waiting for " + description)

    with tempfile.TemporaryDirectory(prefix="notryx-m3-delete-webdriver-") as temporary:
        root = Path(temporary)
        notes, state, run, client, xdg_data, xdg_cache, xdg_config = (
            root / name for name in ("notes", "state", "run", "client", "xdg-data", "xdg-cache", "xdg-config")
        )
        for path in (notes, state, run, client, xdg_data, xdg_cache, xdg_config):
            path.mkdir(mode=0o700)
        port = helpers.free_port()
        config = root / "server.toml"
        config.write_text(f'[server]\nlisten = "127.0.0.1:{port}"\nadmin_socket = "{run}/admin.sock"\n[workspace]\nnotes_root = "{notes}"\nstate_root = "{state}"\n', encoding="utf-8")
        config.chmod(0o600)
        database = client / "drafts.sqlite3"
        source = b"\xef\xbb\xbf# Delete target\r\nSynthetic source\r\n"
        protected = b"# Protected\nSynthetic original\n"

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

        def node_for_name(name):
            if not database.exists():
                return None
            with sqlite3.connect(f"file:{database}?mode=ro", uri=True, timeout=5) as db:
                raw_nodes = db.execute("SELECT n.state FROM remote_nodes n JOIN client_registrations r ON r.id=n.registration_id WHERE r.status='active'").fetchall()
            return next((item for (raw,) in raw_nodes if (item := json.loads(raw)).get("name") == name and not item.get("deleted")), None)

        def delete_rows():
            if not database.exists():
                return []
            with sqlite3.connect(f"file:{database}?mode=ro", uri=True, timeout=5) as db:
                return db.execute("""
                    SELECT o.operation_id,o.operation_seq,o.document_id,o.expected_revision,o.status,
                           o.terminal_kind,o.receipt_json,s.ack_through,s.confirmed_through
                    FROM client_outbox o JOIN client_registrations r ON r.id=o.registration_id
                    JOIN outbox_streams s ON s.registration_id=r.id
                    WHERE r.status='active' AND o.operation_kind='delete' ORDER BY o.operation_seq
                """).fetchall()

        def all_delete_rows():
            with sqlite3.connect(f"file:{database}?mode=ro", uri=True, timeout=5) as db:
                return db.execute("""
                    SELECT r.id,r.status,o.operation_id,o.operation_seq,o.document_id,o.expected_revision,
                           o.status,o.terminal_kind,o.receipt_json
                    FROM client_outbox o JOIN client_registrations r ON r.id=o.registration_id
                    WHERE o.operation_kind='delete' ORDER BY r.id,o.operation_seq
                """).fetchall()

        def active_registration():
            with sqlite3.connect(f"file:{database}?mode=ro", uri=True, timeout=5) as db:
                return db.execute("SELECT id FROM client_registrations WHERE status='active'").fetchone()

        def checkpoint(node_id):
            with sqlite3.connect(f"file:{database}?mode=ro", uri=True, timeout=5) as db:
                return db.execute("""
                    SELECT c.generation,c.bytes,b.bytes,b.revision FROM registered_documents d
                    JOIN client_registrations r ON r.id=d.registration_id
                    JOIN draft_checkpoints c ON c.document_id=d.storage_id
                    JOIN draft_bases b ON b.document_id=d.storage_id
                    WHERE r.status='active' AND d.document_id=?
                """, (node_id,)).fetchone()

        try:
            cli("init", "--config", str(config))
            (notes / "Target.md").write_bytes(source)
            (notes / "Protected.md").write_bytes(protected)
            server = subprocess.Popen([server_binary, "serve", "--config", str(config)],
                stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True, start_new_session=True)
            ready, _, _ = select.select([server.stdout], [], [], 30)
            require(bool(ready) and server.stdout.readline().strip() == "server_started" and helpers.leader_running(server), "synthetic server did not start")
            status = admin("status")
            reader_id = admin("create-principal", "reader")["principal_id"] if args.reader or args.frozen else None
            grant = admin("issue-pairing", reader_id or status["owner_principal_id"])
            driver_port = helpers.free_port()
            origin = f"http://127.0.0.1:{driver_port}"
            opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
            environment = os.environ.copy()
            environment.update(TAURI_WEBVIEW_AUTOMATION="true", NOTRYX_M0_DATA_DIR=str(client),
                XDG_DATA_HOME=str(xdg_data), XDG_CACHE_HOME=str(xdg_cache), XDG_CONFIG_HOME=str(xdg_config))
            driver = subprocess.Popen([args.driver, f"--port={driver_port}", "--host=127.0.0.1"],
                env=environment, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True)

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
            target = wait(lambda: node_for_name("Target.md"), "target metadata")
            other = wait(lambda: node_for_name("Protected.md"), "protected metadata")
            wait(lambda: js("return !!document.querySelector('[data-delete-node=\"'+arguments[0]+'\"]:not(:disabled)')", target["node_id"]), "delete control")

            def open_delete(node_id):
                js("document.querySelector('[data-delete-node=\"'+arguments[0]+'\"]').click()", node_id)
                wait(lambda: js("return !!document.querySelector('[data-testid=delete-confirm]:not(:disabled)')"), "delete confirmation")

            open_delete(target["node_id"])
            require(js("return document.querySelector('[aria-label=\"문서 삭제\"]').textContent.includes('Target.md')"), "confirmation did not show the selected target")
            js("document.querySelector('[data-testid=delete-confirm-cancel]').click()")
            wait(lambda: js("return !document.querySelector('[aria-label=\"문서 삭제\"]')"), "confirmation cancellation")
            require(not delete_rows() and (notes / "Target.md").read_bytes() == source, "confirmation cancellation changed data")

            if args.reader or args.frozen:
                open_delete(target["node_id"])
                js("document.querySelector('[data-testid=delete-confirm]').click()")
                wait(lambda: js("return document.querySelector('[aria-label=\"문서 삭제\"]')?.textContent.includes('삭제 권한이 없습니다')"), "reader permission denial")
                pending = wait(lambda: (rows[0] if len(rows := delete_rows()) == 1 and rows[0][4] == 'sent' else None), "reader pending operation")
                require(pending[2] == target["node_id"] and pending[3] == target["node_etag"] and pending[5] is None and pending[6] is None, "permission denial changed the confirmed operation")
                require((notes / "Target.md").read_bytes() == source, "permission denial changed the original source")
                operation_id = pending[0]
                if args.frozen:
                    old_registration = active_registration()
                    original_rows = all_delete_rows()
                    require(old_registration is not None and len(original_rows) == 1 and original_rows[0][0] == old_registration[0], "original delete registration was not active")
                    admin("set-role", reader_id, "editor")
                    replacement_grant = admin("issue-pairing", reader_id)
                    js("""const form=document.querySelector('.workspace-connect');
                      form.elements.namedItem('origin').value=arguments[0];
                      form.elements.namedItem('workspace_id').value=arguments[1];
                      form.elements.namedItem('pairing_secret').value=arguments[2]; form.requestSubmit();""",
                      f"http://127.0.0.1:{port}", replacement_grant["workspace_id"], replacement_grant["pairing_secret"])
                    require(js("return document.querySelector('[name=pairing_secret]').value === ''"), "replacement pairing secret remained in form")
                    wait(lambda: (value if (value := active_registration()) and value != old_registration else None), "replacement registration")
                    wait(lambda: js("""const done=arguments[arguments.length-1];
                      window.__TAURI_INTERNALS__.invoke('m0_renderer_session')
                      .then(owner=>window.__TAURI_INTERNALS__.invoke('workspace_sync_status',{request:{renderer_session_id:owner}}))
                      .then(value=>done(value.status==='ready')).catch(()=>done(false));""", asynchronous=True), "replacement initial sync")
                    time.sleep(2)
                    frozen_rows = all_delete_rows()
                    require(len(frozen_rows) == 1 and frozen_rows[0][0] == old_registration[0] and frozen_rows[0][1] == 'frozen' and frozen_rows[0][2:] == original_rows[0][2:], "re-registration changed or replayed the old delete")
                    require((notes / "Target.md").read_bytes() == source, "re-registration automatically deleted the source")
                    wait(lambda: js("return [...document.querySelectorAll('button')].some(button=>button.textContent.trim()==='이전 삭제 요청 대조' && !button.disabled)"), "frozen delete review control")
                    js("[...document.querySelectorAll('button')].find(button=>button.textContent.trim()==='이전 삭제 요청 대조').click()")
                    wait(lambda: js("return !!document.querySelector('[data-frozen-delete=\"'+arguments[0]+'\"]:not(:disabled)')", operation_id), "frozen delete entry")
                    js("document.querySelector('[data-frozen-delete=\"'+arguments[0]+'\"]').click()", operation_id)
                    wait(lambda: js("return !!document.querySelector('[data-testid=frozen-delete-apply]')"), "frozen delete choices")
                    require(js("return document.querySelector('[aria-label=\"이전 삭제 요청 대조\"]')?.textContent.includes('현재 상태만으로 이전 요청의 결과를 확정할 수 없습니다')"), "frozen delete result was falsely inferred")
                    if args.frozen == "dismiss":
                        js("document.querySelector('[data-testid=frozen-delete-dismiss]').click()")
                        wait(lambda: js("return !!document.querySelector('[data-testid=frozen-delete-dismiss-confirm]:not(:disabled)')"), "frozen dismiss confirmation")
                        require(all_delete_rows() == frozen_rows, "review choice changed data before confirmation")
                        js("document.querySelector('[data-testid=frozen-delete-dismiss-confirm]').click()")
                        wait(lambda: js("return document.querySelector('[aria-label=\"이전 삭제 요청 대조\"]')?.textContent.includes('새 삭제 요청을 만들지 않았습니다')"), "frozen dismiss completion")
                        require(all_delete_rows() == frozen_rows and (notes / "Target.md").read_bytes() == source, "dismissal changed the old operation or server source")
                    else:
                        current_target = wait(lambda: node_for_name("Target.md"), "replacement current metadata")
                        js("document.querySelector('[data-testid=frozen-delete-apply]').click()")
                        wait(lambda: js("return !!document.querySelector('[data-testid=frozen-delete-apply-confirm]:not(:disabled)')"), "frozen apply confirmation")
                        require(all_delete_rows() == frozen_rows, "review choice changed data before confirmation")
                        js("document.querySelector('[data-testid=frozen-delete-apply-confirm]').click()")
                        def frozen_apply_completed():
                            rows = delete_rows()
                            return rows[0] if len(rows) == 1 and rows[0][4] == 'committed' and rows[0][5] is None and rows[0][6] is not None and rows[0][7] >= rows[0][1] and rows[0][8] >= rows[0][1] and not (notes / "Target.md").exists() else None
                        fresh = wait(frozen_apply_completed, "new deletion and acknowledgement")
                        require(fresh[0] != operation_id and fresh[2] == target["node_id"] and fresh[3] == current_target["node_etag"] and json.loads(fresh[6]).get("status") == "deleted", "new deletion did not use the confirmed current state")
                        require(all_delete_rows()[0] == frozen_rows[0], "new deletion changed the frozen operation")
                        require((state / "objects" / "sha256" / hashlib.sha256(source).hexdigest()).read_bytes() == source, "new deletion lost the original object")
                    require((notes / "Protected.md").read_bytes() == protected, "frozen review changed unrelated source")
                    print(f"M3 frozen delete WebView passed: re-registration, old operation preservation and explicit {args.frozen}.", flush=True)
                    return
                if args.reader == "retry":
                    admin("set-role", reader_id, "editor")
                    time.sleep(2)
                    require(delete_rows() == [pending] and (notes / "Target.md").read_bytes() == source, "permission promotion automatically changed the operation")
                    js("document.querySelector('[data-delete-retry=\"'+arguments[0]+'\"]').click()", operation_id)
                    def retry_completed():
                        rows = delete_rows()
                        return len(rows) == 1 and rows[0][4] == 'committed' and rows[0][5] is None and rows[0][6] is not None and rows[0][7] >= rows[0][1] and rows[0][8] >= rows[0][1] and not (notes / "Target.md").exists()
                    wait(retry_completed, "explicit retry and acknowledgement")
                    final = delete_rows()[0]
                    require(final[0:4] == pending[0:4] and json.loads(final[6]).get("status") == "deleted", "retry changed the operation or receipt")
                    require((state / "objects" / "sha256" / hashlib.sha256(source).hexdigest()).read_bytes() == source, "retried deletion lost the original object")
                else:
                    js("document.querySelector('[data-delete-cancel=\"'+arguments[0]+'\"]').click()", operation_id)
                    def cancellation_completed():
                        rows = delete_rows()
                        return len(rows) == 1 and rows[0][4] == 'committed' and rows[0][5] == 'abandoned' and rows[0][6] is not None and rows[0][7] >= rows[0][1] and rows[0][8] >= rows[0][1]
                    wait(cancellation_completed, "explicit cancellation and acknowledgement")
                    final = delete_rows()[0]
                    require(final[0:4] == pending[0:4] and json.loads(final[6]).get("status") == "abandoned", "cancellation changed the operation or receipt")
                    require((notes / "Target.md").read_bytes() == source, "cancelled deletion changed the original source")
                require((notes / "Protected.md").read_bytes() == protected, "reader operation changed unrelated source")
                print(f"M3 delete WebView passed: reader 403, explicit {args.reader}, receipt and source preservation.", flush=True)
                return

            js("document.querySelector('[data-document=\"'+arguments[0]+'\"]').click()", other["node_id"])
            wait(lambda: js("return document.querySelector('[aria-current=page]')?.dataset.document === arguments[0] && !!document.querySelector('.cm-content[contenteditable=true]')", other["node_id"]), "protected document editor")
            marker = "\n합성 편집\n"
            js("""const element=document.querySelector('.cm-content');
              element.dispatchEvent(new CompositionEvent('compositionstart',{bubbles:true}));
              const view=element.cmTile.root.view;
              view.dispatch({changes:{from:view.state.doc.length,insert:arguments[0]},userEvent:'input.type'});""", marker)
            saved = wait(lambda: (row if (row := checkpoint(other["node_id"])) and marker.encode() in row[1] else None), "protected durable draft")
            require(saved[2] == protected, "protected draft base changed")
            open_delete(other["node_id"])
            js("document.querySelector('[data-testid=delete-confirm]').click()")
            wait(lambda: js("return document.querySelector('[aria-label=\"문서 삭제\"]')?.textContent.includes('열린 문서')"), "open editor deletion rejection")
            require(not delete_rows() and checkpoint(other["node_id"]) == saved and (notes / "Protected.md").read_bytes() == protected, "rejected deletion changed protected data")
            js("document.querySelector('[data-testid=delete-confirm-cancel]').click()")
            wait(lambda: js("return !document.querySelector('[aria-label=\"문서 삭제\"]')"), "protected confirmation dismissal")

            open_delete(target["node_id"])
            js("document.querySelector('[data-testid=delete-confirm]').click()")
            queued = wait(lambda: (rows[0] if len(rows := delete_rows()) == 1 else None), "durable delete request")
            require(queued[2] == target["node_id"] and queued[3] == target["node_etag"] and queued[4] == 'queued', "delete request did not preserve the confirmed target and ETag")
            require((notes / "Target.md").read_bytes() == source and checkpoint(other["node_id"]) == saved, "queued delete changed source or protected draft")
            js("document.querySelector('.cm-content').dispatchEvent(new CompositionEvent('compositionend',{bubbles:true}))")

            def deleted_and_acked():
                rows = delete_rows()
                return len(rows) == 1 and rows[0][4] == 'committed' and rows[0][5] is None and rows[0][6] is not None and rows[0][7] >= rows[0][1] and rows[0][8] >= rows[0][1] and not (notes / "Target.md").exists()
            wait(deleted_and_acked, "delete commit and receipt acknowledgement")
            final = delete_rows()[0]
            receipt = json.loads(final[6])
            require(final[0:4] == queued[0:4] and receipt.get("status") == "deleted" and receipt.get("node", {}).get("node_id") == target["node_id"], "delete receipt did not match the confirmed operation")
            wait(lambda: js("return document.querySelector('[aria-label=\"문서 삭제\"]')?.textContent.includes('휴지통으로 이동 완료')"), "delete completion status")
            preserved = state / "objects" / "sha256" / hashlib.sha256(source).hexdigest()
            require(preserved.read_bytes() == source, "deleted original object was not preserved")
            final_draft = checkpoint(other["node_id"])
            require(final_draft is not None and marker.encode() in final_draft[1], "protected draft was lost")
            js("[...document.querySelector('[aria-label=\"문서 삭제\"]').querySelectorAll('button')].find(button=>button.textContent.trim()==='닫기').click()")
            wait(lambda: js("return !document.querySelector('[aria-label=\"문서 삭제\"]')"), "delete confirmation close")
            wait(lambda: js("return !document.querySelector('[data-delete-node=\"'+arguments[0]+'\"]')", target["node_id"]), "deleted document removed from list")
            js("document.querySelector('[data-testid=delete-requests-toggle]').click()")
            wait(lambda: js("return document.querySelector('[aria-label=\"문서 삭제\"]')?.textContent.includes('휴지통으로 이동 완료')"), "independent delete request status")
            js("document.querySelector('[data-testid=close-document]').click()")
            wait(lambda: js("return !document.querySelector('.cm-content')"), "explicit document close")
            retained = checkpoint(other["node_id"])
            require((retained is not None and marker.encode() in retained[1]) or marker.encode() in (notes / "Protected.md").read_bytes(), "closing the document lost the protected edit")
            print("M3 delete WebView passed: confirmation cancel, open dirty protection, pinned ETag, delete receipt, independent status, document close and source preservation.", flush=True)
        finally:
            if session_id and wd:
                try:
                    wd("DELETE", f"/session/{session_id}")
                except Exception:
                    pass
            try:
                helpers.stop(driver)
            finally:
                helpers.stop(server)


if __name__ == "__main__":
    try:
        main()
    except Exception as failure:
        detail = str(failure) if type(failure).__name__ == "TestFailure" else type(failure).__name__
        print(f"M3 delete WebView failed: {detail}", file=sys.stderr, flush=True)
        raise SystemExit(1) from None
