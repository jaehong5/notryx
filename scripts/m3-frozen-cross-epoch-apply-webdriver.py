#!/usr/bin/env python3
"""Verify explicit cross-epoch frozen content recovery after synthetic cold restore."""
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
import urllib.error
import urllib.request


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
    parser.add_argument("--two-clients", action="store_true", help="verify a second existing client and export both preserved drafts after restore")
    args = parser.parse_args()
    repo = Path(args.repo).resolve()
    helpers = module(repo / "scripts/m2-workspace-webdriver.py", "m2_helpers")
    perf = module(repo / "scripts/m3-search-perf-webdriver.py", "perf_helpers")
    browser_helpers = module(repo / "scripts/m3-two-client-browser-helpers.py", "two_client_browser_helpers")

    class DiagnosticBrowser(browser_helpers.Browser):
        allowed_webdriver_errors = {"javascript error", "no such window", "invalid session id", "script timeout", "unknown error",
            "timeout", "no such element", "stale element reference", "unsupported operation", "unexpected alert open", "invalid argument"}

        def __init__(self, *args, **kwargs):
            super().__init__(*args, **kwargs)
            self.last_webdriver_code = "not_observed"

        def record_code(self, value):
            code = value.get("error") if isinstance(value, dict) else None
            self.last_webdriver_code = code if type(code) is str and code in self.allowed_webdriver_errors else "other"

        def wd(self, method, path, payload=None):
            self.check()
            data = None if payload is None else json.dumps(payload).encode()
            request = urllib.request.Request(self.origin + path, data=data, method=method,
                headers={"Content-Type": "application/json"})
            try:
                with self.opener.open(request, timeout=35) as response:
                    result = json.load(response)["value"]
                if isinstance(result, dict) and "error" in result:
                    self.record_code(result)
                    raise RuntimeError("WebDriver returned an error")
                self.check()
                self.last_webdriver_code = "none"
                return result
            except urllib.error.HTTPError as error:
                self.last_webdriver_code = "http_error"
                try:
                    body = error.read(16385)
                    if len(body) <= 16384:
                        envelope = json.loads(body)
                        self.record_code(envelope.get("value") if isinstance(envelope, dict) else None)
                except Exception:
                    self.last_webdriver_code = "http_error"
                raise RuntimeError("WebDriver request failed") from None
            except (OSError, ValueError, KeyError):
                self.last_webdriver_code = "transport_or_decode_error"
                raise RuntimeError("WebDriver request failed") from None

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
    fixture = Path(tempfile.mkdtemp(prefix="notryx-m3-frozen-cross-epoch-"))
    client_binary = str((repo / args.binary).resolve())
    server_binary = str((repo / args.server).resolve())
    browsers = []
    server = None
    server_fd = None
    server_identity = None
    proxy = None
    cleanup_safe = False
    previous_term = signal.getsignal(signal.SIGTERM)
    test_stage = "setup"

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

    def settle_disk():
        if args.two_clients:
            wait(lambda: (pressure := perf.io_psi_some_avg10()) is not None and pressure < 10,
                "disk pressure between cold backup steps")

    try:
        notes, state, run = (fixture / name for name in ("notes", "state", "run"))
        for path in (notes, state, run):
            path.mkdir(mode=0o700)
        client_directories = []
        for name in (("receiver", "peer") if args.two_clients else ("receiver",)):
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
        base_text = base.decode("utf-8-sig").replace("\r\n", "\n")
        local_suffix = "\nLocal edit 한글\n"
        local_text = base_text + local_suffix
        local_bytes = b"\xef\xbb\xbf" + local_text.encode("utf-8")
        first_local_bytes = local_bytes

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




        def focus_client(client):
            if args.two_clients:
                other = next((item.x11_window for item in browsers if item is not client and hasattr(item, "x11_window")), None)
                focus = helpers.capture_remote_change_focus(exclude_window=other)
                client.x11_window = focus[2]
                focus[0].XCloseDisplay(focus[1])
                wait(lambda: client.js("return document.hasFocus()"), "active client window")

        def pair(browser, secret, origin, phase, expected_epoch=None, retry_preconditions=None, previous_identity=None):
            focus_client(browser)
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
                  alertCodes:[...document.querySelectorAll('[role=alert]')].map(e=>(e.textContent||'').trim().replace(/^Error: /,''))
                    .filter(value=>/^[A-Za-z_]{1,80}$/.test(value)).slice(0,5),
                  focused:document.hasFocus(),
                  timeline:window.__notryxPairTrace?.events || [], attempts:window.__notryxPairTrace?.attempts || [], send:window.__notryxSendTrace || null};""")
                except Exception:
                    ui = {"diagnostic":"unavailable"}
                with proxy.lock:
                    pair_calls = proxy.route_counts.get("pair", 0)
                    pair_inflight = proxy.inflight.get("pair", 0)
                    connections = len(proxy.connections)
                    pair_responses = {key:count for key,count in proxy.response_counts.items() if key.startswith("pair:")}
                def count(value):
                    return value if type(value) is int and 0 <= value <= 1000000 else None
                allowed_steps = {"form", "submit", "retry_submit", "pair_ipc", "sync"}
                allowed_codes = {"transmission_in_progress", "open_edit_sessions", "pairing_in_progress",
                    "pairing_pending_adoption", "pairing_result_unknown", "stale_renderer_session",
                    "stale_registration", "auth_required", "gateway_auth_required", "other_alert"}
                allowed_attempts = {"pending", "ok", "transmission_in_progress", "other", "unknown"}
                allowed_sync = {"connecting", "ready", "offline", "auth_required", "gateway_auth_required", "needs_review", "invalid", "unavailable"}
                flags = {key:ui.get(key) if type(ui.get(key)) is bool else None for key in
                    ("form", "formDisabled", "secretCleared", "selectedDocument", "editor", "editable", "alert", "compositionAlert", "sessionAlert", "queueAlert", "pairingAlert")}
                events = ui.get("timeline") if isinstance(ui.get("timeline"), list) else []
                timeline = [{"code":event.get("code") if type(event.get("code")) is str and event.get("code") in allowed_codes else "invalid", "ms":count(event.get("ms"))}
                    for event in events[:20] if isinstance(event, dict)]
                attempts = ui.get("attempts") if isinstance(ui.get("attempts"), list) else []
                attempt_statuses = [item.get("status") if type(item.get("status")) is str and item.get("status") in allowed_attempts else "invalid"
                    for item in attempts[:5] if isinstance(item, dict)]
                send = ui.get("send") if isinstance(ui.get("send"), dict) else {}
                send_counts = {key:count(send.get(key)) for key in ("started", "finished", "inflight", "suppressed")}
                send_counts["paused"] = send.get("paused") if type(send.get("paused")) is bool else None
                responses = {key:count(value) for key,value in pair_responses.items() if re.fullmatch(r"pair:[1-5][0-9]{2}", key)}
                error_type = type(cause).__name__ if type(cause).__name__ in {"TestFailure", "RuntimeError", "OSError"} else "other"
                diagnostic = {"stage":step if step in allowed_steps else "other", "error_type":error_type,
                    "requests_before":count(calls_before), "requests_after":count(pair_calls), "pair_inflight":count(pair_inflight),
                    "proxy_connections":count(connections), "responses":responses, "proxy_failed":bool(proxy.failure.is_set()),
                    "sync":last_sync["status"] if type(last_sync["status"]) is str and last_sync["status"] in allowed_sync else "other",
                    "epoch_matches":bool(last_sync["epoch_matches"]), "workspace_matches":bool(last_sync["workspace_matches"]),
                    "origin_matches":bool(last_sync["origin_matches"]), "identity_changed":bool(last_sync["identity_changed"]),
                    "ui":flags, "focused":ui.get("focused"), "alert_codes":ui.get("alertCodes", []),
                    "timeline":timeline, "attempts":attempt_statuses, "send":send_counts}
                print(f"Synthetic pairing diagnostic: {diagnostic}", file=sys.stderr, flush=True)
                raise TestFailure("synthetic pairing failed") from cause
            finally:
                try:
                    browser.js("""const trace=window.__notryxPairTrace;
                      trace?.observer?.disconnect();
                      if(trace && window.fetch===trace.wrapped) window.fetch=trace.originalFetch;""")
                except Exception:
                    pass

        def observe_send(browser):
            installed = browser.js("""const original=window.fetch, target=window.__TAURI_INTERNALS__.convertFileSrc('workspace_send_next','ipc');
              const state={started:0,finished:0,inflight:0,paused:true,suppressed:0}; window.__notryxSendTrace=state;
              const wrapped=function(input,init){const url=typeof input==='string'?input:input.url;
                if(url!==target) return original.call(this,input,init);
                if(state.paused && state.finished>0){state.suppressed++; return Promise.resolve(new Response(JSON.stringify('synthetic send paused'),{status:200,headers:{'Tauri-Response':'error','Content-Type':'application/json'}}));}
                state.started++; state.inflight++;
                return original.call(this,input,init).finally(()=>{state.inflight--; state.finished++;});};
              window.fetch=wrapped; return window.fetch===wrapped;""")
            require(installed, "send completion observation could not be installed")

        def open_document(browser, node_id):
            focus_client(browser)
            wait(lambda: browser.js("return !!document.querySelector('[data-document=\"'+arguments[0]+'\"]:not(:disabled)')", node_id), "document row")
            browser.js("document.querySelector('[data-document=\"'+arguments[0]+'\"]').click()", node_id)
            wait(lambda: browser.js("return document.querySelector('[aria-current=page]')?.dataset.document===arguments[0] && !!document.querySelector('.cm-content[contenteditable=true]')", node_id), "opened editor")

        def begin_edit(browser, suffix):
            focus_client(browser)
            browser.js("""const element=document.querySelector('.cm-content');
                element.dispatchEvent(new CompositionEvent('compositionstart',{bubbles:true}));
                const view=element.cmTile.root.view;
                view.dispatch({changes:{from:view.state.doc.length,insert:arguments[0]},userEvent:'input.type'});""", suffix)

        def end_edit(browser):
            browser.js("""const done=arguments[arguments.length-1];
                document.querySelector('.cm-content').dispatchEvent(new CompositionEvent('compositionend',{bubbles:true}));
                setTimeout(done,400);""", asynchronous=True)
            wait(lambda: browser.js("return !document.querySelector('.cm-content').cmTile.root.view.composing"), "composition end")

        def preserved_client(index, registration):
            return (
                query(index, "SELECT * FROM client_outbox WHERE registration_id=? ORDER BY operation_seq", (registration,)),
                query(index, "SELECT c.* FROM draft_checkpoints c JOIN registered_documents d ON d.storage_id=c.document_id WHERE d.registration_id=? ORDER BY c.document_id", (registration,)),
                query(index, "SELECT b.* FROM draft_bases b JOIN registered_documents d ON d.storage_id=b.document_id WHERE d.registration_id=? ORDER BY b.document_id", (registration,)),
                query(index, "SELECT * FROM outbox_streams WHERE registration_id=?", (registration,)),
            )

        def export_frozen_draft(client, document_id, expected, name):
            focus_client(client)
            wait(lambda: client.js("return !!document.querySelector('[data-draft-document=\"'+arguments[0]+'\"]:not(:disabled)')", document_id), "preserved draft row")
            client.js("document.querySelector('[data-draft-document=\"'+arguments[0]+'\"]').click()", document_id)
            wait(lambda: client.js("return !!document.querySelector('#export-source:not(:disabled)') && !!document.querySelector('.cm-content') && !!document.querySelector('[data-draft-document=\"'+arguments[0]+'\"][aria-current=page]')", document_id), "selected preserved draft")
            destination = fixture / name
            failures = []
            def interact():
                try:
                    helpers.respond_to_recovery_dialog(destination)
                except Exception:
                    failures.append(True)
            worker = threading.Thread(target=interact, daemon=True)
            worker.start()
            try:
                client.js("document.querySelector('#export-source').click()")
            finally:
                worker.join(timeout=20)
            require(not worker.is_alive() and not failures, "preserved draft export dialog failed")
            wait(lambda: destination.exists(), "preserved draft export")
            wait(lambda: client.js("return !document.querySelector('#export-source').disabled"), "export completion")
            require(destination.read_bytes() == expected, "exported preserved draft bytes differ")
            client.js("document.querySelector('[data-testid=close-document]').click()")
            wait(lambda: client.js("return !document.querySelector('.cm-content')"), "exported draft closed")


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
                def forward_upstream(self, upstream, request_line, headers, length, method, path, route):
                    if method == "GET" and path == "/v1/devices/self/role":
                        with self.server.lock:
                            self.server.role_gets += 1
                    if method == "PUT" and path == self.server.document_path:
                        expected = self.server.expected_put
                        if expected is not None:
                            wanted = {b"if-match", b"x-workspace-epoch", b"x-device-stream", b"x-operation-seq", b"idempotency-key"}
                            observed = {}
                            for line in headers:
                                key, separator, value = line.partition(b":")
                                key = key.strip().lower()
                                if separator and key in wanted:
                                    observed.setdefault(key, []).append(value.strip())
                            matches = all(observed.get(key) == [value] for key, value in expected.items())
                            with self.server.lock:
                                self.server.checked_put_headers.append(matches)
                    return super().forward_upstream(upstream, request_line, headers, length, method, path, route)
            proxy.RequestHandlerClass = RoleObservingHandler
            proxy.role_gets = 0
            proxy.put_requests = 0
            proxy.expected_put = None
            proxy.checked_put_headers = []
            proxy.failure = PhaseFailures()
            proxy.start()
            principal = admin("status")["owner_principal_id"]
            grant = admin("issue-pairing", principal)
            directories = client_directories[0]
            environment = os.environ.copy()
            environment.update(TAURI_WEBVIEW_AUTOMATION="true", NOTRYX_M0_DATA_DIR=str(directories["data"]),
                XDG_DATA_HOME=str(directories["xdg_data"]), XDG_CACHE_HOME=str(directories["xdg_cache"]), XDG_CONFIG_HOME=str(directories["xdg_config"]))
            browser = DiagnosticBrowser(perf, helpers, fixture, client_binary, args.driver, environment)
            browsers.append(browser); browser.start()
            test_stage = "initial_pair"
            pair(browser, grant, f"http://127.0.0.1:{proxy.server_address[1]}", "initial")
            old_registration = query(0, "SELECT id,workspace_id,workspace_epoch,device_id,device_stream_id,status FROM client_registrations WHERE status='active'")
            require(len(old_registration) == 1, "old registration unavailable")
            old_id, workspace_id, old_epoch, old_device, old_stream, _ = old_registration[0]
            old_node = wait(lambda: cached_node(0, node_id), "old registration metadata")
            require(old_node.get("node_id") == node_id and old_node.get("name") == "Shared.md" and old_node.get("parent_id"), "old synthetic node identity differs")
            if args.two_clients:
                test_stage = "peer_initial_pair"
                directories = client_directories[1]
                peer_environment = os.environ.copy()
                peer_environment.update(TAURI_WEBVIEW_AUTOMATION="true", NOTRYX_M0_DATA_DIR=str(directories["data"]),
                    XDG_DATA_HOME=str(directories["xdg_data"]), XDG_CACHE_HOME=str(directories["xdg_cache"]), XDG_CONFIG_HOME=str(directories["xdg_config"]))
                peer = DiagnosticBrowser(perf, helpers, fixture, client_binary, args.driver, peer_environment)
                browsers.append(peer); peer.start(); browser.link(peer)
                pair(peer, admin("issue-pairing", principal), f"http://127.0.0.1:{proxy.server_address[1]}", "peer initial")
                peer_registration = query(1, "SELECT id,device_id,device_stream_id FROM client_registrations WHERE status='active'")[0]
                require(peer_registration[1] != old_device and peer_registration[2] != old_stream, "clients share a device or stream")
                with sqlite3.connect(f"file:{server_database}?mode=ro", uri=True, timeout=5) as connection:
                    peer_node_id = connection.execute("SELECT node_id FROM nodes WHERE name='Protected.md' AND deleted=0").fetchone()[0]
                wait(lambda: cached_node(1, peer_node_id), "peer initial metadata")
                open_document(peer, peer_node_id)
            open_document(browser, node_id)
            test_stage = "original_send"
            begin_edit(browser, local_suffix)
            sent_checkpoint = wait(lambda: (row if (row := checkpoint(0, node_id)) and row[1] == first_local_bytes else None), "sent draft checkpoint")
            require(sent_checkpoint[2:] == (base, base_revision) and not outbox(0, node_id), "sent draft base differs")
            proxy.hold_original_success.set(); proxy.discard_original_success.set(); proxy.hold_uncertain_lookup.set()
            observe_send(browser)
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
            with proxy.lock:
                puts_after_lost_response = proxy.put_requests
            require(puts_after_lost_response == 1, "PUT observation did not record exactly the original request")
            proxy.hold_original_success.clear()
            def original_server_history():
                with sqlite3.connect(f"file:{server_database}?mode=ro", uri=True, timeout=5) as connection:
                    connection.execute("BEGIN")
                    receipt = connection.execute("SELECT * FROM operation_receipts WHERE operation_id=?", (sent[0],)).fetchall()
                    retired = connection.execute("SELECT * FROM retired_operations WHERE operation_id=?", (sent[0],)).fetchall()
                    stream = connection.execute("SELECT * FROM operation_streams WHERE stream_id=?", (old_stream,)).fetchall()
                return receipt, retired, stream
            old_server_history = original_server_history()
            require(len(old_server_history[0]) == 1 and old_server_history[0][0][:3] == (sent[0], old_stream, str(sent[1]))
                and not old_server_history[1] and len(old_server_history[2]) == 1, "old server receipt or stream was not observed")
            browser.js("document.querySelector('[data-testid=close-document]').click()")
            wait(lambda: browser.js("return !document.querySelector('.cm-content[contenteditable=true]')"), "receiver closes the saved document")
            require(len(outbox(0, node_id)) == 1 and outbox(0, node_id)[0] == sent and checkpoint(0, node_id) == sent_checkpoint,
                "closing changed the unconfirmed operation or draft")
            require(not proxy.failure.is_set(), "proxy failed before planned restore outage")
            proxy.failure.mark("planned_outage")
            stop_server(); os.close(server_fd)
            server = None; server_fd = None; server_identity = None
            if args.two_clients:
                test_stage = "peer_offline_edit"
                observe_send(peer)
                peer_suffix = "\nPeer unsent draft 한글\n"
                peer_bytes = protected + peer_suffix.encode("utf-8")
                begin_edit(peer, peer_suffix)
                wait(lambda: (row := checkpoint(1, peer_node_id)) and row[1] == peer_bytes, "peer offline checkpoint")
                end_edit(peer); peer.js("document.querySelector('#save').click()")
                wait(lambda: len(outbox(1, peer_node_id)) == 1, "peer offline outbox")
                wait(lambda: peer.js("return window.__notryxSendTrace.finished>0 && window.__notryxSendTrace.inflight===0"), "peer offline send settled")
                require(checkpoint(1, peer_node_id)[2] == protected, "peer base changed while offline")
                peer.js("document.querySelector('[data-testid=close-document]').click()")
                wait(lambda: peer.js("return !document.querySelector('.cm-content')"), "peer closes document")
                peer_before_restore = preserved_client(1, peer_registration[0])
                require(len(peer_before_restore[0]) == 1 and (notes / "Protected.md").read_bytes() == protected, "peer edit reached stopped server")
            test_stage = "cold_backup_restore"
            backup, restored = fixture / "cold-backup", fixture / "restored"
            require(not backup.exists() and not restored.exists(), "synthetic backup and restore destinations must be new")
            settle_disk()
            cli("backup", "--config", str(config), str(backup))
            settle_disk()
            cli("backup-verify", str(backup))
            settle_disk()
            cli("restore", str(backup), str(restored))
            settle_disk()
            notes, state = restored / "notes", restored / "state"
            server_database = state / "state.db"
            config = fixture / "restored-live.toml"
            config.write_text(f'[server]\nlisten = "127.0.0.1:{port}"\nadmin_socket = "{run}/admin.sock"\n[workspace]\nnotes_root = "{notes}"\nstate_root = "{state}"\n', encoding="utf-8")
            config.chmod(0o600)
            server, server_fd, server_identity = start_isolated_server()
            proxy.failure.mark("after_restore")
            status = admin("status")
            require(status["workspace_id"] == workspace_id and status["workspace_epoch"] != old_epoch, "restored workspace or epoch differs")
            current_epoch = status["workspace_epoch"]
            restored_node = wait(server_document, "restored synthetic node")
            require(restored_node[0] == node_id and (notes / "Shared.md").read_bytes() == first_local_bytes,
                "restored synthetic node ID or bytes differ")
            require(original_server_history() == old_server_history, "cold backup and restore changed old receipt or stream")
            remote_text = "# Synthetic restored server\nIndependent current content\n"
            remote_bytes = remote_text.encode("utf-8")
            (notes / "Shared.md").write_bytes(remote_bytes)
            current = wait(lambda: (row if (row := server_document()) and row[0] == node_id and row[2] == hashlib.sha256(remote_bytes).hexdigest() else None),
                "restored current server content")
            new_grant = admin("issue-pairing", status["owner_principal_id"])
            settle_disk()
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
            test_stage = "new_epoch_pair"
            pair(browser, new_grant, f"http://127.0.0.1:{proxy.server_address[1]}", "re-registration", current_epoch, retry_preconditions, (old_device, old_stream))
            test_stage = "new_registration_check"
            browser.js("window.__notryxSendTrace.paused=false")
            def registrations():
                return query(0, "SELECT id,workspace_id,workspace_epoch,device_id,device_stream_id,status FROM client_registrations ORDER BY id")
            new_registration = wait(lambda: (rows if len(rows := registrations()) == 2 and any(r[0] == old_id and r[5] == "frozen" for r in rows) and any(r[2] == current_epoch and r[5] == "active" for r in rows) else None), "new epoch re-registration")
            active = next(row for row in new_registration if row[5] == "active")
            require(active[1] == workspace_id and active[2] == current_epoch and active[3] != old_device and active[4] != old_stream, "new registration binding differs")
            test_stage = "restored_index_check"
            new_node = wait(lambda: (row if (row := cached_node(0, node_id)) and row.get("content_rev") == current[1] else None), "new registration bootstrap")
            require(new_node.get("node_id") == node_id and new_node.get("parent_id") == old_node.get("parent_id")
                and new_node.get("name") == "Shared.md", "restored target metadata differs")
            if args.two_clients:
                test_stage = "peer_restored_pair"
                settle_disk()
                require(preserved_client(1, peer_registration[0]) == peer_before_restore, "restore changed peer drafts or outbox")
                pair(peer, admin("issue-pairing", principal), f"http://127.0.0.1:{proxy.server_address[1]}", "peer restored", current_epoch,
                    previous_identity=peer_registration[1:])
                peer.js("window.__notryxSendTrace.paused=false")
                require(query(1, "SELECT status FROM client_registrations WHERE id=?", (peer_registration[0],)) == [("frozen",)], "peer old registration was not frozen")
                require(preserved_client(1, peer_registration[0]) == peer_before_restore, "peer re-pair changed old drafts or outbox")
                with proxy.lock:
                    puts_before_exports = proxy.put_requests
                primary_before_exports = preserved_client(0, old_id)
                test_stage = "two_client_draft_exports"
                for client, document, expected, name in ((browser, node_id, first_local_bytes, "receiver-export.md"),
                        (peer, peer_node_id, peer_bytes, "peer-export.md")):
                    export_frozen_draft(client, document, expected, name)
                    idle = client.js("""const done=arguments[arguments.length-1];
                        window.__TAURI_INTERNALS__.invoke('m0_renderer_session')
                          .then(owner=>window.__TAURI_INTERNALS__.invoke('workspace_send_next',{request:{renderer_session_id:owner}}))
                          .then(value=>done(value.status==='idle')).catch(()=>done(false));""", asynchronous=True)
                    require(idle, "restored client tried to replay an old request")
                require(preserved_client(0, old_id) == primary_before_exports and preserved_client(1, peer_registration[0]) == peer_before_restore,
                    "export or send changed preserved drafts, bases or outbox")
                with proxy.lock:
                    require(proxy.put_requests == puts_before_exports, "restored clients automatically submitted an old PUT")
                print("AT-57 two-client checkpoint: cold restore, re-pair, byte-exact draft exports and no automatic old PUT passed.", flush=True)
                focus_client(browser)
            draft = sent_checkpoint
            draft_text = local_text
            draft_bytes = first_local_bytes

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

            def complete_old_snapshot():
                db = client_directories[0]["data"] / "drafts.sqlite3"
                with sqlite3.connect(f"file:{db}?mode=ro", uri=True, timeout=5) as connection:
                    connection.execute("BEGIN")
                    registration = connection.execute("SELECT * FROM client_registrations WHERE id=?", (old_id,)).fetchall()
                    operations = connection.execute("SELECT * FROM client_outbox WHERE registration_id=? ORDER BY operation_seq", (old_id,)).fetchall()
                    documents = connection.execute("SELECT * FROM registered_documents WHERE registration_id=? ORDER BY document_id", (old_id,)).fetchall()
                    drafts = connection.execute("SELECT c.* FROM draft_checkpoints c JOIN registered_documents d ON d.storage_id=c.document_id WHERE d.registration_id=? ORDER BY c.document_id", (old_id,)).fetchall()
                    bases = connection.execute("SELECT b.* FROM draft_bases b JOIN registered_documents d ON d.storage_id=b.document_id WHERE d.registration_id=? ORDER BY b.document_id", (old_id,)).fetchall()
                    streams = connection.execute("SELECT * FROM outbox_streams WHERE registration_id=?", (old_id,)).fetchall()
                return registration, operations, documents, drafts, bases, streams

            test_stage = "old_data_check"
            before_client = client_snapshot(); before_server = server_snapshot(); before_old = complete_old_snapshot()
            with proxy.lock:
                require(proxy.put_requests == puts_after_lost_response, "a PUT was sent before the comparison")
            old_registration_row = next(row for row in before_client[0] if row[0] == old_id)
            old_stream_row = next(row for row in before_client[3] if row[0] == old_id)
            old_operations = [row for row in before_client[1] if row[1] == old_id]
            original = next((row for row in old_operations if row[0] == sent[0]), None)
            old_draft = next((row for row in before_client[2] if row[0] == old_id and row[1] == node_id), None)
            require(len(old_operations) == 1 and original and original[10] == "sent" and original[8] == first_local_bytes and original[6] == base_revision
                and original[9] == "sha256:" + hashlib.sha256(first_local_bytes).hexdigest(), "frozen sent request differs")
            require(old_draft and old_draft[2:] == (draft[0], draft_bytes, base_revision, base), "frozen draft or base differs")
            require(old_stream_row[1:] == (sent[10], sent[11]), "old acknowledgement watermarks changed")
            require(not [row for row in before_client[1] if row[1] == active[0]], "new registration already has outbox operations")
            test_stage = "review_control_wait"
            wait(lambda: browser.js("return [...document.querySelectorAll('button')].some(b=>b.textContent.trim()==='이전 본문 요청 대조' && !b.disabled)"), "frozen content review control")
            test_stage = "review_control_click"
            browser.js("[...document.querySelectorAll('button')].find(b=>b.textContent.trim()==='이전 본문 요청 대조').click()")
            test_stage = "review_listing_wait"
            wait(lambda: browser.js("return !!document.querySelector('[data-review-operation=\"'+arguments[0]+'\"]')", sent[0]), "frozen sent request listing")
            test_stage = "review_item_status"
            require(browser.js("return document.querySelector('[data-review-operation=\"'+arguments[0]+'\"]')?.textContent.includes('미확정')", sent[0]), "sent result was not shown as unknown")
            test_stage = "review_item_click"
            browser.js("document.querySelector('[data-review-operation=\"'+arguments[0]+'\"]').click()", sent[0])
            test_stage = "review_remote_wait"
            wait(lambda: browser.js("return !!document.querySelector('[data-testid=frozen-content-remote] textarea')"), "frozen source comparison")
            test_stage = "review_capture"
            displayed = browser.js("""const section=document.querySelector('[aria-label="이전 본문 요청 대조"]');
                const value=id=>{const element=section.querySelector('[data-testid="'+id+'"] textarea'); return {value:element?.value, readOnly:element?.readOnly};};
                const choice=section.querySelector('[data-testid=frozen-content-select-draft]');
                const target=section.querySelector('[data-testid=frozen-content-confirm-target]');
                const button=section.querySelector('[data-testid=frozen-content-apply]');
                return {payload:value('frozen-content-payload'),base:value('frozen-content-base'),draft:value('frozen-content-draft'),remote:value('frozen-content-remote'),
                  uncertain:section.textContent.includes('미확정'),epochs:section.textContent.includes(arguments[0])&&section.textContent.includes(arguments[1]),
                  identityWarning:section.textContent.includes('ID가 같아도')&&section.textContent.includes('관계'),
                  target:section.textContent.includes(`현재 적용 대상: /Shared.md (node ID ${arguments[2]}, 상위 node ID ${arguments[3]}, 이름 Shared.md)`),
                  choice:!!choice&&!choice.checked,targetChoice:!!target&&!target.checked,disabled:!!button&&button.disabled};""", old_epoch, current_epoch, node_id, new_node["parent_id"])
            review_flags = {"payload":displayed["payload"] == {"value":local_text,"readOnly":True},
                "base":displayed["base"] == {"value":base_text,"readOnly":True}, "draft":displayed["draft"] == {"value":draft_text,"readOnly":True},
                "remote":displayed["remote"] == {"value":remote_text,"readOnly":True},
                **{key:bool(displayed[key]) for key in ("uncertain", "epochs", "identityWarning", "target", "choice", "targetChoice", "disabled")}}
            if not all(review_flags.values()):
                print(f"Synthetic cross-epoch review conditions: {review_flags}", file=sys.stderr, flush=True)
            require(all(review_flags.values()), "cross-epoch comparison or explicit target differs")
            require(client_snapshot() == before_client and server_snapshot() == before_server, "inspection changed client or server state")
            with proxy.lock:
                role_gets_before = proxy.role_gets
                puts_before = proxy.put_requests
            require(puts_before == puts_after_lost_response, "a PUT was sent before explicit selection")
            test_stage = "explicit_selection"
            browser.js("document.querySelector('[data-testid=frozen-content-select-draft]').click()")
            require(browser.js("return document.querySelector('[data-testid=frozen-content-apply]').disabled"), "draft choice enabled approval before target confirmation")
            require(client_snapshot() == before_client and server_snapshot() == before_server, "draft choice changed state")
            with proxy.lock:
                require(proxy.put_requests == puts_before, "draft choice sent a PUT")
            browser.js("document.querySelector('[data-testid=frozen-content-confirm-target]').click()")
            require(browser.js("return !document.querySelector('[data-testid=frozen-content-apply]').disabled"), "target confirmation did not enable approval")
            require(client_snapshot() == before_client and server_snapshot() == before_server, "target confirmation changed state")
            with proxy.lock:
                require(proxy.put_requests == puts_before, "target confirmation sent a PUT")
            test_stage = "approved_submission"
            proxy.hold_original_request.set()
            browser.js("document.querySelector('[data-testid=frozen-content-apply]').click()")
            wait(proxy.original_request_held.is_set, "approved conditional PUT held before server")
            decisions = wait(lambda: (rows if len(rows := query(0, "SELECT old_operation_id,current_registration_id,new_operation_id,new_operation_seq,generation,draft_bytes,base_bytes,base_revision,new_expected_revision,new_payload_hash FROM frozen_content_reconciliations WHERE old_operation_id=?", (sent[0],))) == 1 else None), "recorded recovery decision")
            decision = decisions[0]
            with proxy.lock:
                require(proxy.role_gets > role_gets_before, "current role was not checked during approval")
            new_id, new_seq = decision[2], decision[3]
            require(decision[:2] == (sent[0], active[0]) and decision[4:9] == (draft[0], draft_bytes, base, base_revision, current[1])
                and decision[9] == "sha256:" + hashlib.sha256(draft_bytes).hexdigest(), "recorded approval differs from displayed versions")
            created = operation(new_id)
            require(created and created[1] == new_seq and created[2:6] == ("content", node_id, current[1], draft_bytes)
                and created[6] in ("queued", "sent"), "approved new operation differs")
            midway = client_snapshot()
            require(complete_old_snapshot() == before_old and next(row for row in midway[0] if row[0] == old_id) == old_registration_row
                and [row for row in midway[1] if row[1] == old_id] == old_operations
                and next(row for row in midway[2] if row[0] == old_id and row[1] == node_id) == old_draft
                and next(row for row in midway[3] if row[0] == old_id) == old_stream_row,
                "old registration, operation, draft or stream changed during approval")
            require(checkpoint(0, node_id) == (1, draft_bytes, remote_bytes, current[1]),
                "new draft or remote base differs from the approved versions")
            require(server_snapshot() == before_server and (notes / "Shared.md").read_bytes() == remote_bytes, "server changed before approved PUT")
            with proxy.lock:
                require(proxy.put_requests == puts_before + 1, "approval did not start exactly one PUT")
                proxy.expected_put = {b"if-match":f'"{current[1]}"'.encode(), b"x-workspace-epoch":current_epoch.encode(),
                    b"x-device-stream":active[4].encode(), b"x-operation-seq":str(new_seq).encode(), b"idempotency-key":new_id.encode()}
            test_stage = "acknowledgement"
            proxy.release_original_request.set()
            committed = wait(lambda: (row if (row := operation(new_id)) and row[6] == "committed" and row[9] >= new_seq and row[10] >= new_seq else None), "new operation committed and ACK confirmed")
            final = wait(lambda: (row if (row := server_document()) and row[2] == hashlib.sha256(draft_bytes).hexdigest() else None), "approved bytes on server")
            require(committed[4] == current[1] and (notes / "Shared.md").read_bytes() == draft_bytes, "conditional PUT or final bytes differ")
            with proxy.lock:
                require(proxy.checked_put_headers == [True], "approved PUT condition, epoch or operation headers differ")
            require(original_server_history() == old_server_history, "new epoch application changed old receipt or stream")
            after_client = client_snapshot(); after_server = server_snapshot()
            require(complete_old_snapshot() == before_old and next(row for row in after_client[0] if row[0] == old_id) == old_registration_row
                and [row for row in after_client[1] if row[1] == old_id] == old_operations
                and next(row for row in after_client[2] if row[0] == old_id and row[1] == node_id) == old_draft
                and next(row for row in after_client[3] if row[0] == old_id) == old_stream_row,
                "old registration, operation, draft or stream changed after ACK")
            require([row for row in after_server[2] if row[1] == new_id] and len([row for row in after_server[2] if row[1] == new_id]) == 1,
                "server journal does not contain exactly one new operation")
            for position in (3, 4):
                require([row for row in after_server[position] if row[0] == sent[0]] == [row for row in before_server[position] if row[0] == sent[0]],
                    "old server receipt or retirement changed")
            with sqlite3.connect(f"file:{server_database}?mode=ro", uri=True, timeout=5) as connection:
                stream = connection.execute("SELECT last_seq,retired_through FROM operation_streams WHERE stream_id=?", (active[4],)).fetchone()
            require(stream is not None and int(stream[0]) >= new_seq and int(stream[1]) >= new_seq,
                "server did not confirm the new stream acknowledgement")
            require((notes / "Protected.md").read_bytes() == protected and not proxy.failure.is_set(), "protected bytes changed or proxy failed")
            if args.two_clients:
                require(preserved_client(1, peer_registration[0]) == peer_before_restore, "peer old data changed after primary recovery")
        finally:
            primary_failure = sys.exc_info()[1]
            if primary_failure is not None:
                known_errors = {"WebDriver returned an error":"webdriver_response", "WebDriver request failed":"webdriver_request",
                    "WebDriver session unavailable":"webdriver_session", "browser watchdog stopped unexpectedly":"watchdog_worker"}
                known_aborts = {"scratch free-space lookup failed":"scratch_lookup", "scratch free space fell below 1 GiB":"scratch_space",
                    "maximum run time exceeded":"runtime_limit", "host I/O PSI some avg10 at or above 45 percent continuously for 10 seconds":"io_pressure",
                    "host memory or swap limit exceeded":"memory_or_swap", "browser resource watchdog failed":"watchdog_worker",
                    "browser watchdog stopped unexpectedly":"watchdog_worker"}
                primary_code = known_errors.get(str(primary_failure), "other")
                active_browser = browsers[-1] if browsers else None
                watchdog = active_browser.watchdog if active_browser is not None else None
                abort_reason = watchdog.abort_reason if watchdog is not None else None
                watchdog_code = known_aborts.get(abort_reason, "none" if abort_reason is None else "other")
                webdriver_code = active_browser.last_webdriver_code if isinstance(active_browser, DiagnosticBrowser) else "not_observed"
                worker_alive = watchdog.worker.is_alive() if watchdog is not None else None
                driver_stat = perf.process_stat(active_browser.driver.pid) if active_browser is not None and active_browser.driver is not None else None
                driver_identity = watchdog.driver_identity if watchdog is not None else None
                driver_alive = (driver_stat["state"] != "Z" and driver_identity is not None and driver_stat["starttime"] == driver_identity[1]) if driver_stat is not None else None
                print(f"Synthetic primary failure: stage={test_stage}, error_type={type(primary_failure).__name__}, "
                    f"error_code={primary_code}, webdriver_code={webdriver_code}, watchdog_code={watchdog_code}, "
                    f"watchdog_alive={worker_alive}, driver_alive={driver_alive}", file=sys.stderr, flush=True)
            failures = []
            previous_interrupt = signal.signal(signal.SIGINT, signal.SIG_IGN)
            signal.signal(signal.SIGTERM, signal.SIG_IGN)
            try:
                for browser in reversed(browsers):
                    try:
                        browser.close()
                    except BaseException as cleanup_error:
                        observations = browser.watchdog.cleanup_observations if browser.watchdog is not None else []
                        last = observations[-1] if observations else {}
                        members = last.get("members", [])
                        observed = last.get("observed_children", [])
                        summary = {"observations":len(observations), "live_members":sum(item.get("state") != "Z" for item in members),
                            "live_known_children":sum(item.get("status") == "same_identity" and item.get("state") != "Z" for item in observed),
                            "observation_errors":len(last.get("errors", []))}
                        print(f"Synthetic browser cleanup: error_type={type(cleanup_error).__name__}, summary={summary}", file=sys.stderr, flush=True)
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
    print("M3 cross-epoch frozen apply WebView passed: cold restore, explicit target, conditional PUT, confirmed ACK and preserved old data")

if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        print(f"M3 cross-epoch frozen apply WebView failed: error_type={type(error).__name__}", file=sys.stderr)
        raise SystemExit(1)
