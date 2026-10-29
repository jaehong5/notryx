#!/usr/bin/env python3
"""Verify explicit frozen content dismissal with isolated clients and a synthetic server."""
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
    fixture = Path(tempfile.mkdtemp(prefix="notryx-m3-frozen-dismiss-"))
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



        def peer_commit(node_id, generation):
            rows = outbox(1, node_id)
            matching = [row for row in rows if row[2] == generation and row[6] == "committed" and row[10] >= row[1] and row[11] >= row[1]]
            return matching[0] if len(matching) == 1 else None





        def operation(operation_id):
            rows = query(0, """SELECT o.operation_id,o.operation_seq,o.operation_kind,o.document_id,o.expected_revision,
                o.bytes,o.status,o.committed_revision,o.receipt_json,s.ack_through,s.confirmed_through
                FROM client_outbox o JOIN client_registrations r ON r.id=o.registration_id
                JOIN outbox_streams s ON s.registration_id=r.id
                WHERE r.status='active' AND o.operation_id=?""", (operation_id,))
            return rows[0] if len(rows) == 1 else None


        def server_document():
            with sqlite3.connect(f"file:{server_database}?mode=ro", uri=True, timeout=5) as db:
                return db.execute("SELECT node_id,content_rev,content_hash,last_change_seq FROM nodes WHERE name='Shared.md' AND kind='file' AND deleted=0").fetchone()




        def pair(browser, secret, origin, phase, expected_epoch=None, retry_preconditions=None, previous_identity=None):
            calls_before = proxy.route_count("pair")
            with proxy.lock:
                successes_before = proxy.response_counts.get("pair:200", 0)
                pair_inflight_before = proxy.inflight.get("pair", 0)
            require(pair_inflight_before == 0, phase + " started with a pair request in flight")
            last_sync = {"status":"unavailable", "epoch_matches":False, "workspace_matches":False, "origin_matches":False, "identity_changed":False}

            def sync_ready():
                nonlocal last_sync
                previous_device, previous_stream = previous_identity or (None, None)
                last_sync = browser.js("""const done=arguments[arguments.length-1], expected=arguments[0], workspace=arguments[1], oldDevice=arguments[2], oldStream=arguments[3], origin=arguments[4];
                  window.__TAURI_INTERNALS__.invoke('m0_renderer_session')
                  .then(owner=>window.__TAURI_INTERNALS__.invoke('workspace_sync_status',{request:{renderer_session_id:owner}}))
                  .then(value=>{const allowed=['connecting','ready','offline','auth_required','gateway_auth_required','needs_review'], binding=value.binding;
                    done({status:allowed.includes(value.status)?value.status:'invalid',epoch_matches:expected===null || binding?.workspace_epoch===expected,
                      workspace_matches:binding?.workspace_id===workspace,origin_matches:binding?.server_origin===origin,
                      identity_changed:!!binding?.device_id && !!binding?.device_stream_id && (oldDevice===null || (binding.device_id!==oldDevice && binding.device_stream_id!==oldStream))});})
                  .catch(()=>done({status:'unavailable',epoch_matches:false,workspace_matches:false,origin_matches:false,identity_changed:false}));""", expected_epoch, secret["workspace_id"], previous_device, previous_stream, origin, asynchronous=True)
                with proxy.lock:
                    succeeded = proxy.response_counts.get("pair:200", 0) > successes_before
                    submitted = proxy.route_counts.get("pair", 0) > calls_before
                return submitted and succeeded and last_sync["status"] == "ready" and last_sync["epoch_matches"] and last_sync["workspace_matches"] and last_sync["origin_matches"] and last_sync["identity_changed"]

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
                  const internals=window.__TAURI_INTERNALS__, originalFetch=window.fetch, attempts=[];
                  const target=internals.convertFileSrc('workspace_pair','ipc');
                  if(typeof originalFetch!=='function') return false;
                  const wrapped=function(input,init){
                    const url=typeof input==='string'?input:input?.url;
                    if(url!==target) return originalFetch.call(this,input,init);
                    const attempt={status:'pending'}; attempts.push(attempt);
                    let result;
                    try { result=originalFetch.call(this,input,init); }
                    catch(error){attempt.status='unknown';throw error;}
                    return Promise.resolve(result).then(async response=>{
                      try {
                        const status=response.headers.get('Tauri-Response');
                        if(status==='ok') attempt.status='ok';
                        else if(status==='error') {
                          const error=await response.clone().json();
                          attempt.status=error==='transmission_in_progress'?'transmission_in_progress':'other';
                        } else attempt.status='unknown';
                      } catch { attempt.status='unknown'; }
                      return response;
                    },error=>{attempt.status='unknown';throw error;});
                  };
                  window.fetch=wrapped;
                  window.__notryxPairTrace={events,observer,attempts,originalFetch,wrapped};
                  return window.fetch===wrapped;""")
                require(browser.js("return window.fetch===window.__notryxPairTrace?.wrapped"), "pair IPC observation unavailable")
                max_attempts = 4
                for attempt_number in range(1, max_attempts + 1):
                    step = "submit" if attempt_number == 1 else "retry_submit"
                    if attempt_number > 1:
                        require(retry_preconditions is not None, "retry was not permitted")
                        wait(lambda: browser.js("return !!window.__notryxSendTrace && window.__notryxSendTrace.started>0 && window.__notryxSendTrace.inflight===0 && window.__notryxSendTrace.finished===window.__notryxSendTrace.started"), "prior native send completion")
                        wait(lambda: browser.js("return !!document.querySelector('.workspace-connect button:not(:disabled)')"), "retry pairing form")
                        time.sleep(0.25)
                        with proxy.lock:
                            no_pair_post = proxy.route_counts.get("pair", 0) == calls_before
                            no_pair_success = proxy.response_counts.get("pair:200", 0) == successes_before
                            no_pair_inflight = proxy.inflight.get("pair", 0) == 0
                        require(no_pair_post and no_pair_success and no_pair_inflight and retry_preconditions(),
                            "pair retry preconditions changed")
                    submitted = browser.js("""const retry=arguments[3], trace=window.__notryxPairTrace, send=window.__notryxSendTrace;
                      if(retry && (!send || send.inflight!==0 || send.finished!==send.started)) return false;
                      const form=document.querySelector('.workspace-connect');
                      if(!trace || !form || form.querySelector('button:disabled')) return false;
                      form.elements.namedItem('origin').value=arguments[0];
                      form.elements.namedItem('workspace_id').value=arguments[1];
                      form.elements.namedItem('pairing_secret').value=arguments[2]; form.requestSubmit(); return true;""",
                      origin, secret["workspace_id"], secret["pairing_secret"], attempt_number > 1)
                    require(submitted, "pairing form or send state changed")
                    require(browser.js("return document.querySelector('[name=pairing_secret]').value === ''"), phase + " pairing secret remained in form")
                    step = "pair_ipc"
                    def attempt_result():
                        statuses = browser.js("return window.__notryxPairTrace?.attempts.map(x=>x.status) || []")
                        if len(statuses) > attempt_number: raise TestFailure("unexpected extra pairing IPC")
                        if len(statuses) < attempt_number or statuses[-1] == "pending": return None
                        return statuses[-1]
                    result = wait(attempt_result, phase + " pairing IPC completion", timeout=30)
                    if result == "ok":
                        step = "sync"
                        wait(sync_ready, phase + " sync")
                        break
                    if result != "transmission_in_progress" or retry_preconditions is None or attempt_number == max_attempts:
                        raise TestFailure("pairing IPC did not complete")
                    with proxy.lock:
                        no_pair_post = proxy.route_counts.get("pair", 0) == calls_before
                        no_pair_success = proxy.response_counts.get("pair:200", 0) == successes_before
                        no_pair_inflight = proxy.inflight.get("pair", 0) == 0
                    require(no_pair_post and no_pair_success and no_pair_inflight and retry_preconditions(),
                        "rejected pairing cannot be safely retried")
                    print(f"Synthetic pairing retry: attempt={attempt_number}, ipc=transmission_in_progress, no_pair_post=True, grant_unused=True, old_data_unchanged=True", flush=True)
                else:
                    raise TestFailure("pairing attempts exhausted")
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
                        timeline:window.__notryxPairTrace?.events || [], attempts:window.__notryxPairTrace?.attempts || [], send:window.__notryxSendTrace || null};""")
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
                    f"sync={last_sync['status']}, expected_epoch_matches={last_sync['epoch_matches']}, "
                    f"workspace_matches={last_sync['workspace_matches']}, origin_matches={last_sync['origin_matches']}, "
                    f"identity_changed={last_sync['identity_changed']}, ui={ui}") from cause
            finally:
                try:
                    browser.js("""const trace=window.__notryxPairTrace;
                      trace?.observer?.disconnect();
                      if(trace && window.fetch===trace.wrapped) window.fetch=trace.originalFetch;""")
                except Exception:
                    pass

        def open_document(browser, node_id):
            wait(lambda: browser.js("return !!document.querySelector('[data-document=\"'+arguments[0]+'\"]:not(:disabled)')", node_id), "document row")
            browser.js("document.querySelector('[data-document=\"'+arguments[0]+'\"]').click()", node_id)
            wait(lambda: browser.js("return document.querySelector('[aria-current=page]')?.dataset.document===arguments[0] && !!document.querySelector('.cm-content[contenteditable=true]')", node_id), "opened editor")

        def focus_except(window):
            focus = helpers.capture_remote_change_focus(exclude_window=window)
            focus[0].XCloseDisplay(focus[1])
            return focus[2]

        def verify_focus(active, inactive):
            wait(lambda: active.js("return document.hasFocus()") and not inactive.js("return document.hasFocus()"), "independent client focus")

        def wait_for_metadata(browser, index, node_id, target_revision, description, trace):
            start = time.monotonic()
            last = None
            def observe():
                nonlocal last
                node = cached_node(index, node_id)
                status = browser.js("""const done=arguments[arguments.length-1];
                    window.__TAURI_INTERNALS__.invoke('m0_renderer_session')
                    .then(owner=>window.__TAURI_INTERNALS__.invoke('workspace_sync_status',{request:{renderer_session_id:owner}}))
                    .then(value=>done({focus:document.hasFocus(),status:value.status})).catch(()=>done({focus:document.hasFocus(),status:'unavailable'}));""", asynchronous=True)
                allowed = {'connecting', 'ready', 'offline', 'auth_required', 'gateway_auth_required', 'needs_review'}
                entry = (cursor(index), bool(node and node.get('content_rev') == target_revision), bool(status.get('focus')), status.get('status') if status.get('status') in allowed else 'other')
                if entry != last:
                    trace.append({'seconds':round(time.monotonic()-start, 1), 'cursor':entry[0], 'revision_matches':entry[1], 'focused':entry[2], 'sync_status':entry[3]})
                    last = entry
                return node if entry[1] and entry[2] and entry[3] == 'ready' else None
            try:
                return wait(observe, description)
            finally:
                print(f"Synthetic sync trace ({description}): {trace}", flush=True)

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
            class RoleObservingHandler(proxy_module.Handler):
                def forward(self):
                    original = self.rfile
                    server = self.server
                    class RequestReader:
                        def __init__(self):
                            self.first = True
                        def readline(self, limit=-1):
                            line = original.readline(limit)
                            if self.first:
                                self.first = False
                                with server.lock:
                                    if line.startswith(b"GET /v1/devices/self/role "):
                                        server.role_gets += 1
                                    if line.startswith(b"PUT "):
                                        server.put_requests += 1
                            return line
                        def __getattr__(self, name):
                            return getattr(original, name)
                    self.rfile = RequestReader()
                    try:
                        return super().forward()
                    finally:
                        self.rfile = original
            proxy.RequestHandlerClass = RoleObservingHandler
            proxy.role_gets = 0
            proxy.put_requests = 0
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
            receiver_focus = helpers.capture_remote_change_focus()
            receiver_window = receiver_focus[2]
            receiver_focus[0].XCloseDisplay(receiver_focus[1])
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
              const state={started:0,finished:0,inflight:0,paused:true,suppressed:0}; window.__notryxSendTrace=state;
              const wrapped=function(input,init){const url=typeof input==='string'?input:input.url;
                if(url!==target) return original.call(this,input,init);
                if(state.paused && state.finished>0){state.suppressed++; return Promise.resolve(new Response(JSON.stringify('synthetic send paused'),{status:200,headers:{'Tauri-Response':'error','Content-Type':'application/json'}}));}
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
            require(not proxy.failure.is_set(), "proxy failed before re-registration")
            wait(lambda: browser.js("return !!window.__notryxSendTrace && window.__notryxSendTrace.started>0 && window.__notryxSendTrace.inflight===0 && window.__notryxSendTrace.finished===window.__notryxSendTrace.started"), "original native send completion")
            require(len(outbox(0, node_id)) == 1 and outbox(0, node_id)[0][6] == "sent", "original operation was not left uncertain")
            proxy.hold_original_success.clear()
            later_suffix = "\nSynthetic later edit 한글\n"
            later_text = local_text + later_suffix
            later_bytes = b"\xef\xbb\xbf" + later_text.encode("utf-8")
            begin_edit(browser, later_suffix)
            later_checkpoint = wait(lambda: (row if (row := checkpoint(0, node_id)) and row[1] == later_bytes else None), "later draft checkpoint")
            end_edit(browser)
            browser.js("document.querySelector('#save').click()")
            successor = wait(lambda: (rows[1] if len(rows := outbox(0, node_id)) == 2 and rows[0][0] == sent[0] and rows[1][6] == "queued" else None), "queued successor")
            require(successor[2] == later_checkpoint[0] and successor[4] == later_bytes, "queued successor differs")
            dependency = query(0, "SELECT dependency FROM client_outbox WHERE operation_id=?", (successor[0],))
            require(dependency == [(sent[0],)], "successor does not depend on the original operation")
            browser.js("document.querySelector('[data-testid=close-document]').click()")
            wait(lambda: browser.js("return !document.querySelector('.cm-content[contenteditable=true]')"), "receiver closes edited document")
            new_grant = admin("issue-pairing", principal)
            def retry_client_snapshot():
                path = client_directories[0]["data"] / "drafts.sqlite3"
                with sqlite3.connect(f"file:{path}?mode=ro", uri=True, timeout=5) as connection:
                    connection.execute("BEGIN")
                    registrations = connection.execute("SELECT * FROM client_registrations ORDER BY id").fetchall()
                    operations = connection.execute("SELECT * FROM client_outbox ORDER BY operation_id").fetchall()
                    streams = connection.execute("SELECT * FROM outbox_streams ORDER BY registration_id").fetchall()
                    documents = connection.execute("SELECT * FROM registered_documents ORDER BY registration_id,document_id").fetchall()
                    checkpoints = connection.execute("SELECT * FROM draft_checkpoints ORDER BY document_id").fetchall()
                    bases = connection.execute("SELECT * FROM draft_bases ORDER BY document_id").fetchall()
                return registrations, operations, streams, documents, checkpoints, bases
            old_client_before_pair = retry_client_snapshot()
            secret_parts = new_grant["pairing_secret"].split(".")
            require(len(secret_parts) == 3 and secret_parts[0] == "pair", "invalid synthetic pairing grant")
            pairing_id = secret_parts[1]
            def retry_preconditions():
                if retry_client_snapshot() != old_client_before_pair: return False
                with sqlite3.connect(f"file:{server_database}?mode=ro", uri=True, timeout=5) as connection:
                    rows = connection.execute("SELECT workspace_id,consumed_at,expires_at FROM pairing_secrets WHERE pairing_id=?", (pairing_id,)).fetchall()
                return len(rows) == 1 and rows[0][0] == workspace_id and rows[0][1] is None and rows[0][2] > int(time.time())
            pair(browser, new_grant, f"http://127.0.0.1:{proxy.server_address[1]}", "re-registration", old_epoch, retry_preconditions, (old_device, old_stream))
            browser.js("window.__notryxSendTrace.paused=false")
            def registrations():
                return query(0, "SELECT id,workspace_id,workspace_epoch,device_id,device_stream_id,status FROM client_registrations ORDER BY id")
            new_registration = wait(lambda: (rows if len(rows := registrations()) == 2 and any(r[0] == old_id and r[5] == "frozen" for r in rows) and any(r[2] == old_epoch and r[5] == "active" for r in rows) else None), "same epoch re-registration")
            active = next(row for row in new_registration if row[5] == "active")
            require(active[1] == workspace_id and active[2] == old_epoch and active[3] != old_device and active[4] != old_stream, "new registration binding differs")
            wait(lambda: cached_node(0, node_id), "new registration bootstrap")

            peer_grant = admin("issue-pairing", principal)
            directories = client_directories[1]
            peer_environment = os.environ.copy()
            peer_environment.update(TAURI_WEBVIEW_AUTOMATION="true", NOTRYX_M0_DATA_DIR=str(directories["data"]),
                XDG_DATA_HOME=str(directories["xdg_data"]), XDG_CACHE_HOME=str(directories["xdg_cache"]), XDG_CONFIG_HOME=str(directories["xdg_config"]))
            peer = browser_helpers.Browser(perf, helpers, fixture, client_binary, args.driver, peer_environment)
            browsers.append(peer); peer.start()
            browser.link(peer)
            peer_window = focus_except(receiver_window)
            require(peer_window != receiver_window, "client windows are not independent")
            verify_focus(peer, browser)
            pair(peer, peer_grant, f"http://127.0.0.1:{proxy.server_address[1]}", "peer", old_epoch)
            require(device(1) not in (old_device, active[3]), "peer device must be independent")
            open_document(peer, node_id)
            require(editor_text(peer) == local_text, "peer did not read the first server version")
            peer_suffix = "\nSynthetic peer edit 한글\n"
            remote_text = local_text + peer_suffix
            remote_bytes = b"\xef\xbb\xbf" + remote_text.encode("utf-8")
            begin_edit(peer, peer_suffix)
            peer_draft = wait(lambda: (row if (row := checkpoint(1, node_id)) and row[1] == remote_bytes else None), "peer checkpoint")
            end_edit(peer)
            peer.js("document.querySelector('#save').click()")
            wait(lambda: peer_commit(node_id, peer_draft[0]), "peer confirmed server ACK")
            peer_checkpoint = checkpoint(1, node_id)
            require(peer_checkpoint and peer_checkpoint[1] == remote_bytes and peer_checkpoint[2] == remote_bytes,
                "peer checkpoint or confirmed base differs")
            peer.js("document.querySelector('[data-testid=close-document]').click()")
            wait(lambda: peer.js("return !document.querySelector('.cm-content[contenteditable=true]')"), "peer closes committed document")
            current = wait(lambda: (row if (row := server_document()) and row[2] == hashlib.sha256(remote_bytes).hexdigest() else None), "current peer server content")
            before_status = browser.js("""const done=arguments[arguments.length-1];
                window.__TAURI_INTERNALS__.invoke('m0_renderer_session')
                .then(owner=>window.__TAURI_INTERNALS__.invoke('workspace_sync_status',{request:{renderer_session_id:owner}}))
                .then(value=>done({focus:document.hasFocus(),status:value.status})).catch(()=>done({focus:document.hasFocus(),status:'unavailable'}));""", asynchronous=True)
            allowed_statuses = {'connecting', 'ready', 'offline', 'auth_required', 'gateway_auth_required', 'needs_review'}
            before_focus = {'cursor': cursor(0), 'revision_matches': bool((row := cached_node(0, node_id)) and row.get('content_rev') == current[1]),
                'focused': bool(before_status.get('focus')), 'sync_status': before_status.get('status') if before_status.get('status') in allowed_statuses else 'other'}
            require(not before_focus['focused'], "receiver was expected to be in the background")
            print(f"Synthetic receiver before foreground: {before_focus}", flush=True)
            focus_except(peer_window)
            verify_focus(browser, peer)
            wait_for_metadata(browser, 0, node_id, current[1], "receiver catches up with peer", [])
            require((notes / "Shared.md").read_bytes() == remote_bytes, "peer server bytes differ")
            draft = later_checkpoint
            draft_text = later_text
            draft_bytes = later_bytes

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
            old_registration_row = next(row for row in before_client[0] if row[0] == old_id)
            old_stream_row = next(row for row in before_client[3] if row[0] == old_id)
            old_operations = [row for row in before_client[1] if row[1] == old_id]
            original = next((row for row in old_operations if row[0] == sent[0]), None)
            old_draft = next((row for row in before_client[2] if row[0] == old_id and row[1] == node_id), None)
            later_operation = next((row for row in old_operations if row[0] == successor[0]), None)
            require(len(old_operations) == 2 and original and original[10] == "sent" and original[8] == first_local_bytes and original[6] == base_revision
                and original[9] == "sha256:" + hashlib.sha256(first_local_bytes).hexdigest(), "frozen sent request differs")
            require(later_operation and later_operation[10] == "queued" and later_operation[7] == sent[0] and later_operation[8] == later_bytes,
                "frozen successor differs")
            require(old_draft and old_draft[2:] == (draft[0], draft_bytes, base_revision, base), "frozen draft or base differs")
            require(not [row for row in before_client[1] if row[1] == active[0]], "new registration already has outbox operations")
            wait(lambda: browser.js("return [...document.querySelectorAll('button')].some(b=>b.textContent.trim()==='이전 본문 요청 대조' && !b.disabled)"), "frozen content review control")
            browser.js("[...document.querySelectorAll('button')].find(b=>b.textContent.trim()==='이전 본문 요청 대조').click()")
            wait(lambda: browser.js("return !!document.querySelector('[data-review-operation=\"'+arguments[0]+'\"]')", sent[0]), "frozen sent request listing")
            require(browser.js("return document.querySelector('[data-review-operation=\"'+arguments[0]+'\"]')?.textContent.includes('미확정')", sent[0]), "sent result was not shown as unknown")
            browser.js("document.querySelector('[data-review-operation=\"'+arguments[0]+'\"]').click()", sent[0])
            wait(lambda: browser.js("return !!document.querySelector('[data-testid=frozen-content-remote] textarea')"), "frozen source comparison")
            displayed = browser.js("""const section=document.querySelector('[aria-label="이전 본문 요청 대조"]');
                const value=id=>{const element=section.querySelector('[data-testid="'+id+'"] textarea'); return {value:element?.value, readOnly:element?.readOnly};};
                const choice=section.querySelector('[data-testid=frozen-content-select-dismiss]');
                const button=section.querySelector('[data-testid=frozen-content-dismiss]');
                return {payload:value('frozen-content-payload'),base:value('frozen-content-base'),draft:value('frozen-content-draft'),remote:value('frozen-content-remote'),
                  uncertain:section.textContent.includes('미확정'), choice:!!choice&&!choice.checked, disabled:!!button&&button.disabled};""")
            require(displayed["payload"] == {"value":local_text,"readOnly":True} and displayed["base"] == {"value":base_text,"readOnly":True}
                and displayed["draft"] == {"value":draft_text,"readOnly":True} and displayed["remote"] == {"value":remote_text,"readOnly":True}
                and displayed["uncertain"] and displayed["choice"] and displayed["disabled"], "comparison or explicit selection differs")
            def dismissals():
                return query(0, """SELECT old_operation_id,current_registration_id,operation_seq,operation_generation,operation_status,
                    abandon_state,expected_revision,dependency,payload_hash,payload,draft_generation,draft,base,base_revision
                    FROM frozen_content_dismissals ORDER BY old_operation_id""")
            require(client_snapshot() == before_client and server_snapshot() == before_server and not dismissals(),
                "inspection changed client or server state")
            with proxy.lock:
                role_gets_before = proxy.role_gets
                puts_before = proxy.put_requests
            require(puts_before >= 2, "PUT observation missed the original and peer requests")
            browser.js("document.querySelector('[data-testid=frozen-content-select-dismiss]').click()")
            require(browser.js("return !document.querySelector('[data-testid=frozen-content-dismiss]').disabled"), "explicit selection did not enable dismissal")
            require(client_snapshot() == before_client and server_snapshot() == before_server and not dismissals(),
                "selection alone changed data")
            with proxy.lock:
                require(proxy.put_requests == puts_before, "selection sent a content PUT")
            browser.js("document.querySelector('[data-testid=frozen-content-dismiss]').click()")
            saved = wait(lambda: (rows[0] if len(rows := dismissals()) == 1 else None), "recorded dismissal")
            wait(lambda: browser.js("return document.querySelector('[aria-label=\"이전 본문 요청 대조\"] [role=status]')?.textContent.includes('재발급 포기')"), "dismissal confirmation")
            with proxy.lock:
                require(proxy.role_gets > role_gets_before, "current role was not checked during dismissal")
            require(saved == (sent[0], active[0], sent[1], sent[2], "sent", "none", base_revision, None,
                "sha256:" + hashlib.sha256(first_local_bytes).hexdigest(), first_local_bytes,
                later_checkpoint[0], later_bytes, base, base_revision), "recorded dismissal differs from confirmed sources")
            after_client = client_snapshot()
            require(after_client == before_client and server_snapshot() == before_server,
                "dismissal changed registrations, operations, drafts, streams or server state")
            require((notes / "Shared.md").read_bytes() == remote_bytes and (notes / "Protected.md").read_bytes() == protected,
                "dismissal changed server files")
            require(query(0, "SELECT old_operation_id FROM frozen_content_reconciliations") == [], "dismissal recorded an apply decision")
            deadline = time.monotonic() + 2
            while time.monotonic() < deadline:
                require(browser.js("return !!document.querySelector('[aria-label=\"이전 본문 요청 대조\"]')"), "receiver renderer stopped")
                with proxy.lock:
                    require(proxy.put_requests == puts_before, "dismissal sent a content PUT")
                require(client_snapshot() == before_client and server_snapshot() == before_server and dismissals() == [saved],
                    "dismissal changed preserved state during observation")
                time.sleep(0.1)

            require(not proxy.failure.is_set(), "proxy failed before the planned offline restart")
            # Stop only processes launched by this fixture and reopen the same local SQLite without a secret service.
            peer.close(); browsers.remove(peer)
            proxy.failure.mark("planned_outage")
            stop_server()
            os.close(server_fd); server_fd = None; server_identity = None; server = None
            browser.close(); browsers.remove(browser)
            require(client_snapshot() == before_client and dismissals() == [saved], "closing client changed recovery sources")
            browser = browser_helpers.Browser(perf, helpers, fixture, client_binary, args.driver, environment)
            browsers.append(browser); browser.start()
            wait(lambda: browser.js("return document.body.textContent.includes('서버에 연결되지 않음')"), "offline client restart")
            status_trace = browser.js("""const original=window.fetch, target=window.__TAURI_INTERNALS__.convertFileSrc('workspace_inspect_frozen_content','ipc');
                const state={inspections:0}; window.__dismissRestartTrace=state;
                window.fetch=function(input,init){const url=typeof input==='string'?input:input.url;
                  if(url===target) state.inspections++; return original.call(this,input,init);}; return true;""")
            require(status_trace, "restart inspection observation unavailable")
            wait(lambda: browser.js("return [...document.querySelectorAll('button')].some(b=>b.textContent.trim()==='이전 본문 요청 대조' && !b.disabled)"), "offline frozen review control")
            browser.js("[...document.querySelectorAll('button')].find(b=>b.textContent.trim()==='이전 본문 요청 대조').click()")
            wait(lambda: browser.js("return !!document.querySelector('[data-review-operation=\"'+arguments[0]+'\"]')", sent[0]), "offline original request listing")
            require(browser.js("return !!document.querySelector('[data-review-operation=\"'+arguments[0]+'\"]')", successor[0]), "offline successor listing missing")
            browser.js("document.querySelector('[data-review-operation=\"'+arguments[0]+'\"]').click()", sent[0])
            wait(lambda: browser.js("return !!document.querySelector('[data-testid=frozen-content-recorded-dismissal]')"), "offline recorded dismissal")
            recorded = browser.js("""const detail=document.querySelector('[data-testid=frozen-content-recorded-dismissal]');
                return {id:detail?.textContent.includes(arguments[0]), unknown:detail?.textContent.includes('미확정'),
                  preserved:detail?.textContent.includes('초안')&&detail?.textContent.includes('다른 작업'),
                  inspections:window.__dismissRestartTrace?.inspections};""", sent[0])
            require(recorded == {"id":True,"unknown":True,"preserved":True,"inspections":0}, "offline recorded dismissal differs")
            require(client_snapshot() == before_client and dismissals() == [saved] and server_snapshot() == before_server,
                "offline restart changed preserved client or server data")
            with proxy.lock:
                require(proxy.put_requests == puts_before, "offline restart sent a content PUT")
            require((notes / "Shared.md").read_bytes() == remote_bytes and (notes / "Protected.md").read_bytes() == protected,
                "offline restart changed server files")
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
    print("M3 frozen dismissal WebView passed: explicit selection, no new PUT, preserved sources and offline application restart")

if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        print(f"M3 frozen dismissal WebView failed: {error}", file=sys.stderr)
        raise SystemExit(1)
