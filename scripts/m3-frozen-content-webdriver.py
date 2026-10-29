#!/usr/bin/env python3
"""Verify frozen content inspection after an isolated response loss and epoch restore."""
import argparse
import hashlib
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
import threading
import time


def module(path, name):
    spec = importlib.util.spec_from_file_location(name, path)
    if spec is None or spec.loader is None:
        raise RuntimeError("test helper unavailable")
    result = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(result)
    return result


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--repo", default=str(Path(__file__).resolve().parents[1]))
    parser.add_argument("--driver", default="WebKitWebDriver")
    parser.add_argument("--binary", default="target/debug/notryx-client")
    parser.add_argument("--server", default="target/debug/notryx-server")
    args = parser.parse_args()
    repo = Path(args.repo).resolve()
    helpers = module(repo / "scripts/m2-workspace-webdriver.py", "m2_helpers")
    perf = module(repo / "scripts/m3-search-perf-webdriver.py", "perf_helpers")
    browser_helpers = module(repo / "scripts/m3-two-client-browser-helpers.py", "two_client_browser_helpers")
    isolation = module(repo / "scripts/m3-remote-delete-webdriver.py", "isolated_xvfb_helpers")
    proxy_module = module(repo / "scripts/m3_content_conflict_proxy.py", "conflict_proxy")
    require, TestFailure = helpers.require, helpers.TestFailure
    require(helpers.xvfb_on_current_display() and isolation.isolated_xvfb(), "fresh xvfb-run display owned by this test required")
    prefix = Path(os.environ.get("NOTRYX_TEST_SECRET_BUS_PREFIX", ""))
    address = os.environ.get("DBUS_SESSION_BUS_ADDRESS", "")
    require(prefix.parent == Path("/tmp") and re.fullmatch(r"notryx-secret-test\.[A-Za-z0-9]{8}", prefix.name) is not None
        and not prefix.is_symlink() and prefix.is_dir() and prefix.stat().st_uid == os.getuid()
        and stat.S_IMODE(prefix.stat().st_mode) == 0o700 and not (prefix / "bus.sock").is_symlink()
        and (prefix / "bus.sock").is_socket()
        and re.fullmatch(r"unix:path=" + re.escape(str(prefix / "bus.sock")) + r"(?:,guid=[0-9a-fA-F]{32})?", address) is not None
        and not os.environ.get("DBUS_STARTER_ADDRESS") and not os.environ.get("DBUS_STARTER_BUS_TYPE"),
        "scripts/m3-secret-no-service.sh isolated session bus required")
    require(hasattr(os, "pidfd_open") and hasattr(signal, "pidfd_send_signal"), "pidfd support is required")
    fixture = Path(tempfile.mkdtemp(prefix="notryx-m3-frozen-content-"))
    client_binary = str((repo / args.binary).resolve())
    server_binary = str((repo / args.server).resolve())
    browsers = []
    server = None
    server_fd = None
    server_identity = None
    proxy = None
    cleanup_safe = False
    previous_term = signal.getsignal(signal.SIGTERM)

    def interrupted(_signum, _frame):
        raise TestFailure("test interrupted; cleaning up isolated processes")

    signal.signal(signal.SIGTERM, interrupted)

    def check_watchdogs():
        for browser in browsers:
            browser.check()

    def wait(check, description, timeout=60):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            check_watchdogs()
            result = check()
            if result:
                return result
            time.sleep(0.1)
        raise TestFailure("timed out waiting for " + description)

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
        require(perf.observation_clear(perf.group_observation(server.pid, {server_identity})), "server termination unconfirmed; fixture retained")

    try:
        notes, state, run = (fixture / name for name in ("notes", "state", "run"))
        for path in (notes, state, run):
            path.mkdir(mode=0o700)
        client_directories = []
        for name in ("receiver", "peer"):
            root = fixture / name
            root.mkdir(mode=0o700)
            directories = {key: root / key for key in ("data", "xdg_data", "xdg_cache", "xdg_config")}
            for directory in directories.values():
                directory.mkdir(mode=0o700)
            client_directories.append(directories)
        port = helpers.free_port()
        config = fixture / "server.toml"
        config.write_text(f'[server]\nlisten = "127.0.0.1:{port}"\nadmin_socket = "{run}/admin.sock"\n[workspace]\nnotes_root = "{notes}"\nstate_root = "{state}"\n', encoding="utf-8")
        config.chmod(0o600)
        server_database = state / "state.db"
        base = b"\xef\xbb\xbf# Synthetic shared\r\nBase text\r\n"
        protected = b"# Synthetic protected\n"
        occupied = b"# Synthetic occupied destination\n"
        base_text = base.decode("utf-8-sig").replace("\r\n", "\n")
        local_suffix = "\nLocal edit 한글\n"
        remote_first_suffix = "\nFirst peer edit\n"
        remote_second_suffix = "\nSecond peer edit\n"
        local_text = base_text + local_suffix
        remote_first_text = base_text + remote_first_suffix
        remote_second_text = remote_first_text + remote_second_suffix
        local_bytes = b"\xef\xbb\xbf" + local_text.encode("utf-8")
        first_local_bytes = local_bytes
        remote_first_bytes = b"\xef\xbb\xbf" + remote_first_text.encode("utf-8")
        remote_second_bytes = b"\xef\xbb\xbf" + remote_second_text.encode("utf-8")

        def cli(*arguments):
            check_watchdogs()
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

        def query(index, sql, values=()):
            path = client_directories[index]["data"] / "drafts.sqlite3"
            if not path.exists():
                return []
            with sqlite3.connect(f"file:{path}?mode=ro", uri=True, timeout=5) as db:
                return db.execute(sql, values).fetchall()

        def cached_node(index, node_id):
            rows = query(index, """SELECT n.state FROM remote_nodes n JOIN client_registrations r ON r.id=n.registration_id
                WHERE r.status='active' AND n.node_id=?""", (node_id,))
            return json.loads(rows[0][0]) if len(rows) == 1 else None

        def cursor(index):
            rows = query(index, "SELECT i.cursor FROM remote_index i JOIN client_registrations r ON r.id=i.registration_id WHERE r.status='active'")
            return int(rows[0][0]) if len(rows) == 1 else None

        def device(index):
            rows = query(index, "SELECT device_id FROM client_registrations WHERE status='active'")
            return rows[0][0] if len(rows) == 1 else None

        def checkpoint(index, node_id):
            rows = query(index, """SELECT c.generation,c.bytes,b.bytes,b.revision FROM registered_documents d
                JOIN client_registrations r ON r.id=d.registration_id
                JOIN draft_checkpoints c ON c.document_id=d.storage_id
                JOIN draft_bases b ON b.document_id=d.storage_id
                WHERE r.status='active' AND d.document_id=?""", (node_id,))
            return rows[0] if len(rows) == 1 else None

        def outbox(index, node_id):
            return query(index, """SELECT o.operation_id,o.operation_seq,o.generation,o.expected_revision,o.bytes,o.payload_hash,
                o.status,o.conflict_revision,o.conflict_etag,o.terminal_kind,s.ack_through,s.confirmed_through
                FROM client_outbox o JOIN client_registrations r ON r.id=o.registration_id
                JOIN outbox_streams s ON s.registration_id=r.id
                WHERE r.status='active' AND o.operation_kind='content' AND o.document_id=? ORDER BY o.operation_seq""", (node_id,))

        def followup_row(node_id):
            rows = query(0, """SELECT o.operation_id,o.operation_seq,o.generation,o.expected_revision,o.bytes,
                o.payload_hash,o.status,o.dependency,o.abandon_state,o.terminal_kind,
                s.ack_through,s.confirmed_through,r.device_stream_id
                FROM client_outbox o JOIN client_registrations r ON r.id=o.registration_id
                JOIN outbox_streams s ON s.registration_id=r.id
                WHERE r.status='active' AND o.operation_kind='content' AND o.document_id=?
                ORDER BY o.operation_seq""", (node_id,))
            return rows[1] if len(rows) == 2 else None

        def server_followup(operation_id, stream_id):
            with sqlite3.connect(f"file:{server_database}?mode=ro", uri=True, timeout=5) as db:
                db.execute("BEGIN")
                stream = db.execute("SELECT last_seq,retired_through FROM operation_streams WHERE stream_id=?", (stream_id,)).fetchone()
                retired = db.execute("SELECT stream_id,operation_seq FROM retired_operations WHERE operation_id=?", (operation_id,)).fetchone()
                remaining = db.execute("SELECT COUNT(*) FROM operation_receipts WHERE operation_id=?", (operation_id,)).fetchone()[0]
                return stream, retired, remaining

        def peer_commit(node_id, generation):
            rows = outbox(1, node_id)
            matching = [row for row in rows if row[2] == generation and row[6] == "committed" and row[10] >= row[1] and row[11] >= row[1]]
            return matching[0] if len(matching) == 1 else None

        def conflict(node_id):
            rows = outbox(0, node_id)
            matching = [row for row in rows if row[6] == "conflict" and row[10] >= row[1] and row[11] >= row[1]]
            return matching[0] if len(matching) == 1 else None

        def conflict_intent(operation_id):
            rows = query(0, """SELECT i.operation_id,i.document_id,i.generation,i.base_revision,i.base_bytes,
                i.local_bytes,i.remote_revision,i.remote_hash,i.remote_bytes,i.choice,
                i.completed_generation,i.completed_through,i.intent_version
                FROM content_conflict_intents i JOIN client_registrations r ON r.id=i.registration_id
                WHERE r.status='active' AND i.operation_id=?""", (operation_id,))
            return rows[0] if len(rows) == 1 else None

        def copy_intent(operation_id):
            rows = query(0, """SELECT c.intent_version,c.parent_id,c.name,c.node_id,c.create_operation_id,c.content_operation_id
                FROM content_conflict_copies c JOIN client_registrations r ON r.id=c.registration_id
                WHERE r.status='active' AND c.operation_id=?""", (operation_id,))
            return rows[0] if len(rows) == 1 else None

        def copy_attempt(operation_id, old_create_id):
            rows = query(0, """SELECT old_parent_id,old_name,old_node_id,new_parent_id,new_name,new_node_id,new_create_operation_id,intent_version
                FROM content_conflict_copy_attempts WHERE operation_id=? AND old_create_operation_id=?""", (operation_id, old_create_id))
            return rows[0] if len(rows) == 1 else None

        def operation(operation_id):
            rows = query(0, """SELECT o.operation_id,o.operation_seq,o.operation_kind,o.document_id,o.expected_revision,
                o.bytes,o.status,o.committed_revision,o.receipt_json,s.ack_through,s.confirmed_through
                FROM client_outbox o JOIN client_registrations r ON r.id=o.registration_id
                JOIN outbox_streams s ON s.registration_id=r.id
                WHERE r.status='active' AND o.operation_id=?""", (operation_id,))
            return rows[0] if len(rows) == 1 else None

        def server_copy(node_id):
            with sqlite3.connect(f"file:{server_database}?mode=ro", uri=True, timeout=5) as db:
                return db.execute("SELECT node_id,parent_id,name,content_rev,content_hash,last_change_seq FROM nodes WHERE node_id=? AND kind='file' AND deleted=0", (node_id,)).fetchone()

        def server_document():
            with sqlite3.connect(f"file:{server_database}?mode=ro", uri=True, timeout=5) as db:
                return db.execute("SELECT node_id,content_rev,content_hash,last_change_seq FROM nodes WHERE name='Shared.md' AND kind='file' AND deleted=0").fetchone()

        def workspace_evidence():
            with sqlite3.connect(f"file:{server_database}?mode=ro", uri=True, timeout=5) as db:
                db.execute("BEGIN")
                head = db.execute("SELECT head_seq FROM workspaces WHERE singleton=1").fetchone()
                journal = db.execute("SELECT seq,operation_id FROM change_sets ORDER BY CAST(seq AS INTEGER)").fetchall()
                nodes = db.execute("""SELECT node_id,parent_id,name,normalized_name_key,kind,content_rev,meta_rev,
                    content_hash,size,children_generation,last_change_seq,deleted FROM nodes
                    WHERE name='Protected.md' AND kind='file' AND deleted=0""").fetchall()
                if head is None or len(nodes) != 1:
                    return None
                return (head[0], tuple(journal), nodes[0])

        def focus_except(window):
            focus = helpers.capture_remote_change_focus(exclude_window=window)
            focus[0].XCloseDisplay(focus[1])
            return focus[2]

        def verify_focus(active, inactive):
            wait(lambda: active.js("return document.hasFocus()") and not inactive.js("return document.hasFocus()"), "independent client focus")

        def pair(browser, secret, origin, phase, expected_epoch=None, retry_preconditions=None):
            calls_before = proxy.route_count("pair")
            last_sync = {"status":"unavailable", "epoch_matches":False}

            def sync_ready():
                nonlocal last_sync
                last_sync = browser.js("""const done=arguments[arguments.length-1], expected=arguments[0];
                  window.__TAURI_INTERNALS__.invoke('m0_renderer_session')
                  .then(owner=>window.__TAURI_INTERNALS__.invoke('workspace_sync_status',{request:{renderer_session_id:owner}}))
                  .then(value=>{const allowed=['connecting','ready','offline','auth_required','gateway_auth_required','needs_review'];
                    done({status:allowed.includes(value.status)?value.status:'invalid',epoch_matches:expected===null || value.binding?.workspace_epoch===expected});})
                  .catch(()=>done({status:'unavailable',epoch_matches:false}));""", expected_epoch, asynchronous=True)
                return last_sync["status"] == "ready" and last_sync["epoch_matches"]

            step = "form"
            try:
                wait(lambda: browser.js("return !!document.querySelector('.workspace-connect button:not(:disabled)')"), phase + " pairing form")
                browser.js("""window.__notryxPairTrace?.observer?.disconnect();
                  const start=performance.now(), events=[];
                  const codes=['transmission_in_progress','open_edit_sessions','pairing_in_progress',
                    'pairing_pending_adoption','pairing_result_unknown','stale_renderer_session','stale_registration'];
                  const add=code=>{if(!events.some(e=>e.code===code)) events.push({code,ms:Math.round(performance.now()-start)});};
                  const record=()=>{for(const el of document.querySelectorAll('[role=alert]')) {
                    const text=el.textContent||''; let matched=false;
                    for(const code of codes) if(text.includes(code)){add(code); matched=true;}
                    if(text.includes('기기 인증이 만료되었거나 취소')){add('auth_required'); matched=true;}
                    if(text.includes('gateway 인증을 확인')){add('gateway_auth_required'); matched=true;}
                    if(text.trim() && !matched) add('other_alert');
                  }};
                  const observer=new MutationObserver(record); observer.observe(document.body,{subtree:true,childList:true,characterData:true});
                  window.__notryxPairTrace={events,observer};""")
                step = "submit"
                browser.js("""const form=document.querySelector('.workspace-connect');
                  form.elements.namedItem('origin').value=arguments[0];
                  form.elements.namedItem('workspace_id').value=arguments[1];
                  form.elements.namedItem('pairing_secret').value=arguments[2]; form.requestSubmit();""",
                  origin, secret["workspace_id"], secret["pairing_secret"])
                step = "secret_clear"
                require(browser.js("return document.querySelector('[name=pairing_secret]').value === ''"), phase + " pairing secret remained in form")
                step = "sync"
                def outcome():
                    if sync_ready(): return "ready"
                    trace = browser.js("return window.__notryxPairTrace?.events || []")
                    if any(event.get("code") != "transmission_in_progress" for event in trace): return "other_error"
                    if any(event.get("code") == "transmission_in_progress" for event in trace): return "transmission_in_progress"
                    return None
                result = wait(outcome, phase + " sync")
                if result == "other_error":
                    raise TestFailure("unexpected pairing alert")
                if result == "transmission_in_progress":
                    if retry_preconditions is None:
                        raise TestFailure("unexpected transmission during initial pairing")
                    trace = browser.js("return window.__notryxPairTrace?.events || []")
                    require(all(event.get("code") == "transmission_in_progress" for event in trace), "additional pairing error observed")
                    require(proxy.route_count("pair") == calls_before, "pair request occurred before transmission retry")
                    with proxy.lock:
                        require(proxy.inflight.get("pair", 0) == 0, "pair request still in flight")
                    require(retry_preconditions(), "pair retry preconditions failed")
                    step = "wait_transmission_end"
                    wait(lambda: browser.js("return !!window.__notryxSendTrace && window.__notryxSendTrace.started>0 && window.__notryxSendTrace.inflight===0 && window.__notryxSendTrace.finished===window.__notryxSendTrace.started"), "prior native send completion")
                    require(proxy.route_count("pair") == calls_before and retry_preconditions(), "pair retry preconditions changed")
                    trace = browser.js("return window.__notryxPairTrace?.events || []")
                    require(all(event.get("code") == "transmission_in_progress" for event in trace), "additional pairing error observed before retry")
                    with proxy.lock:
                        require(proxy.inflight.get("pair", 0) == 0, "pair request still in flight")
                    print(f"Synthetic pairing retry: alert_timeline={trace}, no_pair_post=True, old_outbox_unchanged=True, grant_unused=True, native_send_finished=True", flush=True)
                    step = "retry_submit"
                    submitted = browser.js("""const send=window.__notryxSendTrace;
                      if(!send || send.inflight!==0 || send.finished!==send.started) return false;
                      const form=document.querySelector('.workspace-connect');
                      if(!form || form.querySelector('button:disabled')) return false;
                      form.elements.namedItem('origin').value=arguments[0]; form.elements.namedItem('workspace_id').value=arguments[1];
                      form.elements.namedItem('pairing_secret').value=arguments[2]; form.requestSubmit(); return true;""",
                      origin, secret["workspace_id"], secret["pairing_secret"])
                    require(submitted, "native send restarted before retry")
                    require(browser.js("return document.querySelector('[name=pairing_secret]').value === ''"), "retried pairing secret remained in form")
                    step = "retry_sync"
                    wait(sync_ready, phase + " retry sync")
                browser.js("window.__notryxPairTrace?.observer?.disconnect()")
            except Exception as cause:
                # Diagnostics are classifications only; form values and alert text may contain credentials.
                try:
                    ui = browser.js("""const form=document.querySelector('.workspace-connect');
                      const alerts=[...document.querySelectorAll('[role=alert]')].map(x=>x.textContent||'');
                      const text=alerts.join(' ');
                      return {form:!!form, formDisabled:!!form?.querySelector('button:disabled'),
                        secretCleared:form?.elements.namedItem('pairing_secret')?.value==='',
                        selectedDocument:!!document.querySelector('[data-document][aria-current=page], [data-draft-document][aria-current=page]'),
                        editor:!!document.querySelector('.cm-content'), editable:!!document.querySelector('.cm-content[contenteditable=true]'),
                        alert:alerts.length>0, compositionAlert:/조합/.test(text), sessionAlert:/session|세션|stale/i.test(text),
                        queueAlert:/queue|전송.*준비|checkpoint/i.test(text), pairingAlert:/pairing|등록|인증/i.test(text),
                        timeline:window.__notryxPairTrace?.events || [], send:window.__notryxSendTrace || null};""")
                except Exception:
                    ui = {"diagnostic":"unavailable"}
                with proxy.lock:
                    pair_calls = proxy.route_counts.get("pair", 0)
                    pair_inflight = proxy.inflight.get("pair", 0)
                    connections = len(proxy.connections)
                    pair_responses = {key:count for key,count in proxy.response_counts.items() if key.startswith("pair:")}
                raise TestFailure(f"{phase} pairing failed: stage={step}, error_type={type(cause).__name__}, "
                    f"pair_requests_before={calls_before}, pair_requests_after={pair_calls}, pair_inflight={pair_inflight}, proxy_connections={connections}, "
                    f"pair_responses={pair_responses}, proxy_failed={proxy.failure.is_set()}, "
                    f"sync={last_sync['status']}, expected_epoch_matches={last_sync['epoch_matches']}, ui={ui}") from cause

        def open_document(browser, node_id):
            wait(lambda: browser.js("return !!document.querySelector('[data-document=\"'+arguments[0]+'\"]:not(:disabled)')", node_id), "document row")
            browser.js("document.querySelector('[data-document=\"'+arguments[0]+'\"]').click()", node_id)
            wait(lambda: browser.js("return document.querySelector('[aria-current=page]')?.dataset.document===arguments[0] && !!document.querySelector('.cm-content[contenteditable=true]')", node_id), "opened editor")

        def editor_text(browser):
            return browser.js("return document.querySelector('.cm-content').cmTile.root.view.state.doc.toString()")

        def begin_edit(browser, suffix):
            browser.js("""const element=document.querySelector('.cm-content');
                element.dispatchEvent(new CompositionEvent('compositionstart',{bubbles:true}));
                const view=element.cmTile.root.view;
                view.dispatch({changes:{from:view.state.doc.length,insert:arguments[0]},userEvent:'input.type'});""", suffix)

        def end_edit(browser):
            browser.js("""const done=arguments[arguments.length-1];
                document.querySelector('.cm-content').dispatchEvent(new CompositionEvent('compositionend',{bubbles:true}));
                setTimeout(done,400);""", asynchronous=True)
            wait(lambda: browser.js("return !document.querySelector('.cm-content').cmTile.root.view.composing"), "composition end")

        def observe_editor(browser, expected):
            observation_started = time.monotonic()
            observation_deadline = observation_started + 1.3
            observations = 0
            while time.monotonic() < observation_deadline:
                roundtrip_started = time.monotonic()
                require(browser.js("""const element=document.querySelector('.cm-content');
                    return !!element && element.isContentEditable &&
                      element.cmTile.root.view.state.doc.toString()===arguments[0];""", expected),
                    "renderer did not remain responsive during the 1.3 second observation")
                require(time.monotonic() - roundtrip_started <= 0.75,
                    "renderer round trip exceeded 0.75 seconds during the observation")
                observations += 1
                time.sleep(0.1)
            require(observations >= 2, "insufficient renderer observations during the 1.3 second interval")
            require(1.3 <= time.monotonic() - observation_started <= 2.5,
                "renderer observation did not complete within 2.5 seconds")
            check_watchdogs()

        try:
            cli("init", "--config", str(config))
            (notes / "Shared.md").write_bytes(base)
            (notes / "Protected.md").write_bytes(protected)

            def start_isolated_server():
                nonlocal server, server_fd, server_identity
                server = subprocess.Popen([server_binary, "serve", "--config", str(config)], stdout=subprocess.PIPE,
                    stderr=subprocess.DEVNULL, text=True, start_new_session=True)
                server_fd = os.pidfd_open(server.pid)
                process_stat = perf.process_stat(server.pid)
                if process_stat is None:
                    raise TestFailure("synthetic server identity unavailable")
                server_identity = (server.pid, process_stat["starttime"])
                ready, _, _ = select.select([server.stdout], [], [], 30)
                if not ready or server.stdout.readline().strip() != "server_started" or not helpers.leader_running(server):
                    raise TestFailure("synthetic server did not start")
                return server, server_fd, server_identity

            server, server_fd, server_identity = start_isolated_server()
            initial = wait(server_document, "initial synthetic document")
            node_id, base_revision = initial[0], initial[1]
            require((notes / "Shared.md").read_bytes() == base, "initial synthetic bytes differ")
            class PhaseFailures:
                def __init__(self):
                    self.lock = threading.Lock()
                    self.phase = "before_restore"
                    self.counts = {"before_restore":0, "planned_outage":0, "after_restore":0}
                def set(self):
                    with self.lock: self.counts[self.phase] += 1
                def mark(self, phase):
                    with self.lock: self.phase = phase
                def is_set(self):
                    with self.lock: return self.counts["before_restore"] > 0 or self.counts["after_restore"] > 0
            proxy = proxy_module.ContentConflictProxy(port, node_id)
            proxy.failure = PhaseFailures()
            proxy.start()
            principal = admin("status")["owner_principal_id"]
            grant = admin("issue-pairing", principal)
            directories = client_directories[0]
            environment = os.environ.copy()
            environment.update(TAURI_WEBVIEW_AUTOMATION="true", NOTRYX_M0_DATA_DIR=str(directories["data"]),
                XDG_DATA_HOME=str(directories["xdg_data"]), XDG_CACHE_HOME=str(directories["xdg_cache"]), XDG_CONFIG_HOME=str(directories["xdg_config"]))
            browser = browser_helpers.Browser(perf, helpers, fixture, client_binary, args.driver, environment)
            browsers.append(browser); browser.start()
            pair(browser, grant, f"http://127.0.0.1:{proxy.server_address[1]}", "initial")
            old_registration = query(0, "SELECT id,workspace_id,workspace_epoch,device_id,device_stream_id,status FROM client_registrations WHERE status='active'")
            require(len(old_registration) == 1, "old registration unavailable")
            old_id, workspace_id, old_epoch, old_device, old_stream, _ = old_registration[0]
            wait(lambda: cached_node(0, node_id), "old registration metadata")
            open_document(browser, node_id)
            begin_edit(browser, local_suffix)
            sent_checkpoint = wait(lambda: (row if (row := checkpoint(0, node_id)) and row[1] == first_local_bytes else None), "sent draft checkpoint")
            require(sent_checkpoint[2:] == (base, base_revision) and not outbox(0, node_id), "sent draft base differs")
            proxy.hold_original_success.set(); proxy.discard_original_success.set(); proxy.hold_uncertain_lookup.set()
            installed = browser.js("""const original=window.fetch, target=window.__TAURI_INTERNALS__.convertFileSrc('workspace_send_next','ipc');
              const state={started:0,finished:0,inflight:0}; window.__notryxSendTrace=state;
              const wrapped=function(input,init){const url=typeof input==='string'?input:input.url;
                if(url!==target) return original.call(this,input,init);
                state.started++; state.inflight++;
                return original.call(this,input,init).finally(()=>{state.inflight--; state.finished++;});};
              window.fetch=wrapped; return window.fetch===wrapped;""")
            require(installed, "send completion observation could not be installed")
            end_edit(browser); browser.js("document.querySelector('#save').click()")
            wait(proxy.original_success_held.is_set, "server committed original PUT")
            sent = wait(lambda: (rows[0] if len(rows := outbox(0, node_id)) == 1 and rows[0][6] == "sent" else None), "unconfirmed sent operation")
            require(sent[3] == base_revision and sent[4] == first_local_bytes and (notes / "Shared.md").read_bytes() == first_local_bytes, "sent operation or server bytes differ")
            proxy.original_operation_path = f"/v1/operations/{sent[0]}"
            proxy.release_original_success.set()
            wait(proxy.original_success_discarded.is_set, "discarded successful PUT response")
            later_suffix = "\nLater synthetic draft 한글\n"
            draft_text = local_text + later_suffix
            draft_bytes = b"\xef\xbb\xbf" + draft_text.encode("utf-8")
            begin_edit(browser, later_suffix)
            draft = wait(lambda: (row if (row := checkpoint(0, node_id)) and row[1] == draft_bytes else None), "later durable checkpoint")
            require(draft[0] > sent_checkpoint[0] and draft[2:] == (base, base_revision), "later checkpoint changed original base")
            end_edit(browser)
            require(outbox(0, node_id)[0][6] == "sent" and proxy.original_success_discarded.is_set(), "original result was applied before restore")

            require(not proxy.failure.is_set(), "proxy failed before planned restore outage")
            proxy.failure.mark("planned_outage")
            stop_server(); os.close(server_fd)
            server = None; server_fd = None; server_identity = None
            backup, restored = fixture / "cold-backup", fixture / "restored"
            cli("backup", "--config", str(config), str(backup))
            cli("restore", str(backup), str(restored))
            notes, state = restored / "notes", restored / "state"
            server_database = state / "state.db"
            config = fixture / "restored-live.toml"
            config.write_text(f'[server]\nlisten = "127.0.0.1:{port}"\nadmin_socket = "{run}/admin.sock"\n[workspace]\nnotes_root = "{notes}"\nstate_root = "{state}"\n', encoding="utf-8")
            config.chmod(0o600)
            server, server_fd, server_identity = start_isolated_server()
            proxy.failure.mark("after_restore")
            remote_text = "# Synthetic restored server\nIndependent current content\n"
            remote_bytes = remote_text.encode("utf-8")
            (notes / "Shared.md").write_bytes(remote_bytes)
            current = wait(lambda: (row if (row := server_document()) and row[2] == hashlib.sha256(remote_bytes).hexdigest() else None), "restored current server content")
            status = admin("status")
            require(status["workspace_id"] == workspace_id and status["workspace_epoch"] != old_epoch, "restored workspace or epoch differs")
            current_epoch = status["workspace_epoch"]
            new_grant = admin("issue-pairing", status["owner_principal_id"])
            wait(lambda: (rows if len(rows := outbox(0, node_id)) >= 2 and rows[1][6] == "queued" and rows[1][2] == draft[0] else None), "old durable followup queued")
            old_outbox_before_pair = outbox(0, node_id)
            secret_parts = new_grant["pairing_secret"].split(".")
            require(len(secret_parts) == 3 and secret_parts[0] == "pair", "invalid synthetic pairing grant")
            pairing_id = secret_parts[1]
            def retry_preconditions():
                if outbox(0, node_id) != old_outbox_before_pair: return False
                with sqlite3.connect(f"file:{server_database}?mode=ro", uri=True, timeout=5) as connection:
                    rows = connection.execute("SELECT workspace_id,consumed_at,expires_at FROM pairing_secrets WHERE pairing_id=?", (pairing_id,)).fetchall()
                return len(rows) == 1 and rows[0][0] == workspace_id and rows[0][1] is None and rows[0][2] > int(time.time())
            pair(browser, new_grant, f"http://127.0.0.1:{proxy.server_address[1]}", "re-registration", current_epoch, retry_preconditions)
            def registrations():
                return query(0, "SELECT id,workspace_id,workspace_epoch,device_id,device_stream_id,status FROM client_registrations ORDER BY id")
            new_registration = wait(lambda: (rows if len(rows := registrations()) >= 2 and any(r[0] == old_id and r[5] == "frozen" for r in rows) and any(r[2] == current_epoch and r[5] == "active" for r in rows) else None), "new registration and frozen old registration")
            active = next(row for row in new_registration if row[5] == "active")
            require(active[1] == workspace_id and active[2] == current_epoch and active[3] != old_device and active[4] != old_stream, "new registration did not adopt restored epoch")
            wait(lambda: (row if (row := cached_node(0, node_id)) and row.get("content_rev") == current[1] else None), "new registration bootstrap")
            require(browser.session is not None and not browser.closed, "WebView session did not survive re-registration")

            def client_snapshot():
                db = client_directories[0]["data"] / "drafts.sqlite3"
                with sqlite3.connect(f"file:{db}?mode=ro", uri=True, timeout=5) as connection:
                    connection.execute("BEGIN")
                    registration = connection.execute("SELECT id,server_origin,workspace_id,workspace_epoch,device_id,device_stream_id,status FROM client_registrations ORDER BY id").fetchall()
                    operations = connection.execute("SELECT operation_id,registration_id,operation_seq,operation_kind,document_id,generation,expected_revision,dependency,bytes,payload_hash,status,abandon_state,terminal_kind,receipt_json FROM client_outbox ORDER BY registration_id,operation_seq").fetchall()
                    drafts = connection.execute("SELECT d.registration_id,d.document_id,c.generation,c.bytes,b.revision,b.bytes FROM registered_documents d JOIN draft_checkpoints c ON c.document_id=d.storage_id JOIN draft_bases b ON b.document_id=d.storage_id ORDER BY d.registration_id,d.document_id").fetchall()
                    streams = connection.execute("SELECT registration_id,ack_through,confirmed_through FROM outbox_streams ORDER BY registration_id").fetchall()
                    index = connection.execute("SELECT registration_id,cursor FROM remote_index ORDER BY registration_id").fetchall()
                return registration, operations, drafts, streams, index

            def server_snapshot():
                with sqlite3.connect(f"file:{server_database}?mode=ro", uri=True, timeout=5) as connection:
                    connection.execute("BEGIN")
                    workspace = connection.execute("SELECT workspace_id,epoch,head_seq FROM workspaces WHERE singleton=1").fetchone()
                    nodes = connection.execute("SELECT node_id,parent_id,name,kind,content_rev,meta_rev,content_hash,size,deleted,last_change_seq FROM nodes ORDER BY node_id").fetchall()
                    changes = connection.execute("SELECT seq,operation_id FROM change_sets ORDER BY CAST(seq AS INTEGER)").fetchall()
                    receipts = connection.execute("SELECT operation_id,stream_id,operation_seq FROM operation_receipts ORDER BY operation_id").fetchall()
                    retired = connection.execute("SELECT operation_id,stream_id,operation_seq FROM retired_operations ORDER BY operation_id").fetchall()
                return workspace, nodes, changes, receipts, retired

            before_client = client_snapshot(); before_server = server_snapshot()
            old_operations = [row for row in before_client[1] if row[1] == old_id]
            original = next((row for row in old_operations if row[0] == sent[0]), None)
            old_draft = next((row for row in before_client[2] if row[0] == old_id and row[1] == node_id), None)
            require(original and original[10] == "sent" and original[8] == first_local_bytes and original[6] == base_revision
                and original[9] == "sha256:" + hashlib.sha256(first_local_bytes).hexdigest(), "frozen sent request differs")
            require(old_draft and old_draft[2:] == (draft[0], draft_bytes, base_revision, base), "frozen latest draft or base differs")
            require(not [row for row in before_client[1] if row[1] == active[0]], "new registration already has outbox operations")
            wait(lambda: browser.js("return [...document.querySelectorAll('button')].some(b=>b.textContent.trim()==='이전 본문 요청 대조' && !b.disabled)"), "frozen content review control")
            browser.js("[...document.querySelectorAll('button')].find(b=>b.textContent.trim()==='이전 본문 요청 대조').click()")
            wait(lambda: browser.js("return !!document.querySelector('[aria-label=\"이전 본문 요청 대조\"]')"), "frozen review panel")
            wait(lambda: browser.js("return !!document.querySelector('[data-review-operation=\"'+arguments[0]+'\"]')", sent[0]), "frozen sent request listing")
            require(browser.js("return document.querySelector('[data-review-operation=\"'+arguments[0]+'\"]')?.textContent.includes('미확정')", sent[0]), "sent result was not shown as unknown")
            require(client_snapshot() == before_client and server_snapshot() == before_server, "listing changed client or server state")
            browser.js("document.querySelector('[data-review-operation=\"'+arguments[0]+'\"]').click()", sent[0])
            wait(lambda: browser.js("return !!document.querySelector('[data-testid=frozen-content-remote] textarea')"), "frozen source comparison")
            displayed = browser.js("""const section=document.querySelector('[aria-label="이전 본문 요청 대조"]');
                const value=id=>{const element=section.querySelector('[data-testid="'+id+'"] textarea'); return {value:element?.value, readOnly:element?.readOnly};};
                return {payload:value('frozen-content-payload'),base:value('frozen-content-base'),draft:value('frozen-content-draft'),remote:value('frozen-content-remote'),
                  epochs:section.textContent.includes(arguments[0]) && section.textContent.includes(arguments[1]),
                  warning:section.textContent.includes('epoch가 다릅니다') && section.textContent.includes('미확정')};""", old_epoch, current_epoch)
            require(displayed["payload"] == {"value":local_text,"readOnly":True} and displayed["base"] == {"value":base_text,"readOnly":True}
                and displayed["draft"] == {"value":draft_text,"readOnly":True} and displayed["remote"] == {"value":remote_text,"readOnly":True}
                and displayed["epochs"] and displayed["warning"], "frozen comparison differs from preserved sources or epoch evidence")
            require(client_snapshot() == before_client and server_snapshot() == before_server
                and (notes / "Shared.md").read_bytes() == remote_bytes and (notes / "Protected.md").read_bytes() == protected,
                "frozen inspection changed drafts, registrations, outboxes or server state")
            require(not proxy.failure.is_set(), "synthetic proxy failed")
        finally:
            failures = []
            previous_interrupt = signal.signal(signal.SIGINT, signal.SIG_IGN)
            signal.signal(signal.SIGTERM, signal.SIG_IGN)
            try:
                for browser in reversed(browsers):
                    try:
                        browser.close()
                    except BaseException:
                        failures.append("browser termination unconfirmed")
                if proxy is not None:
                    try:
                        proxy.close()
                        with proxy.lock:
                            active = sum(proxy.inflight.values())
                        if active or proxy.thread.is_alive():
                            failures.append("proxy termination unconfirmed")
                    except BaseException:
                        failures.append("proxy termination unconfirmed")
                try:
                    stop_server()
                except BaseException:
                    failures.append("server termination unconfirmed")
                if server_fd is not None:
                    os.close(server_fd)
            finally:
                signal.signal(signal.SIGINT, previous_interrupt)
                signal.signal(signal.SIGTERM, interrupted)
            if failures:
                raise TestFailure("; ".join(failures) + "; fixture retained")
        cleanup_safe = True
    finally:
        signal.signal(signal.SIGTERM, previous_term)
        if cleanup_safe:
            shutil.rmtree(fixture)
        else:
            print(f"Synthetic fixture retained for inspection: {fixture}", file=sys.stderr)
    print("M3 frozen content WebView passed: synthetic response loss, restored epoch, explicit inspection and preserved state")

if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        print(f"M3 frozen content WebView failed: {error}", file=sys.stderr)
        raise SystemExit(1)
