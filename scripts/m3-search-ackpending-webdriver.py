#!/usr/bin/env python3
"""Exercise search invalidation after a committed receipt with an unconfirmed ACK."""

import argparse
import base64
import hashlib
import http.client
import importlib.util
import json
import os
from pathlib import Path
import select
import socket
import sqlite3
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request

from m3_open_proxy import OpenProxy
from m3_resync_proxy import Handler as ResyncHandler, ResyncProxy


class AckHoldHandler(ResyncHandler):
    def _handle_request(self):
        self.connection.settimeout(15)
        deadline = time.monotonic() + 10
        while True:
            prefix = self.connection.recv(8192, socket.MSG_PEEK)
            if b"\r\n" in prefix:
                break
            if not prefix or len(prefix) == 8192 or time.monotonic() >= deadline:
                raise ValueError("invalid request line")
            time.sleep(0.01)
        if not prefix.startswith(b"POST /v1/operation-stream/ack "):
            return super()._handle_request()
        request_line = self.rfile.readline(8193)
        headers = []
        total = len(request_line)
        length = 0
        while True:
            line = self.rfile.readline(8193)
            total += len(line)
            if not line or total > 65536:
                raise ValueError("invalid ACK headers")
            if line == b"\r\n":
                break
            name, value = line.split(b":", 1)
            name = name.strip().lower()
            if name == b"content-length":
                length = int(value.strip())
            elif name == b"transfer-encoding":
                raise ValueError("unsupported ACK transfer encoding")
            if name != b"connection":
                headers.append(line)
        if not 0 <= length <= 65536:
            raise ValueError("ACK request too large")
        body = self.rfile.read(length)
        if len(body) != length:
            raise ValueError("incomplete ACK request")
        payload = json.loads(body)
        if not isinstance(payload, dict):
            raise ValueError("invalid ACK request")
        with socket.create_connection(("127.0.0.1", self.server.target_port), timeout=15) as upstream:
            upstream.settimeout(15)
            upstream.sendall(request_line + b"".join(headers) + b"Connection: close\r\n\r\n" + body)
            response = bytearray()
            while True:
                chunk = upstream.recv(65536)
                if not chunk:
                    break
                response.extend(chunk)
                if len(response) > 1024 * 1024:
                    raise ValueError("ACK response too large")
        if not response.startswith(b"HTTP/1.1 200 "):
            raise ValueError("unexpected ACK response")
        with self.server.ack_control:
            held = self.server.ack_response_held
            release = self.server.release_ack
            delivered = self.server.ack_response_delivered
            self.server.ack_payload = payload
        held.set()
        if not release.wait(20):
            raise TimeoutError("ACK response release timed out")
        self.wfile.write(response)
        self.wfile.flush()
        delivered.set()


