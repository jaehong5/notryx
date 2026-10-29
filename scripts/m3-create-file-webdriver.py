#!/usr/bin/env python3
"""Check empty Markdown creation and offline recovery in an isolated WebView."""
import argparse
import base64
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


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--repo", default=str(Path(__file__).resolve().parents[1]))
    parser.add_argument("--driver", default="WebKitWebDriver")
    parser.add_argument("--binary", default="target/debug/notryx-client")
    parser.add_argument("--server", default="target/debug/notryx-server")
    parser.add_argument("--old-binary", default="/tmp/notryx-pre-file-create-client")
    args = parser.parse_args()
    repo = Path(args.repo).resolve()
    spec = importlib.util.spec_from_file_location("m2_helpers", repo / "scripts/m2-workspace-webdriver.py")
    if spec is None or spec.loader is None:
        raise RuntimeError("WebDriver helpers missing")
    helpers = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(helpers)
    require = helpers.require
    TestFailure = helpers.TestFailure
    require(helpers.xvfb_on_current_display(), "isolated Xvfb required")
    bus = os.environ.get("DBUS_SESSION_BUS_ADDRESS", "")
    prefix = os.environ.get("NOTRYX_TEST_SECRET_BUS_PREFIX", "")
    require(prefix.startswith("/tmp/notryx-secret-test.") and bus.startswith("unix:path=" + prefix + "/bus.sock"), "isolated no-service session bus required")
    client_binary = str((repo / args.binary).resolve())
    server_binary = str((repo / args.server).resolve())
    old_binary = str(Path(args.old_binary).resolve())
    driver = server = proxy = None
    sys.path.insert(0, str(repo / "scripts"))
    from m3_trash_proxy import TrashProxy
    session_id = None
    wd = None
    screenshots = Path(tempfile.mkdtemp(prefix="notryx-m3-create-visual-"))

    def wait(check, description, timeout=60):
        until = time.monotonic() + timeout
        while time.monotonic() < until:
            result = check()
            if result:
                return result
            time.sleep(0.1)
        raise TestFailure("timed out waiting for " + description)

    with tempfile.TemporaryDirectory(prefix="notryx-m3-create-webdriver-") as temporary:
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

        def rows():
            if not database.exists():
                return []
            with sqlite3.connect(f"file:{database}?mode=ro", uri=True, timeout=5) as db:
                return db.execute("""
                    SELECT o.operation_id,o.operation_seq,o.document_id,o.status,o.abandon_state,
                           o.terminal_kind,o.receipt_json,o.bytes,r.id,r.device_stream_id,r.workspace_epoch,
                           s.ack_through,s.confirmed_through
                    FROM client_outbox o JOIN client_registrations r ON r.id=o.registration_id
                    JOIN outbox_streams s ON s.registration_id=r.id
                    WHERE r.status='active' AND o.operation_kind='create' ORDER BY o.operation_seq
                """).fetchall()

        def root_id():
            if not database.exists():
                return None
            with sqlite3.connect(f"file:{database}?mode=ro", uri=True, timeout=5) as db:
                stored = db.execute("SELECT n.state FROM remote_nodes n JOIN client_registrations r ON r.id=n.registration_id WHERE r.status='active'").fetchall()
            for (raw,) in stored:
                node = json.loads(raw)
                if node.get("parent_id") is None and node.get("kind") == "folder" and not node.get("deleted"):
                    return node["node_id"]
            return None

        try:
            cli("init", "--config", str(config))
            server = subprocess.Popen([server_binary, "serve", "--config", str(config)],
                stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True, start_new_session=True)
            ready, _, _ = select.select([server.stdout], [], [], 30)
            require(bool(ready) and server.stdout.readline().strip() == "server_started" and helpers.leader_running(server), "synthetic server did not start")
            status = admin("status")
            grant = admin("issue-pairing", status["owner_principal_id"])
            proxy = TrashProxy(port, "unused")
            proxy.restore_path = "/v1/nodes"
            proxy.start()
            driver_port = helpers.free_port()
            origin = f"http://127.0.0.1:{driver_port}"
            opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
            env = os.environ.copy()
            env.update(TAURI_WEBVIEW_AUTOMATION="true", NOTRYX_M0_DATA_DIR=str(client),
                XDG_DATA_HOME=str(xdg_data), XDG_CACHE_HOME=str(xdg_cache), XDG_CONFIG_HOME=str(xdg_config))
            driver = subprocess.Popen([args.driver, f"--port={driver_port}", "--host=127.0.0.1"],
                env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True)

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
            def start_client():
                identifier = wd("POST", "/session", capabilities)["sessionId"]
                wd("POST", f"/session/{identifier}/timeouts", {"script": 30000})
                return identifier
            session_id = start_client()
            def js(script, *values, asynchronous=False):
                return wd("POST", f"/session/{session_id}/execute/" + ("async" if asynchronous else "sync"), {"script": script, "args": list(values)})
            wait(lambda: js("return !!document.querySelector('.workspace-connect button:not(:disabled)')"), "pairing form")
            js("""const form=document.querySelector('.workspace-connect');
              form.elements.namedItem('origin').value=arguments[0];
              form.elements.namedItem('workspace_id').value=arguments[1];
              form.elements.namedItem('pairing_secret').value=arguments[2]; form.requestSubmit();""",
              f"http://127.0.0.1:{proxy.server_address[1]}", grant["workspace_id"], grant["pairing_secret"])
            require(js("return document.querySelector('[name=pairing_secret]').value === ''"), "pairing secret remained in form")
            wait(lambda: js("""const done=arguments[arguments.length-1];
              window.__TAURI_INTERNALS__.invoke('m0_renderer_session')
              .then(owner=>window.__TAURI_INTERNALS__.invoke('workspace_sync_status',{request:{renderer_session_id:owner}}))
              .then(value=>done(value.status==='ready')).catch(()=>done(false));""", asynchronous=True), "initial sync")
            parent = wait(root_id, "cached root")

            def open_panel():
                wait(lambda: js("return !!document.querySelector('[aria-label=\"새 폴더 또는 문서\"]') || [...document.querySelectorAll('button')].some(x=>x.textContent.trim()==='새 폴더 또는 문서' && !x.disabled)"), "create control")
                js("if(!document.querySelector('[aria-label=\"새 폴더 또는 문서\"]')) [...document.querySelectorAll('button')].find(x=>x.textContent.trim()==='새 폴더 또는 문서').click()")
                wait(lambda: js("return !!document.querySelector('[data-testid=create-parent] option[value=\"'+arguments[0]+'\"]')", parent), "cached parent")
            def visual_check(label):
                info = js("""const panel=document.querySelector('[aria-label=\"새 폴더 또는 문서\"]');
                  if (!panel) return null;
                  const selectors=['[data-testid=create-parent]','[data-testid=create-kind]','[data-testid=create-name]','[data-testid=create-submit]'];
                  const controls=selectors.map(selector=>panel.querySelector(selector));
                  const visible=controls.every(element=>{ if (!element) return false; const rect=element.getBoundingClientRect(); const style=getComputedStyle(element); return rect.width>0 && rect.height>0 && style.display!=='none' && style.visibility!=='hidden'; });
                  return {visible, overflow:panel.scrollWidth>panel.clientWidth+1, panelWidth:panel.clientWidth, panelScrollWidth:panel.scrollWidth, viewportWidth:window.innerWidth, controls:controls.map(element=>element ? {width:element.getBoundingClientRect().width, left:element.getBoundingClientRect().left, right:element.getBoundingClientRect().right} : null)};""")
                js("document.querySelector('[aria-label=\"새 폴더 또는 문서\"]').scrollIntoView({block:'start'})")
                encoded = wd("GET", f"/session/{session_id}/screenshot")
                screenshot = screenshots / (label + ".png")
                screenshot.write_bytes(base64.b64decode(encoded, validate=True))
                if not info or not info["visible"] or info["overflow"]:
                    print(f"Synthetic create panel metrics: {json.dumps(info)}; screenshot: {screenshot}", flush=True)
                    raise TestFailure("create panel controls are hidden or overflow horizontally")
            def create(name):
                open_panel()
                js("""const select=document.querySelector('[data-testid=create-parent]'); select.value=arguments[0]; select.dispatchEvent(new Event('change',{bubbles:true}));
                  const kind=document.querySelector('[data-testid=create-kind]'); kind.value='file'; kind.dispatchEvent(new Event('change',{bubbles:true}));
                  const input=document.querySelector('[data-testid=create-name]'); Object.getOwnPropertyDescriptor(HTMLInputElement.prototype,'value').set.call(input,arguments[1]); input.dispatchEvent(new Event('input',{bubbles:true}));""", parent, name)
                wait(lambda: js("return !!document.querySelector('[data-testid=create-submit]:not(:disabled)')"), "create submit")
                js("const submit=document.querySelector('[data-testid=create-submit]'); submit.click(); submit.click()")
            create("Online.md")
            def online_committed():
                found = rows()
                return found[0] if len(found) == 1 and found[0][3] == "committed" and found[0][6] and found[0][11] >= 1 and found[0][12] >= 1 else None
            online = wait(online_committed, "online created receipt and ACK")
            visual_check("01-online-created")
            require((notes / "Online.md").is_file() and (notes / "Online.md").read_bytes() == b"", "server empty document not created")
            receipt = json.loads(online[6]); require(receipt.get("status") == "created" and receipt.get("operation_id") == online[0], "invalid create receipt")
            wait(lambda: js("return !!document.querySelector('[data-document=\"'+arguments[0]+'\"]:not(:disabled)')", online[2]), "created document sync")
            js("document.querySelector('[data-document=\"'+arguments[0]+'\"]').click()", online[2])
            wait(lambda: js("return document.querySelector('[aria-current=page]')?.dataset.document === arguments[0] && document.querySelector('.cm-content[contenteditable=true]')?.cmTile.root.view.state.doc.toString() === ''", online[2]), "open empty document")
            text = "# 합성 문서\n첫 저장\n"
            js("const view=document.querySelector('.cm-content').cmTile.root.view; view.dispatch({changes:{from:0,insert:arguments[0]},userEvent:'input.type'}); document.querySelector('#save').click();", text)
            def edit_committed():
                with sqlite3.connect(f"file:{database}?mode=ro", uri=True, timeout=5) as db:
                    row = db.execute("""SELECT o.operation_seq,s.ack_through FROM client_outbox o JOIN client_registrations r ON r.id=o.registration_id JOIN outbox_streams s ON s.registration_id=r.id WHERE r.status='active' AND o.operation_kind='content' AND o.document_id=? AND o.status='committed' ORDER BY o.operation_seq DESC LIMIT 1""", (online[2],)).fetchone()
                return row is not None and row[1] >= row[0] and (notes / "Online.md").read_bytes() == text.encode("utf-8")
            wait(edit_committed, "created document edit and ACK")
            online = rows()[0]
            helpers.stop(server); server = None
            wait(lambda: js("return document.body.textContent.includes('서버에 연결되지 않음')"), "offline state")
            create("Offline.md")
            def queued_offline():
                found = rows()
                return found[1] if len(found) == 2 and found[1][3] == "queued" and found[1][4] == "none" and found[1][6] is None else None
            pending = wait(queued_offline, "durable offline create")
            visual_check("02-offline-queued")
            require(not (notes / "Offline.md").exists(), "offline create touched server files")
            wd("DELETE", f"/session/{session_id}"); session_id = None
            session_id = start_client()
            wait(lambda: js("return document.body.textContent.includes('서버에 연결되지 않음')"), "offline restart")
            after = wait(queued_offline, "pending create after restart")
            open_panel()
            require(js("return document.querySelector('[aria-label=\"새 폴더 또는 문서\"]').textContent.includes('서버 반영과 동기화 후 열 수 있습니다')"), "offline file availability not explained")
            wd("DELETE", f"/session/{session_id}"); session_id = None
            with sqlite3.connect(f"file:{database}?mode=ro", uri=True, timeout=5) as db:
                version_before = db.execute("PRAGMA user_version").fetchone()[0]
            require(version_before > 13, "expected newer client schema")
            server = subprocess.Popen([server_binary, "serve", "--config", str(config)], stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True, start_new_session=True)
            ready, _, _ = select.select([server.stdout], [], [], 30)
            require(bool(ready) and server.stdout.readline().strip() == "server_started" and helpers.leader_running(server), "synthetic server did not restart")
            admin("status")
            try:
                opener.open(f"http://127.0.0.1:{proxy.server_address[1]}/v1/server-info", timeout=5).close()
                raise TestFailure("unauthenticated server probe unexpectedly succeeded")
            except urllib.error.HTTPError as error:
                require(error.code == 401, "synthetic server probe failed")
            def proxy_idle():
                with proxy.lock:
                    return not proxy.connections
            wait(proxy_idle, "proxy idle before old client")
            with proxy.lock:
                original_requests = tuple(proxy.restore_requests)
                original_events = tuple(proxy.events)
                original_acknowledgements = tuple(proxy.acknowledgements)
                original_abandons = tuple(proxy.abandon_operations)
            require(original_requests and original_requests[0][0] == online[0], "proxy did not observe original creation")
            proxy.failure.clear()
            capabilities["capabilities"]["alwaysMatch"]["webkitgtk:browserOptions"]["binary"] = old_binary
            session_id = start_client()
            wait(lambda: js("return !!document.querySelector('[data-testid=\"migration-recovery\"]')"), "old client recovery")
            require(js("return document.querySelector('[data-testid=\"migration-recovery\"]').textContent.includes('서버 전송을 중지')"), "old client recovery did not block sending")
            end = time.monotonic() + 2
            while time.monotonic() < end:
                require(js("return !!document.querySelector('[data-testid=\"migration-recovery\"]')"), "old client left recovery")
                time.sleep(0.1)
            require(wait(queued_offline, "old client preserved pending file") == pending, "old client changed pending file")
            with proxy.lock:
                require(tuple(proxy.restore_requests) == original_requests, "old client sent pending file")
                require(tuple(proxy.events) == original_events and tuple(proxy.acknowledgements) == original_acknowledgements and tuple(proxy.abandon_operations) == original_abandons and not proxy.connections, "old client made network requests")
            require(helpers.leader_running(server) and not proxy.failure.is_set(), "proxy failed during old client recovery")
            require((notes / "Online.md").read_bytes() == text.encode("utf-8") and not (notes / "Offline.md").exists(), "old client changed server files")
            with sqlite3.connect(f"file:{database}?mode=ro", uri=True, timeout=5) as db:
                require(db.execute("PRAGMA user_version").fetchone()[0] == version_before, "old client changed schema version")
            wd("DELETE", f"/session/{session_id}"); session_id = None
            helpers.stop(server); server = None
            capabilities["capabilities"]["alwaysMatch"]["webkitgtk:browserOptions"]["binary"] = client_binary
            session_id = start_client()
            wait(lambda: js("return document.body.textContent.includes('서버에 연결되지 않음')"), "new client offline restart")
            after = wait(queued_offline, "pending create after restart")
            require((after[0], after[1], after[2], after[7], after[8:11]) == (pending[0], pending[1], pending[2], pending[7], pending[8:11]), "restart changed create identity, payload or binding")
            open_panel()
            wait(lambda: js("return !!document.querySelector('[data-create-cancel=\"'+arguments[0]+'\"]:not(:disabled)')", pending[0]), "offline cancel control")
            js("document.querySelector('[data-create-cancel=\"'+arguments[0]+'\"]').click()", pending[0])
            wait(lambda: js("return document.querySelector('[aria-label=\"새 폴더 또는 문서\"]')?.textContent.includes('취소 처리 중')"), "offline cancellation pending")
            def cancellation_requested():
                found = rows()
                return found[1] if len(found) == 2 and found[1][0] == pending[0] and found[1][3] == "queued" and found[1][4] == "requested" and found[1][6] is None else None
            after_cancel = wait(cancellation_requested, "durable offline cancellation intent")
            require((after_cancel[0], after_cancel[1], after_cancel[2], after_cancel[7], after_cancel[8:11]) == (pending[0], pending[1], pending[2], pending[7], pending[8:11]), "offline cancellation changed request identity, payload or binding")
            require(after_cancel[11] == online[11] and after_cancel[12] == online[12], "offline cancellation changed receipt watermark")
            visual_check("03-offline-cancellation-requested")
            wd("DELETE", f"/session/{session_id}"); session_id = None
            session_id = start_client()
            wait(lambda: js("return document.body.textContent.includes('서버에 연결되지 않음')"), "second offline restart")
            require(wait(cancellation_requested, "cancellation intent after restart")[0] == pending[0], "restart changed cancellation intent")
            open_panel()
            wait(lambda: js("return document.querySelector('[aria-label=\"새 폴더 또는 문서\"]')?.textContent.includes('취소 처리 중')"), "pending cancellation after restart")
            visual_check("04-offline-cancellation-after-restart")
            require((notes / "Online.md").is_file() and (notes / "Online.md").read_bytes() == text.encode("utf-8") and not (notes / "Offline.md").exists(), "offline work altered synthetic documents")
        finally:
            if session_id and wd is not None:
                try: wd("DELETE", f"/session/{session_id}")
                except Exception: pass
            try: helpers.stop(driver)
            finally:
                try: helpers.stop(server)
                finally:
                    if proxy: proxy.close()
    print(f"M3 file create WebView passed: online empty file, editor save and ACK, offline queue, previous client recovery and preservation and persistent cancellation intent. Screenshots: {screenshots}", flush=True)


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        safe_messages = {
            "isolated Xvfb required", "isolated no-service session bus required",
            "synthetic server command failed", "invalid synthetic admin response",
            "synthetic server did not start", "WebDriver returned an error", "WebDriver request failed",
            "pairing secret remained in form", "create panel controls are hidden or overflow horizontally",
            "server empty document not created", "invalid create receipt", "offline create touched server files",
            "restart changed create identity, payload or binding", "offline cancellation changed request identity, payload or binding",
            "offline file availability not explained", "old client changed pending file",
            "expected newer client schema", "synthetic server did not restart", "old client recovery did not block sending",
            "old client left recovery", "old client sent pending file", "old client made network requests", "proxy failed during old client recovery",
            "unauthenticated server probe unexpectedly succeeded", "synthetic server probe failed", "proxy did not observe original creation",
            "old client changed server files", "old client changed schema version",
            "offline cancellation changed receipt watermark", "restart changed cancellation intent",
            "offline work altered synthetic documents",
        }
        wait_stages = {
            "WebDriver startup", "WebDriver status", "pairing form", "initial sync", "cached root",
            "create control", "cached parent", "create submit", "online created receipt and ACK",
            "offline state", "durable offline create", "offline restart", "pending create after restart",
            "old client recovery", "old client preserved pending file", "new client offline restart", "proxy idle before old client",
            "created document sync", "open empty document", "created document edit and ACK",
            "offline cancel control", "offline cancellation pending", "durable offline cancellation intent",
            "second offline restart", "cancellation intent after restart", "pending cancellation after restart",
        }
        detail = str(error)
        if type(error).__name__ == "TestFailure" and (detail in safe_messages or any(detail == f"timed out waiting for {stage}" for stage in wait_stages)):
            print(f"M3 create WebView failed: {detail}", file=sys.stderr)
        else:
            print(f"M3 create WebView failed: {type(error).__name__}", file=sys.stderr)
        raise SystemExit(1) from None
