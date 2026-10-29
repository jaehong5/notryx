#!/usr/bin/env python3
"""Draft: real reader 403, re-registration and frozen create review."""
import argparse
import hashlib
import http.client
import importlib.util
import json
import os
import re
import threading
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
    expected_bus = "unix:path=" + prefix + "/bus.sock"
    require(Path(prefix).parent == Path("/tmp") and re.fullmatch(r"notryx-secret-test\.[A-Za-z0-9]{8}", Path(prefix).name) is not None
        and (bus == expected_bus or bus.startswith(expected_bus + ",")), "isolated no-service session bus required")
    client_binary = str((repo / args.binary).resolve())
    server_binary = str((repo / args.server).resolve())
    driver = server = proxy = None
    session_id = None
    wd = None

    sys.path.insert(0, str(repo / "scripts"))
    from m3_trash_proxy import TrashProxy

    def wait(check, description, timeout=60):
        until = time.monotonic() + timeout
        while time.monotonic() < until:
            result = check()
            if result:
                return result
            time.sleep(0.1)
        raise TestFailure("timed out waiting for " + description)

    with tempfile.TemporaryDirectory(prefix="notryx-m3-frozen-webdriver-", dir="/tmp") as temporary:
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
                           s.ack_through,s.confirmed_through,r.status,r.device_id,o.payload_hash
                    FROM client_outbox o JOIN client_registrations r ON r.id=o.registration_id
                    JOIN outbox_streams s ON s.registration_id=r.id
                    WHERE o.operation_kind='create' ORDER BY r.id,o.operation_seq
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
            def server_request(method, path, body=None, headers=None):
                connection = http.client.HTTPConnection("127.0.0.1", port, timeout=20)
                try:
                    connection.request(method, path, body, headers or {})
                    response = connection.getresponse()
                    data = response.read(1024 * 1024 + 1)
                    require(response.status == 200 and len(data) <= 1024 * 1024, "synthetic setup request failed")
                    return json.loads(data) if data else None
                except (OSError, http.client.HTTPException, ValueError):
                    raise TestFailure("synthetic setup request failed") from None
                finally: connection.close()
            seed_grant = admin("issue-pairing", status["owner_principal_id"])
            seed_pair = server_request("POST", "/v1/pair", json.dumps({
                "protocol":"mdw-sync/1.2", "workspace_id":seed_grant["workspace_id"], "pairing_secret":seed_grant["pairing_secret"]
            }).encode(), {"Content-Type":"application/json"})
            authorization = {"Authorization":f"Bearer {seed_pair['device_token']}"}
            info = server_request("GET", "/v1/server-info", headers=authorization)
            seed_node = helpers.operation_id()
            server_request("POST", "/v1/nodes", json.dumps({"node_id":seed_node,"parent_id":info["root_id"],"name":"Discard Folder","kind":"folder"}).encode(), {
                **authorization, "Content-Type":"application/json", "X-Workspace-Epoch":info["workspace_epoch"],
                "X-Device-Stream":seed_pair["device_stream_id"], "X-Operation-Seq":"1", "Idempotency-Key":helpers.operation_id(),
            })
            require((notes / "Discard Folder").is_dir(), "synthetic existing folder missing")
            principal = admin("create-principal", "reader")
            reader_grant = admin("issue-pairing", principal["principal_id"])
            class PhaseFailures:
                def __init__(self):
                    self.lock = threading.Lock()
                    self.phase = "before_outage"
                    self.counts = {"before_outage": 0, "intentional_outage": 0, "after_restart": 0}
                def set(self):
                    with self.lock: self.counts[self.phase] += 1
                def mark(self, phase):
                    with self.lock: self.phase = phase
                def count(self, phase):
                    with self.lock: return self.counts[phase]
            proxy = TrashProxy(port, "unused")
            proxy.restore_path = "/v1/nodes"
            proxy.failure = PhaseFailures()
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
                try: return wd("GET", "/status")
                except TestFailure: return False
            wait(driver_ready, "WebDriver status")
            capabilities = {"capabilities": {"alwaysMatch": {"browserName":"wry", "webkitgtk:browserOptions":{"binary":client_binary}}}}
            session_id = wd("POST", "/session", capabilities)["sessionId"]
            wd("POST", f"/session/{session_id}/timeouts", {"script":30000})
            def js(script, *values, asynchronous=False):
                return wd("POST", f"/session/{session_id}/execute/" + ("async" if asynchronous else "sync"), {"script":script, "args":list(values)})
            def pair(grant):
                wait(lambda: js("return !!document.querySelector('.workspace-connect button:not(:disabled)')"), "pairing form")
                js("""const form=document.querySelector('.workspace-connect');
                  form.elements.namedItem('origin').value=arguments[0];
                  form.elements.namedItem('workspace_id').value=arguments[1];
                  form.elements.namedItem('pairing_secret').value=arguments[2]; form.requestSubmit();""",
                  f"http://127.0.0.1:{proxy.server_address[1]}", grant["workspace_id"], grant["pairing_secret"])
                require(js("return document.querySelector('[name=pairing_secret]').value === ''"), "pairing secret remained in form")
            def sync_ready():
                return js("""const done=arguments[arguments.length-1];
                  window.__TAURI_INTERNALS__.invoke('m0_renderer_session')
                  .then(owner=>window.__TAURI_INTERNALS__.invoke('workspace_sync_status',{request:{renderer_session_id:owner}}))
                  .then(value=>done(value.status==='ready')).catch(()=>done(false));""", asynchronous=True)
            def server_snapshot():
                with sqlite3.connect(f"file:{state / 'state.db'}?mode=ro", uri=True, timeout=5) as db:
                    head = db.execute("SELECT head_seq FROM workspaces WHERE singleton=1").fetchone()
                    nodes = db.execute("SELECT node_id,parent_id,name,kind,deleted FROM nodes ORDER BY node_id").fetchall()
                    changes = db.execute("SELECT seq,operation_id FROM change_sets ORDER BY CAST(seq AS INTEGER)").fetchall()
                    receipts = db.execute("SELECT operation_id FROM operation_receipts ORDER BY operation_id").fetchall()
                return head, nodes, changes, receipts
            def root_parent():
                return root_id()
            def open_panel():
                wait(lambda: js("return !!document.querySelector('[aria-label=\"새 폴더 또는 문서\"]') || [...document.querySelectorAll('button')].some(x=>x.textContent.trim()==='새 폴더 또는 문서' && !x.disabled)"), "create control")
                js("if(!document.querySelector('[aria-label=\"새 폴더 또는 문서\"]')) [...document.querySelectorAll('button')].find(x=>x.textContent.trim()==='새 폴더 또는 문서').click()")
                wait(lambda: js("return !!document.querySelector('[data-testid=create-parent] option[value=\"'+arguments[0]+'\"]')", parent), "root destination")
            def create(name):
                open_panel()
                js("""const select=document.querySelector('[data-testid=create-parent]'); select.value=arguments[0]; select.dispatchEvent(new Event('change',{bubbles:true}));
                   const input=document.querySelector('[data-testid=create-name]'); Object.getOwnPropertyDescriptor(HTMLInputElement.prototype,'value').set.call(input,arguments[1]); input.dispatchEvent(new Event('input',{bubbles:true}));""", parent, name)
                wait(lambda: js("return !!document.querySelector('[data-testid=create-submit]:not(:disabled)')"), "create submit")
                js("document.querySelector('[data-testid=create-submit]').click()")
            def decisions():
                with sqlite3.connect(f"file:{database}?mode=ro", uri=True, timeout=5) as db:
                    return db.execute("SELECT old_operation_id,decision,new_operation_id FROM create_reconciliations ORDER BY old_operation_id").fetchall()
            def open_review():
                wait(lambda: js("return !!document.querySelector('[aria-label=\"이전 생성 요청 대조\"]') || [...document.querySelectorAll('button')].some(x=>x.textContent.trim()==='이전 생성 요청 대조' && !x.disabled)"), "review control")
                js("if(!document.querySelector('[aria-label=\"이전 생성 요청 대조\"]')) [...document.querySelectorAll('button')].find(x=>x.textContent.trim()==='이전 생성 요청 대조').click()")
                wait(lambda: js("return !!document.querySelector('[aria-label=\"이전 생성 요청 대조\"]')"), "review panel")
            def valid_v7(value):
                import uuid
                try: return str(uuid.UUID(value)) == value and uuid.UUID(value).version == 7
                except (ValueError, AttributeError): return False
            def same_original(row, saved):
                return row[:11] == saved[:11] and row[13:] == ("frozen", saved[14], saved[15])

            pair(reader_grant)
            wait(sync_ready, "reader initial sync")
            parent = wait(root_parent, "cached root")
            before_server = server_snapshot()
            create("Uncertain Folder")
            wait(lambda: proxy.count("restore", 403) > 0, "actual reader create 403")
            first = wait(lambda: (r[0] if len(r := rows()) == 1 and r[0][3] == "sent" and r[0][6] is None else None), "sent unconfirmed create")
            wait(lambda: js("return document.querySelector('[aria-label=\"새 폴더 또는 문서\"]')?.textContent.includes('생성 권한이 없습니다')"), "reader permission notice")
            with proxy.lock:
                require(proxy.restore_operation == first[0] and len(proxy.restore_requests) >= 1, "denied request identity disagreed")
                first_wire = tuple(proxy.restore_requests)
                require(all(entry[1:3] == ("1", first[9]) and entry[3] == hashlib.sha256(first[7]).digest() for entry in first_wire), "denied request payload or identity mismatch")
            create("Discard Folder")
            second = wait(lambda: (r[1] if len(r := rows()) == 2 and r[1][3] == "queued" and r[1][6] is None else None), "second queued create")
            require((first[1], second[1]) == (1, 2) and first[8:11] == second[8:11], "old FIFO or binding mismatch")
            require(all(row[15] == "sha256:" + hashlib.sha256(row[7]).hexdigest() for row in (first, second)), "old payload hash mismatch")
            require(server_snapshot() == before_server and not (notes / "Uncertain Folder").exists() and (notes / "Discard Folder").is_dir(), "denied or queued request changed server")
            owner_grant = admin("issue-pairing", status["owner_principal_id"])
            pair(owner_grant)
            def replaced():
                current = rows()
                return len(current) == 2 and all(row[13] == "frozen" for row in current)
            wait(replaced, "frozen old registration")
            wait(sync_ready, "owner bootstrap")
            wait(lambda: root_parent() == parent, "owner local index")
            after_pair = rows()
            require(same_original(after_pair[0], first) and same_original(after_pair[1], second), "re-registration changed original requests")
            require(server_snapshot() == before_server and decisions() == [], "re-registration applied or decided a request")
            with proxy.lock:
                require(tuple(proxy.restore_requests) == first_wire, "previous stream was resent")
                proxy.restore_operation = None
            open_review()
            wait(lambda: js("return !!document.querySelector('[data-review-operation=\"'+arguments[0]+'\"]') && !!document.querySelector('[data-review-operation=\"'+arguments[1]+'\"]')", first[0], second[0]), "both frozen requests")
            require(js("return document.querySelector('[data-review-operation=\"'+arguments[0]+'\"]').textContent.includes('결과 미확정')", first[0]), "sent request shown as confirmed")
            require(server_snapshot() == before_server and decisions() == [], "review applied a request without user action")
            require(proxy.failure.count("before_outage") == 0, "proxy failed before controlled outage")
            proxy.failure.mark("intentional_outage")
            helpers.stop(server); server = None
            wait(lambda: js("return document.body.textContent.includes('서버에 연결되지 않음')"), "isolated server stopped")
            js("document.querySelector('[data-review-operation=\"'+arguments[0]+'\"]').click()", first[0])
            wait(lambda: js("return !!document.querySelector('[data-testid=review-parent] option[value=\"'+arguments[0]+'\"]')", parent), "review destination")
            js("""const select=document.querySelector('[data-testid=review-parent]'); select.value=arguments[0]; select.dispatchEvent(new Event('change',{bubbles:true}));
              const input=document.querySelector('[data-testid=review-name]'); Object.getOwnPropertyDescriptor(HTMLInputElement.prototype,'value').set.call(input,arguments[1]); input.dispatchEvent(new Event('input',{bubbles:true}));""", parent, "Applied Folder")
            wait(lambda: js("return !!document.querySelector('[data-testid=review-apply]:not(:disabled)')"), "review apply")
            js("document.querySelector('[data-testid=review-apply]').click()")
            def applied():
                entries = decisions(); result = rows()
                match = [entry for entry in entries if entry[0] == first[0] and entry[1] == "applied"]
                active = [row for row in result if row[13] == "active"]
                return (match[0], active[0]) if len(match) == 1 and len(active) == 1 and match[0][2] == active[0][0] and active[0][3] == "queued" else None
            decision, new = wait(applied, "new queued operation")
            require(new[1] == 1 and new[9] != first[9] and new[8] != first[8], "new stream or sequence incorrect")
            require(valid_v7(new[0]) and valid_v7(new[2]) and new[0] != first[0] and new[2] != first[2], "new identities invalid")
            payload = json.loads(new[7]); require(payload.get("node_id") == new[2] and payload.get("parent_id") == parent and payload.get("name") == "Applied Folder" and payload.get("kind") == "folder", "new request incorrect")
            require(new[6] is None and not (notes / "Applied Folder").exists() and server_snapshot() == before_server, "new request applied while server stopped")
            js("document.querySelector('[data-review-operation=\"'+arguments[0]+'\"]').click()", second[0])
            wait(lambda: js("return document.querySelector('[aria-label=\"이전 생성 요청 대조\"]')?.textContent.includes('Discard Folder')"), "existing same-name folder")
            wait(lambda: js("return !!document.querySelector('[data-testid=review-dismiss]:not(:disabled)')"), "dismiss control")
            js("document.querySelector('[data-testid=review-dismiss]').click()")
            wait(lambda: js("return !!document.querySelector('[data-testid=review-dismiss-confirm]:not(:disabled)')"), "explicit dismissal confirmation")
            js("document.querySelector('[data-testid=review-dismiss-confirm]').click()")
            wait(lambda: any(entry == (second[0], "dismissed", None) for entry in decisions()), "dismiss decision")
            old = [row for row in rows() if row[13] == "frozen"]
            require(len(old) == 2 and same_original(old[0], first) and same_original(old[1], second), "review changed old requests")
            require((notes / "Discard Folder").is_dir() and server_snapshot() == before_server, "dismiss changed existing server folder")
            server = subprocess.Popen([server_binary, "serve", "--config", str(config)], stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True, start_new_session=True)
            ready, _, _ = select.select([server.stdout], [], [], 30)
            require(bool(ready) and server.stdout.readline().strip() == "server_started" and helpers.leader_running(server), "synthetic server did not restart")
            proxy.failure.mark("after_restart")
            def committed():
                active = [row for row in rows() if row[13] == "active"]
                return active[0] if len(active) == 1 and active[0][0] == new[0] and active[0][3] == "committed" and active[0][6] and active[0][11] >= 1 and active[0][12] >= 1 else None
            complete = wait(committed, "new create receipt and confirmed ACK", timeout=90)
            require(complete[1:3] == new[1:3] and complete[7:11] == new[7:11] and complete[15] == new[15], "new operation changed identity, payload or binding")
            receipt = json.loads(complete[6]); require(receipt.get("status") == "created" and receipt.get("operation_id") == new[0] and receipt.get("operation_seq") == "1" and receipt.get("workspace_epoch") == new[10] and receipt.get("node", {}).get("node_id") == new[2], "invalid completed receipt")
            require((notes / "Applied Folder").is_dir() and not list((notes / "Applied Folder").iterdir()), "server folder incorrect")
            require((notes / "Discard Folder").is_dir() and not (notes / "Uncertain Folder").exists(), "existing folder removed or old folder unexpectedly created")
            with sqlite3.connect(f"file:{state / 'state.db'}?mode=ro", uri=True, timeout=5) as db:
                require(db.execute("SELECT COUNT(*) FROM nodes WHERE node_id=? AND deleted=0", (new[2],)).fetchone()[0] == 1, "new node missing")
                require(db.execute("SELECT COUNT(*) FROM nodes WHERE node_id=? AND deleted=0", (seed_node,)).fetchone()[0] == 1, "existing folder removed")
                require(db.execute("SELECT COUNT(*) FROM nodes WHERE node_id IN (?,?)", (first[2], second[2])).fetchone()[0] == 0, "old node unexpectedly applied")
                require(db.execute("SELECT COUNT(*) FROM change_sets WHERE operation_id=?", (new[0],)).fetchone()[0] == 1, "new operation count incorrect")
            with proxy.lock:
                requests = [request for request in proxy.restore_requests if request[0] == new[0]]
                require(len(requests) >= 1 and all(request[1:3] == ("1", new[9]) and request[3] == hashlib.sha256(new[7]).digest() for request in requests), "new wire identity or payload incorrect")
                require(all(request[0] in (first[0], new[0]) for request in proxy.restore_requests), "unexpected create request")
                require("1" in proxy.acknowledgements, "new ACK was not observed")
            require(proxy.failure.count("after_restart") == 0, "proxy failed after controlled outage")
            old = [row for row in rows() if row[13] == "frozen"]
            require(len(old) == 2 and same_original(old[0], first) and same_original(old[1], second), "completion changed frozen payloads")
        finally:
            if session_id and wd is not None:
                try: wd("DELETE", f"/session/{session_id}")
                except Exception: pass
            try: helpers.stop(driver)
            finally:
                try:
                    if proxy: proxy.close()
                finally: helpers.stop(server)
    print("M3 frozen create WebView passed: 403, re-registration, apply, dismiss and ACK.", flush=True)


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        safe_messages = {
            'WebDriver request failed',
            'WebDriver returned an error',
            'completion changed frozen payloads',
            'denied or queued request changed server',
            'denied request identity disagreed',
            'denied request payload or identity mismatch',
            'dismiss changed existing server folder',
            'existing folder removed',
            'existing folder removed or old folder unexpectedly created',
            'invalid completed receipt',
            'invalid synthetic admin response',
            'isolated Xvfb required',
            'isolated no-service session bus required',
            'new ACK was not observed',
            'new identities invalid',
            'new node missing',
            'new operation changed identity, payload or binding',
            'new operation count incorrect',
            'new request applied while server stopped',
            'new request incorrect',
            'new stream or sequence incorrect',
            'new wire identity or payload incorrect',
            'old FIFO or binding mismatch',
            'old node unexpectedly applied',
            'old payload hash mismatch',
            'pairing secret remained in form',
            'previous stream was resent',
            'proxy failed after controlled outage',
            'proxy failed before controlled outage',
            're-registration applied or decided a request',
            're-registration changed original requests',
            'review applied a request without user action',
            'review changed old requests',
            'sent request shown as confirmed',
            'server folder incorrect',
            'synthetic existing folder missing',
            'synthetic server command failed',
            'synthetic server did not restart',
            'synthetic server did not start',
            'synthetic setup request failed',
            'timed out waiting for WebDriver startup',
            'timed out waiting for WebDriver status',
            'timed out waiting for actual reader create 403',
            'timed out waiting for both frozen requests',
            'timed out waiting for cached root',
            'timed out waiting for create control',
            'timed out waiting for create submit',
            'timed out waiting for dismiss control',
            'timed out waiting for dismiss decision',
            'timed out waiting for existing same-name folder',
            'timed out waiting for explicit dismissal confirmation',
            'timed out waiting for frozen old registration',
            'timed out waiting for isolated server stopped',
            'timed out waiting for new create receipt and confirmed ACK',
            'timed out waiting for new queued operation',
            'timed out waiting for owner bootstrap',
            'timed out waiting for owner local index',
            'timed out waiting for pairing form',
            'timed out waiting for reader initial sync',
            'timed out waiting for reader permission notice',
            'timed out waiting for review apply',
            'timed out waiting for review control',
            'timed out waiting for review destination',
            'timed out waiting for review panel',
            'timed out waiting for root destination',
            'timed out waiting for second queued create',
            'timed out waiting for sent unconfirmed create',
            'unexpected create request',
        }
        detail = str(error)
        if type(error).__name__ == "TestFailure" and detail in safe_messages:
            print(f"M3 frozen create WebView failed: {detail}", file=sys.stderr)
        else:
            print(f"M3 frozen create WebView failed: {type(error).__name__}", file=sys.stderr)
        raise SystemExit(1) from None