class ReceiptProxy(ResyncProxy):
    def __init__(self, target_port, document_id):
        super().__init__(target_port, document_id)
        self.RequestHandlerClass = AckHoldHandler
        self.ack_control = threading.Lock()
        self.ack_response_held = threading.Event()
        self.release_ack = threading.Event()
        self.ack_response_delivered = threading.Event()
        self.ack_payload = None

    def arm_next_ack(self):
        with self.ack_control:
            self.ack_response_held = threading.Event()
            self.release_ack = threading.Event()
            self.ack_response_delivered = threading.Event()
            self.ack_payload = None

    def close(self):
        self.release_ack.set()
        super().close()


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


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--driver", default="WebKitWebDriver")
    parser.add_argument("--binary", default="target/debug/notryx-client")
    parser.add_argument("--server", default="target/debug/notryx-server")
    parser.add_argument("--verify-confirmed-ack", action="store_true")
    args = parser.parse_args()
    require(helpers.xvfb_on_current_display(), "search receipt test requires isolated Xvfb")
    client_binary = str(Path(args.binary).resolve())
    server_binary = str(Path(args.server).resolve())
    server = driver = outer = inner = None
    session_id = None
    wd = None

    with tempfile.TemporaryDirectory(prefix="notryx-m3-search-ackpending-") as temporary:
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

        def local_operation(node_id):
            path = client_data / "drafts.sqlite3"
            if not path.exists():
                return None
            with sqlite3.connect(f"file:{path}?mode=ro", uri=True, timeout=5) as db:
                return db.execute("""
                    SELECT o.operation_id, o.operation_seq, o.status, o.committed_revision,
                           o.bytes, s.ack_through, s.confirmed_through,
                           c.bytes, b.bytes, b.revision, r.device_stream_id, o.generation, c.generation
                    FROM client_outbox o
                    JOIN client_registrations r ON r.id=o.registration_id
                    JOIN outbox_streams s ON s.registration_id=r.id
                    JOIN registered_documents d ON d.registration_id=r.id AND d.document_id=o.document_id
                    JOIN draft_checkpoints c ON c.document_id=d.storage_id
                    JOIN draft_bases b ON b.document_id=d.storage_id
                    WHERE r.status='active' AND o.document_id=? AND o.operation_kind='content'
                    ORDER BY o.operation_seq DESC LIMIT 1
                """, (node_id,)).fetchone()

        def server_stream(stream_id):
            with sqlite3.connect(f"file:{state / 'state.db'}?mode=ro", uri=True, timeout=5) as db:
                return db.execute("SELECT last_seq, retired_through FROM operation_streams WHERE stream_id=?", (stream_id,)).fetchone()

        def current_held_ack():
            with inner.ack_control:
                return dict(inner.ack_payload) if inner.ack_payload is not None else None

        def local_outbox_snapshot():
            with sqlite3.connect(f"file:{client_data / 'drafts.sqlite3'}?mode=ro", uri=True, timeout=5) as db:
                outbox = db.execute("SELECT * FROM client_outbox ORDER BY operation_id").fetchall()
                streams = db.execute("SELECT * FROM outbox_streams ORDER BY registration_id").fetchall()
                return outbox, streams

        def sync_cursor():
            with sqlite3.connect(f"file:{client_data / 'drafts.sqlite3'}?mode=ro", uri=True, timeout=5) as db:
                row = db.execute("""
                    SELECT i.cursor FROM remote_index i JOIN client_registrations r ON r.id=i.registration_id
                    WHERE r.status='active'
                """).fetchone()
                return row[0] if row else None

        def receipt_timestamps():
            with sqlite3.connect(f"file:{client_data / 'drafts.sqlite3'}?mode=ro", uri=True, timeout=5) as db:
                return [row[0] for row in db.execute("SELECT applied_ms FROM synthetic_receipt_timing")]

        def confirmation_timestamps():
            with sqlite3.connect(f"file:{client_data / 'drafts.sqlite3'}?mode=ro", uri=True, timeout=5) as db:
                return [row[0] for row in db.execute("SELECT confirmed_ms FROM synthetic_confirmation_timing")]

        def change_requests():
            with outer.lock:
                return outer.route_counts.get("changes", 0)

        def no_change_in_flight():
            with outer.lock:
                return outer.inflight.get("changes", 0) == 0

        try:
            cli("init", "--config", str(config))
            server = subprocess.Popen(
                [server_binary, "serve", "--config", str(config)], stdout=subprocess.PIPE,
                stderr=subprocess.DEVNULL, text=True, start_new_session=True,
            )
            ready, _, _ = select.select([server.stdout], [], [], 30)
            require(bool(ready) and server.stdout.readline().strip() == "server_started" and helpers.leader_running(server), "synthetic server did not start")
            status = admin("status")
            setup_grant = admin("issue-pairing", status["owner_principal_id"])
            owner = server_request("POST", "/v1/pair", body=json.dumps({
                "protocol": "mdw-sync/1.2", "workspace_id": setup_grant["workspace_id"],
                "pairing_secret": setup_grant["pairing_secret"],
            }).encode(), headers={"Content-Type": "application/json"})
            authorization = {"Authorization": f"Bearer {owner['device_token']}"}
            info = server_request("GET", "/v1/server-info", headers=authorization)
            node_id = helpers.operation_id()
            original = b"# Synthetic search receipt\n"
            marker = "\n합성검색초안\n"
            edited = original + marker.encode()
            digest = base64.b64encode(hashlib.sha256(original).digest()).decode()
            upload = server_request("POST", "/v1/uploads", body=original, expected=201, headers={
                **authorization, "Content-Type": "application/octet-stream", "Content-Digest": f"sha-256=:{digest}:",
            })
            server_request("POST", "/v1/nodes", body=json.dumps({
                "node_id": node_id, "parent_id": info["root_id"], "name": "Synthetic.md",
                "kind": "file", "upload_id": upload["upload_id"],
            }).encode(), headers={
                **authorization, "Content-Type": "application/json", "X-Workspace-Epoch": info["workspace_epoch"],
                "X-Device-Stream": owner["device_stream_id"], "X-Operation-Seq": "1",
                "Idempotency-Key": helpers.operation_id(),
            })
            before_node = server_request("GET", f"/v1/nodes/{node_id}", headers=authorization)
            ui_grant = admin("issue-pairing", status["owner_principal_id"])
            inner = ReceiptProxy(port, node_id)
            inner.start()
            outer = OpenProxy(inner.server_address[1], node_id)
            outer.start()

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

            wait(lambda: js("const button=document.querySelector('.workspace-connect button'); return !!button && !button.disabled"), "pairing form")
            js("""
                const form=document.querySelector('.workspace-connect');
                form.elements.namedItem('origin').value=arguments[0];
                form.elements.namedItem('workspace_id').value=arguments[1];
                form.elements.namedItem('pairing_secret').value=arguments[2];
                form.requestSubmit();
            """, f"http://127.0.0.1:{outer.server_address[1]}", ui_grant["workspace_id"], ui_grant["pairing_secret"])
            require(js("return document.querySelector('[name=pairing_secret]').value === ''"), "pairing secret remained in the form")

            def sync_ready():
                return js("""
                    const done=arguments[arguments.length-1];
                    window.__TAURI_INTERNALS__.invoke('m0_renderer_session')
                      .then(owner=>window.__TAURI_INTERNALS__.invoke('workspace_sync_status',{request:{renderer_session_id:owner}}))
                      .then(value=>done(value.status==='ready')).catch(()=>done(false));
                """, asynchronous=True)

            wait(sync_ready, "initial synchronization")
            registered = js("""
                const done=arguments[arguments.length-1];
                window.__receiptTrace={phase:'setup',metadata:[],views:[],overflow:false};
                const handler=window.__TAURI_INTERNALS__.transformCallback(()=>{
                  const trace=window.__receiptTrace;
                  if(trace.metadata.length<64) trace.metadata.push({phase:trace.phase,at:Date.now()});
                  else trace.overflow=true;
                });
                window.__TAURI_INTERNALS__.invoke('plugin:event|listen',{
                  event:'workspace-metadata-changed',target:{kind:'Any'},handler
                }).then(()=>done(true)).catch(()=>done(false));
            """, asynchronous=True)
            require(registered, "metadata event observation could not start")
            wait(lambda: js("return !!document.querySelector('[data-document=\"'+arguments[0]+'\"]:not(:disabled)')", node_id), "synthetic document in workspace")
            js("document.querySelector('[data-document=\"'+arguments[0]+'\"]').click()", node_id)
            wait(lambda: js("return document.querySelector('[aria-current=page]')?.dataset.document===arguments[0] && document.querySelector('.cm-content')?.cmTile.root.view.state.doc.toString()===arguments[1]", node_id, original.decode()), "synthetic document open")
            wait(no_change_in_flight, "initial metadata request completion")
            outer.hold_events.set()
            wait(no_change_in_flight, "metadata isolation")
            changes_before = change_requests()
            cursor_before = sync_cursor()
            require(cursor_before is not None, "initial local sync cursor is missing")
            inner.hold_put.set()
            js("""
                const view=document.querySelector('.cm-content').cmTile.root.view;
                view.dispatch({changes:{from:view.state.doc.length,insert:arguments[0]},userEvent:'input.type'});
            """, marker)
            require(inner.put_response_held.wait(20), "committed PUT response was not held")
            held = wait(lambda: (row if (row := local_operation(node_id)) and row[2] == "sent" and row[7] == edited else None), "checkpoint and queued send")
            require(held[8] == original and held[5:7] == (0, 0), "held receipt unexpectedly changed local base or acknowledgement")
            require(held[11] == held[12] and held[11] > 0, "queued operation and checkpoint generations differ")
            require((notes / "Synthetic.md").read_bytes() == edited, "held PUT did not commit the synthetic server content")
            wait(lambda: js("return document.querySelector('#save-status')?.textContent.trim()==='이 기기에 저장됨'"), "UI checkpoint completion")
            with sqlite3.connect(client_data / "drafts.sqlite3", timeout=5) as db:
                db.execute("CREATE TRIGGER synthetic_fail_ack_confirmation BEFORE UPDATE OF confirmed_through ON outbox_streams BEGIN SELECT RAISE(ABORT, 'synthetic ack failure'); END")
                db.execute("CREATE TABLE synthetic_receipt_timing (applied_ms INTEGER NOT NULL)")
                db.execute("""
                    CREATE TRIGGER synthetic_record_receipt AFTER UPDATE OF status ON client_outbox
                    WHEN OLD.status='sent' AND NEW.status='committed' BEGIN
                      INSERT INTO synthetic_receipt_timing VALUES (CAST(ROUND((julianday('now')-2440587.5)*86400000) AS INTEGER));
                    END
                """)
                db.commit()
            wait(no_change_in_flight, "metadata request before search")
            require(change_requests() == changes_before and sync_cursor() == cursor_before, "metadata catch-up interfered before draft search")
            def find_draft():
                js("window.dispatchEvent(new KeyboardEvent('keydown',{key:'F',ctrlKey:true,shiftKey:true,bubbles:true}))")
                wait(lambda: js("return !!document.querySelector('[aria-label=\"본문 검색\"]')"), "search shortcut")
                js("""
                    const panel=document.querySelector('[aria-label="본문 검색"]');
                    const input=panel.querySelector('input');
                    Object.getOwnPropertyDescriptor(HTMLInputElement.prototype,'value').set.call(input,arguments[0]);
                    input.dispatchEvent(new Event('input',{bubbles:true}));
                    const select=panel.querySelector('select');
                    Object.getOwnPropertyDescriptor(HTMLSelectElement.prototype,'value').set.call(select,'workspace');
                    select.dispatchEvent(new Event('change',{bubbles:true}));
                """, "합성검색초안")
                wait(lambda: js("const b=document.querySelector('[aria-label=\"본문 검색\"] button[type=submit]'); return !!b && !b.disabled"), "search form")
                js("document.querySelector('[aria-label=\"본문 검색\"] form').requestSubmit()")

                def search_page():
                    return js("""
                        const panel=document.querySelector('[aria-label="본문 검색"]');
                        if(!panel) return null;
                        const statuses=[...panel.querySelectorAll('[role=status]')];
                        const coverage=[...panel.querySelectorAll('p')].find(e=>e.textContent.startsWith('이번 구간에서 검색 가능한 문서:'));
                        const next=[...panel.querySelectorAll('button')].find(e=>e.textContent.trim()==='다음 결과');
                        return {busy:statuses.some(e=>e.textContent==='검색 중'),
                          failed:statuses.some(e=>e.textContent.includes('검색을 완료하지 못했습니다')),
                          coverage:coverage?.textContent ?? null,
                          results:[...panel.querySelectorAll('[data-search-node]')].map(e=>e.dataset.searchNode),
                          next:!!next && !next.disabled};
                    """)

                previous = None
                for _ in range(10):
                    def changed_page():
                        page = search_page()
                        if page and page["failed"]:
                            raise TestFailure("search UI reported an error")
                        return page if page and not page["busy"] and page["coverage"] is not None and page != previous else None
                    page = wait(changed_page, "draft search page", seconds=10)
                    if node_id in page["results"]:
                        break
                    require(page["next"], "local draft search result was not found")
                    previous = page
                    js("[...document.querySelectorAll('[aria-label=\"본문 검색\"] button')].find(b=>b.textContent.trim()==='다음 결과').click()")
                else:
                    raise TestFailure("draft search exceeded page limit")
                require(js("return document.querySelector('[data-search-node=\"'+arguments[0]+'\"]')?.textContent.includes('기기 초안')", node_id), "search did not use the dirty draft")

            find_draft()
            js("""
                const trace=window.__receiptTrace;
                const initial=document.querySelector('[aria-label="본문 검색"]');
                trace.phase='search_shown'; trace.initialPanel=initial;
                const observe=()=>{
                  const panel=document.querySelector('[aria-label="본문 검색"]');
                  const state={at:Date.now(),phase:trace.phase,panelPresent:!!panel,
                    samePanel:panel===initial,resultPresent:!!panel?.querySelector('[data-search-node]'),
                    expanded:[...document.querySelectorAll('button')].some(b=>b.textContent.trim()==='본문 검색' && b.getAttribute('aria-expanded')==='true')};
                  const last=trace.views[trace.views.length-1];
                  if(!last || last.panelPresent!==state.panelPresent || last.samePanel!==state.samePanel || last.resultPresent!==state.resultPresent || last.expanded!==state.expanded) {
                    if(trace.views.length<64) trace.views.push(state);
                    else trace.overflow=true;
                  }
                };
                observe();
                trace.observer=new MutationObserver(observe);
                trace.observer.observe(document.body,{childList:true,subtree:true,attributes:true,attributeFilter:['aria-expanded','data-search-node']});
            """)
            stable = js("""
                const done=arguments[arguments.length-1], trace=window.__receiptTrace;
                const startMetadata=trace.metadata.length, startViews=trace.views.length;
                setTimeout(()=>{
                  const panel=document.querySelector('[aria-label="본문 검색"]');
                  done(!trace.overflow && trace.metadata.length===startMetadata && trace.views.length===startViews && panel===trace.initialPanel && !!panel?.querySelector('[data-search-node]'));
                },800);
            """, asynchronous=True)
            require(stable, "search changed before receipt response was released")
            require(js("return document.querySelector('#save-status')?.textContent.trim()==='이 기기에 저장됨'"), "UI checkpoint was not settled before search")
            require(change_requests() == changes_before and sync_cursor() == cursor_before, "metadata catch-up interfered before receipt release")
            js("window.__receiptTrace.phase='receipt_released'")
            release_ms = time.time_ns() // 1_000_000
            inner.release_put.set()
            require(inner.put_response_delivered.wait(10), "committed PUT response was not delivered")
            require(inner.ack_response_held.wait(10), "server ACK response was not held")
            ack_held_observed_ms = time.time_ns() // 1_000_000

            def locally_applied():
                row = local_operation(node_id)
                if not row or row[0] != held[0] or row[2] != "committed" or row[3] is None:
                    return None
                if row[5] != row[1] or row[6] != 0 or row[7] != edited or row[8] != edited or row[9] != row[3]:
                    return None
                server_ack = server_stream(row[10])
                if not server_ack or int(server_ack[1]) < row[1]:
                    return None
                return row

            applied = wait(locally_applied, "local receipt application and server ACK", seconds=15)
            applied_observed_ms = time.time_ns() // 1_000_000
            require(applied[4] == edited and applied[3] != before_node["content_rev"], "committed receipt did not advance the draft base")
            require(applied[11:13] == held[11:13], "checkpoint generation changed during receipt processing")
            applied_times = receipt_timestamps()
            require(len(applied_times) == 1 and applied_times[0] >= release_ms - 5, "local receipt timing was not recorded")
            try:
                wait(lambda: js("const p=document.querySelector('[aria-label=\"본문 검색\"]'); return !!p && !p.querySelector('[data-search-node=\"'+arguments[0]+'\"]') && [...document.querySelectorAll('button')].some(b=>b.textContent.trim()==='본문 검색' && b.getAttribute('aria-expanded')==='true')", node_id), "search invalidation while ACK response is held", seconds=4)
                invalidated = True
            except TestFailure:
                invalidated = False
            require(change_requests() == changes_before and sync_cursor() == cursor_before, "metadata catch-up interfered with receipt observation")
            inner.release_ack.set()
            wait(lambda: js("return document.querySelector('#sync-status')?.textContent.includes('영수증 확인 중')"), "unconfirmed receipt UI", seconds=10)

            def pending_ack():
                status = js("""
                    const done=arguments[arguments.length-1];
                    window.__TAURI_INTERNALS__.invoke('m0_renderer_session')
                      .then(owner=>window.__TAURI_INTERNALS__.invoke('workspace_send_next',{request:{renderer_session_id:owner}}))
                      .then(value=>done(value.status==='ack_pending')).catch(()=>done(false));
                """, asynchronous=True)
                return bool(status)

            wait(pending_ack, "AckPending result after injected confirmation failure", seconds=12)
            after_probe = local_operation(node_id)
            require(after_probe is not None and after_probe[0:10] == applied[0:10], "ACK retry changed the local committed receipt")
            require(change_requests() == changes_before and sync_cursor() == cursor_before, "metadata catch-up interfered with the ACK probe")
            require(not inner.failure.is_set() and not outer.failure.is_set(), "isolated proxy reported a failure")
            trace = js("""
                const trace=window.__receiptTrace;
                return {metadata:trace.metadata,views:trace.views,overflow:trace.overflow};
            """)
            changes = [event for event in trace["views"] if not event["resultPresent"]]
            panel_closed = any(not event["panelPresent"] or not event["expanded"] for event in trace["views"])
            first_change_ms = changes[0]["at"] if changes else None
            diagnostic = {
                "saved_before_search": True,
                "stable_before_release": stable,
                "receipt_ms_from_release": applied_times[0] - release_ms,
                "receipt_commit_observed_ms_from_release": applied_observed_ms - release_ms,
                "ack_held_observed_ms_from_release": ack_held_observed_ms - release_ms,
                "first_result_removal_ms_from_release": None if first_change_ms is None else first_change_ms - release_ms,
                "metadata_before_release": sum(event["phase"] == "search_shown" for event in trace["metadata"]),
                "metadata_after_release": sum(event["phase"] == "receipt_released" for event in trace["metadata"]),
                "metadata_ms_from_release": [event["at"] - release_ms for event in trace["metadata"] if event["phase"] == "receipt_released"],
                "metadata_before_result_removal": sum(event["phase"] == "receipt_released" and first_change_ms is not None and event["at"] <= first_change_ms for event in trace["metadata"]),
                "panel_closed": panel_closed,
                "panel_replaced": any(not event["samePanel"] for event in trace["views"]),
                "result_removed": bool(changes),
                "trace_overflow": trace["overflow"],
                "removal_before_commit_observed": first_change_ms is not None and first_change_ms < applied_observed_ms,
            }
            print("Search receipt diagnostic:", json.dumps(diagnostic), flush=True)
            require(not trace["overflow"], "search observation trace overflowed")
            require(diagnostic["metadata_before_release"] == 0, "metadata notification arrived between search and receipt release")
            require(not panel_closed, "search panel closed during receipt observation")
            require(first_change_ms is None or first_change_ms >= applied_times[0] - 5, "search changed before the local receipt was applied")
            require(invalidated, "search result remained visible while ACK response was held after local receipt commit")
            require(bool(changes), "search result removal was not observed")
            receipt_notifications = [event for event in trace["metadata"] if event["phase"] == "receipt_released" and applied_times[0] - 5 <= event["at"] <= first_change_ms + 5]
            require(bool(receipt_notifications), "receipt metadata notification did not precede result removal")
            if args.verify_confirmed_ack:
                inner.arm_next_ack()

                def start_ack_retry():
                    return js("""
                        const done=arguments[arguments.length-1];
                        window.__TAURI_INTERNALS__.invoke('m0_renderer_session').then(owner=>{
                          const attempt=(window.__syntheticAckAttempt || 0)+1;
                          window.__syntheticAckAttempt=attempt;
                          window.__syntheticAckRetry='pending';
                          window.__TAURI_INTERNALS__.invoke('workspace_send_next',{request:{renderer_session_id:owner}})
                            .then(value=>{if(window.__syntheticAckAttempt===attempt) window.__syntheticAckRetry=['idle','pending','ack_pending','needs_review','sync_pending'].includes(value.status) ? value.status : 'other';})
                            .catch(()=>{if(window.__syntheticAckAttempt===attempt) window.__syntheticAckRetry='rejected';});
                          done(true);
                        }).catch(()=>done(false));
                    """, asynchronous=True)

                deadline = time.monotonic() + 12
                while not inner.ack_response_held.is_set() and time.monotonic() < deadline:
                    require(start_ack_retry(), "ACK retry could not start")
                    inner.ack_response_held.wait(0.5)
                require(inner.ack_response_held.is_set(), "retry ACK response was not held")
                pending = local_operation(node_id)
                require(pending is not None and pending[0:10] == applied[0:10], "local ACK changed before the held response")
                require(current_held_ack() == {"workspace_epoch": info["workspace_epoch"], "device_stream_id": pending[10], "ack_through": str(pending[5])}, "held retry ACK did not match the active stream and watermark")
                with sqlite3.connect(client_data / "drafts.sqlite3", timeout=5) as db:
                    db.execute("DROP TRIGGER synthetic_fail_ack_confirmation")
                    db.execute("CREATE TABLE synthetic_confirmation_timing (confirmed_ms INTEGER NOT NULL)")
                    db.execute("""
                        CREATE TRIGGER synthetic_record_confirmation AFTER UPDATE OF confirmed_through ON outbox_streams
                        WHEN NEW.confirmed_through>OLD.confirmed_through BEGIN
                          INSERT INTO synthetic_confirmation_timing VALUES (CAST(ROUND((julianday('now')-2440587.5)*86400000) AS INTEGER));
                        END
                    """)
                    db.commit()
                find_draft()
                require(change_requests() == changes_before and sync_cursor() == cursor_before, "metadata catch-up interfered before ACK confirmation")
                js("""
                    const trace=window.__receiptTrace;
                    trace.observer.disconnect();
                    trace.phase='confirmation_search'; trace.views=[];
                    const initial=document.querySelector('[aria-label="본문 검색"]');
                    trace.initialPanel=initial;
                    const observe=()=>{
                      const panel=document.querySelector('[aria-label="본문 검색"]');
                      const state={at:Date.now(),phase:trace.phase,panelPresent:!!panel,
                        samePanel:panel===initial,resultPresent:!!panel?.querySelector('[data-search-node]'),
                        expanded:[...document.querySelectorAll('button')].some(b=>b.textContent.trim()==='본문 검색' && b.getAttribute('aria-expanded')==='true')};
                      const last=trace.views[trace.views.length-1];
                      if(!last || last.panelPresent!==state.panelPresent || last.samePanel!==state.samePanel || last.resultPresent!==state.resultPresent || last.expanded!==state.expanded) {
                        if(trace.views.length<64) trace.views.push(state);
                        else trace.overflow=true;
                      }
                    };
                    observe();
                    trace.observer=new MutationObserver(observe);
                    trace.observer.observe(document.body,{childList:true,subtree:true,attributes:true,attributeFilter:['aria-expanded','data-search-node']});
                """)
                confirmation_stable = js("""
                    const done=arguments[arguments.length-1], trace=window.__receiptTrace;
                    const startMetadata=trace.metadata.length, startViews=trace.views.length;
                    setTimeout(()=>{
                      const panel=document.querySelector('[aria-label="본문 검색"]');
                      done(!trace.overflow && trace.metadata.length===startMetadata && trace.views.length===startViews && panel===trace.initialPanel && !!panel?.querySelector('[data-search-node]'));
                    },800);
                """, asynchronous=True)
                require(confirmation_stable, "search changed before ACK confirmation response")
                require(local_operation(node_id)[6] == 0, "ACK was confirmed before the response was released")
                require(change_requests() == changes_before and sync_cursor() == cursor_before, "metadata catch-up interfered before ACK response release")
                js("window.__receiptTrace.phase='confirmation_released'")
                confirmation_release_ms = time.time_ns() // 1_000_000
                inner.release_ack.set()
                require(inner.ack_response_delivered.wait(10), "retry ACK response was not delivered")

                def locally_confirmed():
                    row = local_operation(node_id)
                    return row if row and row[0] == applied[0] and row[6] == row[1] else None

                confirmed = wait(locally_confirmed, "local ACK confirmation", seconds=12)
                confirmation_observed_ms = time.time_ns() // 1_000_000
                require(confirmed[0:6] == applied[0:6] and confirmed[7:13] == applied[7:13], "ACK confirmation changed committed receipt or draft")
                confirm_times = confirmation_timestamps()
                require(len(confirm_times) == 1 and confirm_times[0] >= confirmation_release_ms - 5, "local ACK confirmation timing was not recorded")
                try:
                    wait(lambda: js("const p=document.querySelector('[aria-label=\"본문 검색\"]'); return !!p && !p.querySelector('[data-search-node=\"'+arguments[0]+'\"]') && [...document.querySelectorAll('button')].some(b=>b.textContent.trim()==='본문 검색' && b.getAttribute('aria-expanded')==='true')", node_id), "search invalidation after ACK confirmation", seconds=4)
                    confirmation_invalidated = True
                except TestFailure:
                    confirmation_invalidated = False
                require(change_requests() == changes_before and sync_cursor() == cursor_before, "metadata catch-up interfered with ACK confirmation observation")
                confirmation_trace = js("""
                    const trace=window.__receiptTrace;
                    return {metadata:trace.metadata,views:trace.views,overflow:trace.overflow};
                """)
                confirmation_changes = [event for event in confirmation_trace["views"] if not event["resultPresent"]]
                first_confirmation_change = confirmation_changes[0]["at"] if confirmation_changes else None
                confirmation_events = [event for event in confirmation_trace["metadata"] if event["phase"] == "confirmation_released"]
                confirmation_diagnostic = {
                    "search_stable_before_confirmation": confirmation_stable,
                    "confirmed_ms_from_release": confirm_times[0] - confirmation_release_ms,
                    "confirmed_observed_ms_from_release": confirmation_observed_ms - confirmation_release_ms,
                    "first_removal_ms_from_release": None if first_confirmation_change is None else first_confirmation_change - confirmation_release_ms,
                    "receipt_notifications": len(receipt_notifications),
                    "confirmation_notifications": len(confirmation_events),
                    "notification_ms_from_release": [event["at"] - confirmation_release_ms for event in confirmation_events],
                    "panel_closed": any(not event["panelPresent"] or not event["expanded"] for event in confirmation_trace["views"]),
                    "trace_overflow": confirmation_trace["overflow"],
                }
                print("Search ACK confirmation diagnostic:", json.dumps(confirmation_diagnostic), flush=True)
                require(not confirmation_trace["overflow"], "ACK confirmation observation trace overflowed")
                require(not confirmation_diagnostic["panel_closed"], "search panel closed during ACK confirmation")
                require(first_confirmation_change is None or first_confirmation_change >= confirm_times[0] - 5, "search changed before ACK confirmation")
                require(confirmation_invalidated, "search result remained visible after ACK confirmation")
                require(bool(confirmation_changes), "search removal after ACK confirmation was not observed")
                require(any(confirm_times[0] - 5 <= event["at"] <= first_confirmation_change + 5 for event in confirmation_events), "ACK confirmation notification did not precede result removal")
                require(not inner.failure.is_set() and not outer.failure.is_set(), "isolated proxy reported a failure during ACK confirmation")

                inner.arm_next_ack()
                require(start_ack_retry(), "repeat ACK could not start")
                require(inner.ack_response_held.wait(10), "repeat ACK response was not held")
                require(js("return window.__syntheticAckRetry==='pending'"), "repeat ACK sender was not isolated")
                require(local_operation(node_id)[6] == confirmed[6], "confirmed watermark changed before repeat ACK")
                require(current_held_ack() == {"workspace_epoch": info["workspace_epoch"], "device_stream_id": confirmed[10], "ack_through": str(confirmed[6])}, "repeat ACK did not match the confirmed stream and watermark")
                find_draft()
                require(change_requests() == changes_before and sync_cursor() == cursor_before, "metadata catch-up interfered before repeat ACK")
                js("""
                    const trace=window.__receiptTrace;
                    trace.observer.disconnect();
                    trace.phase='repeat_search'; trace.views=[];
                    const initial=document.querySelector('[aria-label="본문 검색"]');
                    trace.initialPanel=initial;
                    const observe=()=>{
                      const panel=document.querySelector('[aria-label="본문 검색"]');
                      const state={at:Date.now(),phase:trace.phase,panelPresent:!!panel,
                        samePanel:panel===initial,resultPresent:!!panel?.querySelector('[data-search-node]'),
                        expanded:[...document.querySelectorAll('button')].some(b=>b.textContent.trim()==='본문 검색' && b.getAttribute('aria-expanded')==='true')};
                      const last=trace.views[trace.views.length-1];
                      if(!last || last.panelPresent!==state.panelPresent || last.samePanel!==state.samePanel || last.resultPresent!==state.resultPresent || last.expanded!==state.expanded) {
                        if(trace.views.length<64) trace.views.push(state);
                        else trace.overflow=true;
                      }
                    };
                    observe();
                    trace.observer=new MutationObserver(observe);
                    trace.observer.observe(document.body,{childList:true,subtree:true,attributes:true,attributeFilter:['aria-expanded','data-search-node']});
                """)
                repeat_stable = js("""
                    const done=arguments[arguments.length-1], trace=window.__receiptTrace;
                    const startMetadata=trace.metadata.length, startViews=trace.views.length;
                    setTimeout(()=>{
                      const panel=document.querySelector('[aria-label="본문 검색"]');
                      done(!trace.overflow && trace.metadata.length===startMetadata && trace.views.length===startViews && panel===trace.initialPanel && !!panel?.querySelector('[data-search-node]'));
                    },800);
                """, asynchronous=True)
                require(repeat_stable, "search changed before repeat ACK response")
                require(js("return window.__syntheticAckRetry==='pending'"), "repeat ACK completed before response release")
                require(local_operation(node_id)[6] == confirmed[6], "confirmed watermark changed during repeat search")
                require(change_requests() == changes_before and sync_cursor() == cursor_before, "metadata catch-up interfered before repeat ACK release")
                repeat_outbox_before = local_outbox_snapshot()
                repeat_server_before = server_stream(confirmed[10])
                js("window.__receiptTrace.phase='repeat_released'")
                inner.release_ack.set()
                require(inner.ack_response_delivered.wait(10), "repeat ACK response was not delivered")
                retry_result = wait(lambda: js("return window.__syntheticAckRetry==='pending' ? null : window.__syntheticAckRetry"), "repeat ACK completion", seconds=10)
                require(retry_result == "idle", "repeat ACK sender did not complete without another operation")
                after_repeat = local_operation(node_id)
                require(after_repeat == confirmed and confirmation_timestamps() == confirm_times, "repeat ACK changed the committed state or watermark")
                require(local_outbox_snapshot() == repeat_outbox_before and server_stream(confirmed[10]) == repeat_server_before, "repeat ACK changed the outbox or server stream")
                repeat_preserved = js("""
                    const done=arguments[arguments.length-1], trace=window.__receiptTrace;
                    const startMetadata=trace.metadata.length, startViews=trace.views.length;
                    setTimeout(()=>{
                      const panel=document.querySelector('[aria-label="본문 검색"]');
                      done(!trace.overflow && trace.metadata.length===startMetadata && trace.views.length===startViews && panel===trace.initialPanel && !!panel?.querySelector('[data-search-node]'));
                    },800);
                """, asynchronous=True)
                repeat_trace = js("""
                    const trace=window.__receiptTrace;
                    return {metadata:trace.metadata,views:trace.views,overflow:trace.overflow};
                """)
                repeat_notifications = [event for event in repeat_trace["metadata"] if event["phase"] == "repeat_released"]
                repeat_diagnostic = {
                    "search_stable_before_repeat": repeat_stable,
                    "search_preserved_after_repeat": repeat_preserved,
                    "repeat_notifications": len(repeat_notifications),
                    "panel_changed": any(not event["panelPresent"] or not event["samePanel"] or not event["expanded"] or not event["resultPresent"] for event in repeat_trace["views"]),
                    "trace_overflow": repeat_trace["overflow"],
                }
                print("Search repeat ACK diagnostic:", json.dumps(repeat_diagnostic), flush=True)
                require(change_requests() == changes_before and sync_cursor() == cursor_before, "metadata catch-up interfered with repeat ACK")
                require(not inner.failure.is_set() and not outer.failure.is_set(), "isolated proxy reported a failure during repeat ACK")
                require(not repeat_trace["overflow"], "repeat ACK observation trace overflowed")
                require(not repeat_notifications, "repeat ACK emitted a metadata change")
                require(repeat_preserved and not repeat_diagnostic["panel_changed"], "repeat ACK cleared the unchanged search result")
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
                    if outer:
                        outer.close()
                finally:
                    try:
                        if inner:
                            inner.close()
                    finally:
                        helpers.stop(server)

    print("M3 search receipt WebView passed: local committed receipt invalidates search while ACK confirmation is pending.", flush=True)


if __name__ == "__main__":
    try:
        main()
    except TestFailure as failure:
        print(f"M3 search receipt WebView failed: {failure}", file=sys.stderr)
        raise SystemExit(1) from None
