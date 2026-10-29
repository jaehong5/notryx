#!/usr/bin/env python3
"""Draft: confirmed create conflict after isolated WebView re-registration."""
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
            owner_grant = admin("issue-pairing", status["owner_principal_id"])
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

            pair(owner_grant)
            wait(sync_ready, "first owner initial sync")
            parent = wait(root_parent, "cached root")
            before_server = server_snapshot()
            require((notes / "Discard Folder").is_dir(), "original folder missing")
            create("Discard Folder")
            wait(lambda: proxy.count("restore", 409) > 0, "actual server conflict")
            wait(lambda: proxy.count("ack", 200) > 0, "actual receipt acknowledgement")
            def failed():
                found = rows()
                return found[0] if len(found) == 1 and found[0][3] == "conflict" and found[0][5] == "destination_exists" and found[0][6] is not None and found[0][11] >= 1 and found[0][12] >= 1 else None
            original = wait(failed, "durable conflict receipt and ACK")
            receipt = json.loads(original[6])
            require(receipt.get("status") == "destination_exists" and receipt.get("operation_id") == original[0] and receipt.get("operation_seq") == "1" and receipt.get("workspace_epoch") == original[10], "invalid failure receipt")
            require(original[1] == 1 and original[4] == "none" and original[15] == "sha256:" + hashlib.sha256(original[7]).hexdigest(), "failed request identity or payload mismatch")
            with proxy.lock:
                require(proxy.restore_operation == original[0], "proxy operation disagreed")
                require(len(proxy.restore_requests) >= 1 and all(item[0:3] == (original[0], "1", original[9]) and item[3] == hashlib.sha256(original[7]).digest() for item in proxy.restore_requests), "proxy request changed")
                require("1" in proxy.acknowledgements, "proxy ACK missing")
                observed_requests = tuple(proxy.restore_requests)
            after_failure = server_snapshot()
            require(after_failure[:3] == before_server[:3], "conflict changed server tree or journal")
            with sqlite3.connect(f"file:{state / 'state.db'}?mode=ro", uri=True, timeout=5) as db:
                retired = db.execute("SELECT stream_id,operation_seq FROM retired_operations WHERE operation_id=?", (original[0],)).fetchone()
                stream = db.execute("SELECT last_seq,retired_through FROM operation_streams WHERE stream_id=?", (original[9],)).fetchone()
                require(retired == (original[9], "1") and stream and int(stream[0]) >= 1 and int(stream[1]) >= 1, "server did not retire the failure receipt")
                require(db.execute("SELECT COUNT(*) FROM nodes WHERE node_id=? AND deleted=0", (seed_node,)).fetchone()[0] == 1, "original server node missing")
            grant = admin("issue-pairing", status["owner_principal_id"])
            pair(grant)
            wait(lambda: len(rows()) == 1 and rows()[0][13] == "frozen", "frozen failed registration")
            wait(sync_ready, "replacement owner bootstrap")
            wait(lambda: root_parent() == parent, "replacement local index")
            frozen = rows()[0]
            require(same_original(frozen, original) and frozen[11:13] == original[11:13], "re-registration changed failure receipt or request")
            require(server_snapshot()[:3] == after_failure[:3] and decisions() == [], "re-registration changed server or decided request")
            with proxy.lock:
                require(tuple(proxy.restore_requests) == observed_requests, "failed old request resent")
            open_review()
            wait(lambda: js("return !!document.querySelector('[data-review-operation=\"'+arguments[0]+'\"]')", original[0]), "frozen failure entry")
            label = js("return document.querySelector('[data-review-operation=\"'+arguments[0]+'\"]').textContent", original[0])
            require("서버 처리 실패" in label and "결과 미확정" not in label, "failure shown as uncertain")
            js("document.querySelector('[data-review-operation=\"'+arguments[0]+'\"]').click()", original[0])
            wait(lambda: js("const p=document.querySelector('[aria-label=\"이전 생성 요청 대조\"]'); return !!p && p.textContent.includes('서버 처리 실패: 목적지에 같은 이름이 있습니다.')"), "conflict reason")
            require(js("return document.querySelector('[aria-label=\"이전 생성 요청 대조\"]')?.textContent.includes('현재 같은 이름: Discard Folder')"), "same-name folder not displayed")
            require(not js("return document.querySelector('[aria-label=\"이전 생성 요청 대조\"]')?.textContent.includes('같은 이름이나 ID가 보여도 이전 요청의 서버 결과는 확정하지 않습니다.')"), "confirmed failure described as uncertain")
            require(decisions() == [] and server_snapshot()[:3] == after_failure[:3], "viewing review changed result")
            wait(lambda: js("return !!document.querySelector('[data-testid=review-dismiss]:not(:disabled)')"), "dismiss control")
            js("document.querySelector('[data-testid=review-dismiss]').click()")
            wait(lambda: js("return !!document.querySelector('[data-testid=review-dismiss-confirm]:not(:disabled)')"), "explicit dismissal confirmation")
            js("document.querySelector('[data-testid=review-dismiss-confirm]').click()")
            wait(lambda: decisions() == [(original[0], "dismissed", None)], "dismissed decision")
            final = rows()
            require(len(final) == 1 and same_original(final[0], original) and final[0][11:13] == original[11:13], "dismiss changed old payload or receipt")
            require((notes / "Discard Folder").is_dir(), "dismiss removed original folder")
            require(server_snapshot()[:3] == after_failure[:3], "dismiss changed server tree or journal")
            with sqlite3.connect(f"file:{state / 'state.db'}?mode=ro", uri=True, timeout=5) as db:
                require(db.execute("SELECT COUNT(*) FROM nodes WHERE node_id=? AND deleted=0", (seed_node,)).fetchone()[0] == 1, "dismiss removed original server node")
                require(db.execute("SELECT COUNT(*) FROM nodes WHERE node_id=?", (original[2],)).fetchone()[0] == 0, "failed node unexpectedly created")
            with sqlite3.connect(f"file:{database}?mode=ro", uri=True, timeout=5) as db:
                active_creates = db.execute("SELECT COUNT(*) FROM client_outbox o JOIN client_registrations r ON r.id=o.registration_id WHERE r.status='active' AND o.operation_kind='create'").fetchone()[0]
                require(active_creates == 0, "dismiss enqueued a new create")
            require(proxy.failure.count("before_outage") == 0, "isolated proxy failed")
        finally:
            if session_id and wd is not None:
                try: wd("DELETE", f"/session/{session_id}")
                except Exception: pass
            try: helpers.stop(driver)
            finally:
                try:
                    if proxy: proxy.close()
                finally: helpers.stop(server)
    print("M3 frozen failure WebView passed: conflict receipt, ACK, re-registration and dismissal.", flush=True)


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        safe_messages = {
            'WebDriver request failed',
            'WebDriver returned an error',
            'confirmed failure described as uncertain',
            'conflict changed server tree or journal',
            'dismiss changed old payload or receipt',
            'dismiss changed server tree or journal',
            'dismiss enqueued a new create',
            'dismiss removed original folder',
            'dismiss removed original server node',
            'failed node unexpectedly created',
            'failed old request resent',
            'failed request identity or payload mismatch',
            'failure shown as uncertain',
            'invalid failure receipt',
            'invalid synthetic admin response',
            'isolated Xvfb required',
            'isolated no-service session bus required',
            'isolated proxy failed',
            'original folder missing',
            'original server node missing',
            'pairing secret remained in form',
            'proxy ACK missing',
            'proxy operation disagreed',
            'proxy request changed',
            're-registration changed failure receipt or request',
            're-registration changed server or decided request',
            'same-name folder not displayed',
            'server did not retire the failure receipt',
            'synthetic existing folder missing',
            'synthetic server command failed',
            'synthetic server did not start',
            'synthetic setup request failed',
            'timed out waiting for WebDriver startup',
            'timed out waiting for WebDriver status',
            'timed out waiting for actual receipt acknowledgement',
            'timed out waiting for actual server conflict',
            'timed out waiting for cached root',
            'timed out waiting for conflict reason',
            'timed out waiting for create control',
            'timed out waiting for create submit',
            'timed out waiting for dismiss control',
            'timed out waiting for dismissed decision',
            'timed out waiting for durable conflict receipt and ACK',
            'timed out waiting for explicit dismissal confirmation',
            'timed out waiting for first owner initial sync',
            'timed out waiting for frozen failed registration',
            'timed out waiting for frozen failure entry',
            'timed out waiting for pairing form',
            'timed out waiting for replacement local index',
            'timed out waiting for replacement owner bootstrap',
            'timed out waiting for review control',
            'timed out waiting for review panel',
            'timed out waiting for root destination',
            'viewing review changed result',
        }
        detail = str(error)
        if type(error).__name__ == "TestFailure" and detail in safe_messages:
            print(f"M3 frozen failure WebView failed: {detail}", file=sys.stderr)
        else:
            print(f"M3 frozen failure WebView failed: {type(error).__name__}", file=sys.stderr)
        raise SystemExit(1) from None
