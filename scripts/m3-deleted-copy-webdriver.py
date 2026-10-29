#!/usr/bin/env python3
"""Verify deleted-draft copy with a sent PUT using isolated clients."""
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


def isolated_xvfb():
    display = os.environ.get("DISPLAY", "")
    authority = Path(os.environ.get("XAUTHORITY", ""))
    if not re.fullmatch(r":[0-9]+", display) or not authority.is_absolute() or authority.is_symlink():
        return False
    try:
        parent = os.getppid()
        parent_args = (Path(f"/proc/{parent}/cmdline").read_bytes().split(b"\0"))
        if not any(Path(os.fsdecode(arg)).name == "xvfb-run" for arg in parent_args if arg):
            return False
        if Path(f"/proc/{parent}").stat().st_uid != os.getuid():
            return False
        parent_dir = authority.parent
        if authority.name != "Xauthority" or not parent_dir.name.startswith("xvfb-run.") or parent_dir.is_symlink():
            return False
        if parent_dir.stat().st_uid != os.getuid() or stat.S_IMODE(parent_dir.stat().st_mode) != 0o700:
            return False
        if not authority.is_file() or authority.stat().st_uid != os.getuid():
            return False
        matches = []
        for path in Path("/proc").iterdir():
            if not path.name.isdigit():
                continue
            try:
                args = path.joinpath("cmdline").read_bytes().split(b"\0")
                if not args or Path(os.fsdecode(args[0])).name != "Xvfb" or display.encode() not in args:
                    continue
                fields = path.joinpath("stat").read_text().rsplit(")", 1)[1].split()
                if int(fields[1]) == parent and path.stat().st_uid == os.getuid() and b"-auth" in args:
                    auth_index = args.index(b"-auth")
                    if auth_index + 1 < len(args) and args[auth_index + 1] == os.fsencode(authority):
                        matches.append(path.name)
            except FileNotFoundError:
                continue
        return len(matches) == 1
    except (OSError, ValueError, IndexError):
        return False


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--repo", default=str(Path(__file__).resolve().parents[1]))
    parser.add_argument("--driver", default="WebKitWebDriver")
    parser.add_argument("--binary", default="target/debug/notryx-client")
    parser.add_argument("--server", default="target/debug/notryx-server")
    parser.add_argument("--received", action="store_true", help="hold the successful original PUT response after the server commits it")
    parser.add_argument("--later-edit", action="store_true", help="edit again after confirming the copy in the request-not-received scenario")
    args = parser.parse_args()
    if args.received and args.later_edit:
        parser.error("--later-edit applies to the request-not-received scenario")
    repo = Path(args.repo).resolve()
    helpers = module(repo / "scripts/m2-workspace-webdriver.py", "m2_helpers")
    perf = module(repo / "scripts/m3-search-perf-webdriver.py", "perf_helpers")
    proxy_module = module(repo / "scripts/m3_content_conflict_proxy.py", "conflict_proxy")
    browser_helpers = module(repo / "scripts/m3-two-client-browser-helpers.py", "two_client_browser_helpers")
    require, TestFailure = helpers.require, helpers.TestFailure
    require(helpers.xvfb_on_current_display() and isolated_xvfb(), "fresh xvfb-run display owned by this test required")
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
    fixture = Path(tempfile.mkdtemp(prefix="notryx-m3-remote-delete-"))
    client_binary = str((repo / args.binary).resolve())
    server_binary = str((repo / args.server).resolve())
    browsers = []
    server = None
    proxy = None
    server_fd = None
    server_identity = None
    cleanup_safe = False
    previous_term = signal.getsignal(signal.SIGTERM)

    def interrupted(_signum, _frame):
        raise TestFailure("test interrupted; cleaning up isolated processes")

    signal.signal(signal.SIGTERM, interrupted)

    def check_watchdogs():
        for browser in browsers:
            browser.check()

    def wait(check, description, timeout=60):
        until = time.monotonic() + timeout
        while time.monotonic() < until:
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
        require(perf.observation_clear(perf.group_observation(server.pid, {server_identity})), "server process termination unconfirmed; fixture retained")

    try:
        notes, state, run, exports = (fixture / name for name in ("notes", "state", "run", "exports"))
        for path in (notes, state, run, exports):
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
        clean_bytes = b"\xef\xbb\xbf# Synthetic clean\r\nOriginal clean source\r\n"
        dirty_bytes = b"\xef\xbb\xbf# Synthetic dirty\r\nOriginal dirty source\r\n"
        protected_bytes = b"# Synthetic protected\n"
        marker = "\nSynthetic local edit 한글\n"
        dirty_expected = b"\xef\xbb\xbf" + (dirty_bytes.decode("utf-8-sig").replace("\r\n", "\n") + marker).encode("utf-8")

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

        def database(index):
            return client_directories[index]["data"] / "drafts.sqlite3"

        def query(index, sql, values=()):
            dbpath = database(index)
            if not dbpath.exists():
                return []
            with sqlite3.connect(f"file:{dbpath}?mode=ro", uri=True, timeout=5) as db:
                return db.execute(sql, values).fetchall()

        def nodes(index):
            return [json.loads(raw) for (raw,) in query(index, "SELECT n.state FROM remote_nodes n JOIN client_registrations r ON r.id=n.registration_id WHERE r.status='active'")]

        def find_node(index, name):
            matches = [node for node in nodes(index) if node.get("name") == name and node.get("kind") == "file" and not node.get("deleted")]
            return matches[0] if len(matches) == 1 else None

        def cached_deleted(index, node_id):
            return next((node for node in nodes(index) if node.get("node_id") == node_id and node.get("deleted") is True), None)

        def cursor(index):
            rows = query(index, "SELECT i.cursor FROM remote_index i JOIN client_registrations r ON r.id=i.registration_id WHERE r.status='active'")
            return int(rows[0][0]) if len(rows) == 1 else None

        def device(index):
            rows = query(index, "SELECT device_id FROM client_registrations WHERE status='active'")
            return rows[0][0] if len(rows) == 1 else None

        def checkpoint(node_id):
            rows = query(0, """SELECT c.generation,c.bytes,b.bytes,b.revision FROM registered_documents d
                JOIN client_registrations r ON r.id=d.registration_id
                JOIN draft_checkpoints c ON c.document_id=d.storage_id
                JOIN draft_bases b ON b.document_id=d.storage_id
                WHERE r.status='active' AND d.document_id=?""", (node_id,))
            return rows[0] if len(rows) == 1 else None

        def receiver_outbox(node_id):
            return query(0, """SELECT o.operation_id,o.operation_seq,o.status,o.bytes,o.expected_revision,o.receipt_json
                FROM client_outbox o JOIN client_registrations r ON r.id=o.registration_id
                WHERE r.status='active' AND o.document_id=? ORDER BY o.operation_seq""", (node_id,))

        def followup_evidence(operation_id):
            rows = query(0, """SELECT o.operation_id,o.operation_seq,o.generation,o.payload_hash,o.bytes,o.status
                FROM client_outbox o JOIN client_registrations r ON r.id=o.registration_id
                WHERE r.status='active' AND o.operation_id=?""", (operation_id,))
            return rows[0] if len(rows) == 1 else None

        def copy_intent(node_id):
            rows = query(0, """SELECT generation,local_bytes,local_hash,parent_id,name,node_id,create_operation_id,content_operation_id
                FROM deleted_draft_copy_intents i JOIN client_registrations r ON r.id=i.registration_id
                WHERE r.status='active' AND i.document_id=?""", (node_id,))
            return rows[0] if len(rows) == 1 else None

        def operation(operation_id):
            rows = query(0, """SELECT o.operation_id,o.operation_seq,o.operation_kind,o.document_id,o.expected_revision,
                o.bytes,o.status,o.committed_revision,o.receipt_json,s.ack_through,s.confirmed_through,o.terminal_kind
                FROM client_outbox o JOIN client_registrations r ON r.id=o.registration_id
                JOIN outbox_streams s ON s.registration_id=r.id
                WHERE r.status='active' AND o.operation_id=?""", (operation_id,))
            return rows[0] if len(rows) == 1 else None

        def server_operation(operation_id):
            with sqlite3.connect(f"file:{server_database}?mode=ro", uri=True, timeout=5) as db:
                pending = db.execute("SELECT stream_id,operation_seq FROM operation_receipts WHERE operation_id=?", (operation_id,)).fetchall()
                retired = db.execute("SELECT stream_id,operation_seq FROM retired_operations WHERE operation_id=?", (operation_id,)).fetchall()
            return pending, retired

        def delete_row(node_id):
            rows = query(1, """SELECT o.operation_id,o.operation_seq,o.document_id,o.expected_revision,o.status,
                o.terminal_kind,o.receipt_json,s.ack_through,s.confirmed_through,r.workspace_epoch
                FROM client_outbox o JOIN client_registrations r ON r.id=o.registration_id
                JOIN outbox_streams s ON s.registration_id=r.id
                WHERE r.status='active' AND o.operation_kind='delete' AND o.document_id=?""", (node_id,))
            if len(rows) != 1:
                return None
            row = rows[0]
            return row if row[4] == "committed" and row[6] and row[7] >= row[1] and row[8] >= row[1] else None

        def server_node(node_id):
            with sqlite3.connect(f"file:{server_database}?mode=ro", uri=True, timeout=5) as db:
                return db.execute("SELECT node_id,parent_id,name,kind,content_rev,meta_rev,content_hash,size,deleted,last_change_seq FROM nodes WHERE node_id=?", (node_id,)).fetchone()

        def server_head():
            with sqlite3.connect(f"file:{server_database}?mode=ro", uri=True, timeout=5) as db:
                row = db.execute("SELECT head_seq FROM workspaces WHERE singleton=1").fetchone()
            return int(row[0]) if row else None

        def focus_except(window):
            focus = helpers.capture_remote_change_focus(exclude_window=window)
            focus[0].XCloseDisplay(focus[1])
            return focus[2]

        def verify_focus(active, inactive):
            wait(lambda: active.js("return document.hasFocus()") and not inactive.js("return document.hasFocus()"), "independent client focus")

        def pair(browser, secret, origin_port):
            wait(lambda: browser.js("return !!document.querySelector('.workspace-connect button:not(:disabled)')"), "pairing form")
            browser.js("""const form=document.querySelector('.workspace-connect');
              form.elements.namedItem('origin').value=arguments[0];
              form.elements.namedItem('workspace_id').value=arguments[1];
              form.elements.namedItem('pairing_secret').value=arguments[2]; form.requestSubmit();""",
              f"http://127.0.0.1:{origin_port}", secret["workspace_id"], secret["pairing_secret"])
            require(browser.js("return document.querySelector('[name=pairing_secret]').value === ''"), "pairing secret remained in form")
            wait(lambda: browser.js("""const done=arguments[arguments.length-1];
              window.__TAURI_INTERNALS__.invoke('m0_renderer_session')
              .then(owner=>window.__TAURI_INTERNALS__.invoke('workspace_sync_status',{request:{renderer_session_id:owner}}))
              .then(value=>done(value.status==='ready')).catch(()=>done(false));""", asynchronous=True), "initial sync")

        def open_document(browser, node_id):
            wait(lambda: browser.js("return !!document.querySelector('[data-document=\"'+arguments[0]+'\"]:not(:disabled)')", node_id), "document row")
            browser.js("document.querySelector('[data-document=\"'+arguments[0]+'\"]').click()", node_id)
            wait(lambda: browser.js("return document.querySelector('[aria-current=page]')?.dataset.document === arguments[0] && !!document.querySelector('.cm-content[contenteditable=true]')", node_id), "opened editor")

        def editor_text(browser):
            return browser.js("return document.querySelector('.cm-content').cmTile.root.view.state.doc.toString()")

        def delete_with_peer(peer, node):
            wait(lambda: peer.js("return !!document.querySelector('[data-delete-node=\"'+arguments[0]+'\"]:not(:disabled)')", node["node_id"]), "peer delete control")
            peer.js("document.querySelector('[data-delete-node=\"'+arguments[0]+'\"]').click()", node["node_id"])
            wait(lambda: peer.js("return !!document.querySelector('[data-testid=delete-confirm]:not(:disabled)')"), "peer delete confirmation")
            require(peer.js("return document.querySelector('[aria-label=\"문서 삭제\"]')?.textContent.includes(arguments[0])", node["name"]), "peer confirmation target differs")
            peer.js("document.querySelector('[data-testid=delete-confirm]').click()")
            row = wait(lambda: delete_row(node["node_id"]), "peer delete receipt and ACK")
            receipt = json.loads(row[6]); deleted = receipt.get("node", {})
            require(row[2] == node["node_id"] and row[3] == node["node_etag"] and row[5] is None, "peer delete changed the confirmed request")
            require(receipt.get("status") == "deleted" and receipt.get("operation_id") == row[0]
                and int(receipt.get("operation_seq", -1)) == row[1] and receipt.get("workspace_epoch") == row[9]
                and deleted.get("node_id") == node["node_id"] and deleted.get("deleted") is True, "delete receipt differs from request")
            peer.js("[...document.querySelectorAll('[aria-label=\"문서 삭제\"] button')].find(x=>x.textContent.trim()==='닫기').click()")
            wait(lambda: peer.js("return !document.querySelector('[aria-label=\"문서 삭제\"]')"), "peer delete panel close")
            return row

        def source_recovery(receiver, expected, export_name):
            phase = export_name.removesuffix(".md")
            wait(lambda: receiver.js("return !!document.querySelector('#copy-source:not(:disabled)') && !!document.querySelector('#export-source:not(:disabled)')"), "source recovery controls")
            receiver.js("document.querySelector('#copy-source').click()")
            try:
                wait(lambda: receiver.js("return [...document.querySelectorAll('[role=status]')].some(e=>e.textContent.includes('원문을 복사했습니다'))"), f"{phase} copy completion")
            except TestFailure:
                diagnostic = receiver.js("""const messages=[...document.querySelectorAll('[role=status]')].map(e=>e.textContent||'');
                  return {focused:document.hasFocus(),copyDisabled:!!document.querySelector('#copy-source')?.disabled,
                    compositionError:messages.some(value=>value.includes('한글 조합을 마친 뒤')),
                    sessionError:messages.some(value=>value.includes('열린 문서를 다시 확인')),
                    nativeError:messages.some(value=>value.includes('원문 복구 작업에 실패했습니다'))};""")
                print(f"Synthetic {phase} copy diagnostic: {json.dumps(diagnostic)}", file=sys.stderr)
                raise
            probe = """import hashlib, json, gi
gi.require_version('Gtk', '3.0')
from gi.repository import Gdk, Gtk
clipboard = Gtk.Clipboard.get(Gdk.SELECTION_CLIPBOARD)
raw = clipboard.wait_for_contents(Gdk.Atom.intern('UTF8_STRING', False))
data = bytes(raw.get_data()) if raw is not None and raw.get_data() is not None else b''
print(json.dumps({'sha256': hashlib.sha256(data).hexdigest(), 'length': len(data)}))
"""
            def copied_source_matches():
                try:
                    result = json.loads(subprocess.run([sys.executable, "-c", probe], stdout=subprocess.PIPE,
                        stderr=subprocess.DEVNULL, text=True, timeout=10, check=True).stdout)
                except (OSError, subprocess.SubprocessError, ValueError):
                    raise TestFailure("isolated clipboard could not be inspected") from None
                return result == {"sha256": hashlib.sha256(expected).hexdigest(), "length": len(expected)}
            wait(copied_source_matches, "matching copied source")
            destination = exports / export_name
            failures = []
            def interact():
                try:
                    helpers.respond_to_recovery_dialog(destination)
                except Exception:
                    failures.append(True)
            worker = threading.Thread(target=interact, daemon=True)
            worker.start()
            try:
                receiver.js("document.querySelector('#export-source').click()")
            finally:
                worker.join(timeout=20)
            require(not worker.is_alive() and not failures, "recovery dialog interaction failed")
            wait(lambda: destination.exists(), "source export")
            wait(lambda: receiver.js("return !document.querySelector('#export-source').disabled"), "source export completion")
            require(destination.read_bytes() == expected, "exported source differs from preserved bytes")

        try:
            cli("init", "--config", str(config))
            (notes / "Clean.md").write_bytes(clean_bytes)
            (notes / "Dirty.md").write_bytes(dirty_bytes)
            (notes / "Protected.md").write_bytes(protected_bytes)
            server = subprocess.Popen([server_binary, "serve", "--config", str(config)], stdout=subprocess.PIPE,
                stderr=subprocess.DEVNULL, text=True, start_new_session=True)
            server_fd = os.pidfd_open(server.pid)
            server_stat = perf.process_stat(server.pid)
            require(server_stat is not None, "server identity unavailable")
            server_identity = (server.pid, server_stat["starttime"])
            output = bytearray(); deadline = time.monotonic() + 30
            while b"\n" not in output and time.monotonic() < deadline:
                require(helpers.leader_running(server), "synthetic server stopped before startup")
                ready, _, _ = select.select([server.stdout], [], [], 0.1)
                if ready:
                    chunk = os.read(server.stdout.fileno(), 128)
                    require(bool(chunk), "synthetic server startup stream closed")
                    output.extend(chunk)
                    require(len(output) <= 128, "invalid synthetic server startup response")
            require(b"\n" in output and output.partition(b"\n")[0] == b"server_started" and helpers.leader_running(server), "synthetic server did not start")
            principal = admin("status")["owner_principal_id"]
            grants = [admin("issue-pairing", principal), admin("issue-pairing", principal)]
            def initial_document():
                require(helpers.leader_running(server), "synthetic server stopped during initial scan")
                with sqlite3.connect(f"file:{server_database}?mode=ro", uri=True, timeout=5) as db:
                    rows = db.execute("SELECT name,node_id,content_rev,content_hash,size FROM nodes WHERE name IN ('Clean.md','Dirty.md','Protected.md') AND kind='file' AND deleted=0").fetchall()
                expected = {"Clean.md": clean_bytes, "Dirty.md": dirty_bytes, "Protected.md": protected_bytes}
                found = {}
                for name, node_id, revision, digest, size in rows:
                    require(name not in found, "duplicate synthetic document")
                    require(node_id and revision and digest == hashlib.sha256(expected[name]).hexdigest() and size == str(len(expected[name])), "synthetic document metadata differs")
                    found[name] = (node_id, revision)
                return found.get("Dirty.md") if len(found) == len(expected) else None

            # Server readiness precedes its asynchronous startup file scan.
            initial = wait(initial_document, "synthetic document startup scan", timeout=30)
            proxy = proxy_module.ContentConflictProxy(port, initial[0])
            proxy.start()
            for directories in client_directories:
                environment = os.environ.copy()
                environment.update(TAURI_WEBVIEW_AUTOMATION="true", NOTRYX_M0_DATA_DIR=str(directories["data"]),
                    XDG_DATA_HOME=str(directories["xdg_data"]), XDG_CACHE_HOME=str(directories["xdg_cache"]), XDG_CONFIG_HOME=str(directories["xdg_config"]))
                browser = browser_helpers.Browser(perf, helpers, fixture, client_binary, args.driver, environment)
                browsers.append(browser)
                browser.start()
                if len(browsers) == 2:
                    browsers[0].link(browsers[1])
                pair(browser, grants[len(browsers) - 1], proxy.server_address[1] if len(browsers) == 1 else port)
                if len(browsers) == 1:
                    first_focus = helpers.capture_remote_change_focus()
                    receiver_window = first_focus[2]
                    first_focus[0].XCloseDisplay(first_focus[1])
            receiver, peer = browsers
            peer_window = focus_except(receiver_window)
            require(peer_window != receiver_window, "client windows are not independent")
            verify_focus(peer, receiver)
            receiver_id = wait(lambda: device(0), "receiver device")
            peer_id = wait(lambda: device(1), "peer device")
            require(receiver_id != peer_id and database(0) != database(1), "client stores are not independent")
            dirty = wait(lambda: find_node(0, "Dirty.md"), "receiver metadata")
            peer_dirty = wait(lambda: find_node(1, "Dirty.md"), "peer metadata")
            require(dirty["node_id"] == peer_dirty["node_id"] == initial[0], "client document identity differs")
            focus_except(peer_window); verify_focus(receiver, peer)
            open_document(receiver, dirty["node_id"])
            dirty_text = dirty_bytes.decode("utf-8-sig").replace("\r\n", "\n")
            require(editor_text(receiver) == dirty_text and not receiver_outbox(dirty["node_id"]), "initial draft differs")
            first_suffix = "\nSynthetic first edit 한글\n"
            later_suffix = "\nSynthetic second edit\n"
            first_bytes = b"\xef\xbb\xbf" + (dirty_text + first_suffix).encode("utf-8")
            selected_bytes = b"\xef\xbb\xbf" + (dirty_text + first_suffix + later_suffix).encode("utf-8")
            receiver.js("""const element=document.querySelector('.cm-content');
                element.dispatchEvent(new CompositionEvent('compositionstart',{bubbles:true}));
                const view=element.cmTile.root.view;
                view.dispatch({changes:{from:view.state.doc.length,insert:arguments[0]},userEvent:'input.type'});""", first_suffix)
            first_checkpoint = wait(lambda: (row if (row := checkpoint(dirty["node_id"])) and row[1] == first_bytes else None), "first durable checkpoint")
            require(first_checkpoint[2] == dirty_bytes and not receiver_outbox(dirty["node_id"]), "first checkpoint changed base or transmitted early")
            (proxy.hold_original_success if args.received else proxy.hold_original_request).set()
            receiver.js("""const done=arguments[arguments.length-1];
                document.querySelector('.cm-content').dispatchEvent(new CompositionEvent('compositionend',{bubbles:true}));
                setTimeout(done,400);""", asynchronous=True)
            receiver.js("document.querySelector('#save').click()")
            wait((proxy.original_success_held if args.received else proxy.original_request_held).is_set, "original PUT held")
            first = wait(lambda: (rows[0] if len(rows := receiver_outbox(dirty["node_id"])) == 1 and rows[0][2] == "sent" else None), "original sent operation")
            require(first[3] == first_bytes and first[4] == initial[1], "original operation differs from checkpoint")
            proxy.original_operation_path = f"/v1/operations/{first[0]}"
            receiver.js("""const element=document.querySelector('.cm-content');
                element.dispatchEvent(new CompositionEvent('compositionstart',{bubbles:true}));
                const view=element.cmTile.root.view;
                view.dispatch({changes:{from:view.state.doc.length,insert:arguments[0]},userEvent:'input.type'});""", later_suffix)
            selected = wait(lambda: (row if (row := checkpoint(dirty["node_id"])) and row[1] == selected_bytes else None), "later durable checkpoint")
            require(selected[0] > first_checkpoint[0] and selected[2:] == first_checkpoint[2:], "later checkpoint changed base")
            receiver.js("""const done=arguments[arguments.length-1];
                document.querySelector('.cm-content').dispatchEvent(new CompositionEvent('compositionend',{bubbles:true}));
                setTimeout(done,400);""", asynchronous=True)
            receiver.js("document.querySelector('#save').click()")
            followup = wait(lambda: (rows[1] if len(rows := receiver_outbox(dirty["node_id"])) == 2 and rows[1][2] == "queued" else None), "queued later edit")
            require(followup[1] == first[1] + 1 and followup[3] == selected_bytes, "queued later edit differs")
            focus_except(receiver_window); verify_focus(peer, receiver)
            if args.received:
                changed = wait(lambda: (item if (item := find_node(1, "Dirty.md")) and item["content_rev"] != initial[1] else None), "peer received committed revision")
                require((notes / "Dirty.md").read_bytes() == first_bytes, "server did not commit held PUT")
                peer_dirty = changed
            else:
                require((notes / "Dirty.md").read_bytes() == dirty_bytes, "held request reached server")
            delete_with_peer(peer, peer_dirty)
            deleted_head = server_head()
            deleted_node = server_node(dirty["node_id"])
            deleted_bytes = first_bytes if args.received else dirty_bytes
            require(deleted_node and deleted_node[8] == 1 and deleted_node[6] == hashlib.sha256(deleted_bytes).hexdigest()
                and (state / "objects" / "sha256" / deleted_node[6]).read_bytes() == deleted_bytes and not (notes / "Dirty.md").exists(), "delete object differs")
            focus_except(peer_window); verify_focus(receiver, peer)
            wait(lambda: cursor(0) is not None and cursor(0) >= deleted_head and cached_deleted(0, dirty["node_id"]), "receiver tombstone")
            try:
                wait(lambda: receiver.js("return !!document.querySelector('[data-testid=deleted-copy-open]:not(:disabled)')"), "deleted copy action")
            except TestFailure as cause:
                if str(cause) != "timed out waiting for deleted copy action":
                    raise
                rows = receiver_outbox(dirty["node_id"])
                editor_flags = receiver.js("""const button=document.querySelector('[data-testid=deleted-copy-open]');
                    return { button:!!button, enabled:!!button && !button.disabled,
                      deletionNotice:document.body.textContent.includes('다른 기기에서 삭제됨'),
                      editor:!!document.querySelector('.cm-content[contenteditable=true]') };""")
                raise TestFailure("deleted copy action unavailable after tombstone: "
                    f"sent={sum(row[2] == 'sent' for row in rows)}, queued={sum(row[2] == 'queued' for row in rows)}, "
                    f"checkpoint_preserved={checkpoint(dirty['node_id']) == selected}, "
                    f"tombstone_cached={cached_deleted(0, dirty['node_id']) is not None}, "
                    f"hold_active={not (proxy.release_original_success if args.received else proxy.release_original_request).is_set()}, "
                    f"notice={editor_flags['deletionNotice']}, editor={editor_flags['editor']}, "
                    f"button={editor_flags['button']}, button_enabled={editor_flags['enabled']}") from cause
            require(checkpoint(dirty["node_id"]) == selected, "delete altered durable draft or base")
            require(receiver_outbox(dirty["node_id"])[0] == first, "original operation changed while held")
            require(not proxy.failure.is_set(), "proxy failed while original operation was held")
            if not args.received:
                require(server_operation(first[0]) == ([], []), "held original operation reached the server")
            receiver.js("document.querySelector('[data-testid=deleted-copy-open]').click()")
            parent_id = dirty["parent_id"]
            wait(lambda: receiver.js("return !!document.querySelector('[data-testid=deleted-copy-parent] option[value=\"'+arguments[0]+'\"]')", parent_id), "copy destination")
            require(receiver.js("return document.querySelector('[aria-label=\"보존할 기기 초안\"]')?.value") == dirty_text + first_suffix + later_suffix, "displayed draft differs")
            receiver.js("""const parent=document.querySelector('[data-testid=deleted-copy-parent]');
                const name=document.querySelector('[data-testid=deleted-copy-name]');
                Object.getOwnPropertyDescriptor(HTMLSelectElement.prototype,'value').set.call(parent,arguments[0]);
                parent.dispatchEvent(new Event('change',{bubbles:true}));
                Object.getOwnPropertyDescriptor(HTMLInputElement.prototype,'value').set.call(name,arguments[1]);
                name.dispatchEvent(new Event('input',{bubbles:true}));""", parent_id, "Recovered.md")
            wait(lambda: receiver.js("return !!document.querySelector('[data-testid=deleted-copy-review]:not(:disabled)')"), "copy review")
            receiver.js("document.querySelector('[data-testid=deleted-copy-review]').click()")
            wait(lambda: receiver.js("return !!document.querySelector('[data-testid=deleted-copy-confirm]:not(:disabled)')"), "copy confirmation")
            require(not copy_intent(dirty["node_id"]) and not (notes / "Recovered.md").exists() and checkpoint(dirty["node_id"]) == selected, "copy changed before confirmation")
            receiver.js("document.querySelector('[data-testid=deleted-copy-confirm]').click()")
            copy = wait(lambda: copy_intent(dirty["node_id"]), "durable copy selection")
            require(copy[0] == selected[0] and copy[1] == selected_bytes and copy[2] == "sha256:" + hashlib.sha256(selected_bytes).hexdigest()
                and copy[3:5] == (parent_id, "Recovered.md") and copy[5] != dirty["node_id"] and copy[7] is None
                and not operation(copy[6]) and not (notes / "Recovered.md").exists(), "confirmed copy differs")
            latest = selected
            if args.later_edit:
                wait(lambda: receiver.js("return !!document.querySelector('[data-testid=deleted-copy-progress]:not(:disabled)')"), "confirmed copy status")
                last_suffix = "\nSynthetic edit after copy confirmation\n"
                latest_bytes = selected_bytes + last_suffix.encode("utf-8")
                receiver.js("""const element=document.querySelector('.cm-content');
                    element.dispatchEvent(new CompositionEvent('compositionstart',{bubbles:true}));
                    const view=element.cmTile.root.view;
                    view.dispatch({changes:{from:view.state.doc.length,insert:arguments[0]},userEvent:'input.type'});""", last_suffix)
                latest = wait(lambda: (row if (row := checkpoint(dirty["node_id"])) and row[1] == latest_bytes else None), "post-confirmation durable checkpoint")
                require(latest[0] > selected[0] and latest[2:] == selected[2:] and copy_intent(dirty["node_id"]) == copy, "later edit changed confirmed copy or base")
                receiver.js("""const done=arguments[arguments.length-1];
                    document.querySelector('.cm-content').dispatchEvent(new CompositionEvent('compositionend',{bubbles:true}));
                    setTimeout(done,400);""", asynchronous=True)
                receiver.js("document.querySelector('#save').click()")
                latest_followup = wait(lambda: (row if (row := followup_evidence(followup[0])) and row[2] == latest[0] and row[4] == latest_bytes and row[5] == "queued" else None), "latest queued followup")
                require(latest_followup[1] == followup[1] and latest_followup[3] == "sha256:" + hashlib.sha256(latest_bytes).hexdigest()
                    and copy_intent(dirty["node_id"]) == copy and proxy.route_count("abandon") == 0, "later edit did not preserve copy or queued identity")
            else:
                latest_followup = followup_evidence(followup[0])
                require(latest_followup and latest_followup[4] == selected_bytes, "queued followup missing")
            proxy.copy_content_path = f"/v1/nodes/{copy[5]}/content"
            proxy.hold_copy_put.set()
            wait(lambda: receiver.js("return !!document.querySelector('[data-testid=deleted-copy-progress]:not(:disabled)')"), "copy progress")
            receiver.js("document.querySelector('[data-testid=deleted-copy-progress]').click()")
            queued_copy = wait(lambda: (row if (row := operation(copy[6])) and row[6] == "queued" and row[8] is None else None), "queued copy creation before original resolution")
            require(queued_copy[2:4] == ("create", copy[5]) and not (notes / "Recovered.md").exists(), "copy creation was not safely queued")
            (proxy.release_original_success if args.received else proxy.release_original_request).set()
            resolved = wait(lambda: (row if (row := operation(first[0])) and row[10] >= row[1] and (row[6] == "committed" and row[11] is None if args.received else row[6] == "conflict" and row[11] == "node_deleted") else None), "original operation result and confirmed ACK")
            stream = query(0, "SELECT device_stream_id FROM client_registrations WHERE status='active'")[0][0]
            require(resolved[0] == first[0] and resolved[1] == first[1] and (not args.received or resolved[7]), "original operation result differs")
            retired = wait(lambda: (rows if (rows := server_operation(first[0])[1]) else None), "server retired original operation")
            require(retired == [(stream, str(first[1]))], "server retired a different original operation")
            if not args.received:
                wait(lambda: receiver.js("return !!document.querySelector('[data-testid=deleted-copy-followup-open]:not(:disabled)') && document.body.textContent.includes(arguments[0]) && document.body.textContent.includes(arguments[1])", first[0], followup[0]), "specific deleted followup")
                require(receiver.js("""const text=document.querySelector('[aria-label="삭제 초안 사본 상태"]')?.textContent || '';
                    return text.includes(arguments[0]) && text.includes(arguments[1]) && text.includes('순번 '+arguments[2]) &&
                      text.includes('generation '+arguments[3]) && text.includes(arguments[4]);""",
                    first[0], latest_followup[0], str(latest_followup[1]), str(latest_followup[2]), latest_followup[3]), "displayed followup evidence differs")
                require(proxy.route_count("abandon") == 0 and operation(copy[6]) is not None and operation(copy[6])[:9] == queued_copy[:9] and not (notes / "Recovered.md").exists()
                    and followup_evidence(followup[0]) == latest_followup and copy_intent(dirty["node_id"]) == copy
                    and checkpoint(dirty["node_id"]) == latest, "followup changed before approval")
                receiver.js("document.querySelector('[data-testid=deleted-copy-followup-open]').click()")
                wait(lambda: receiver.js("return !!document.querySelector('[data-testid=deleted-copy-followup-approve]:not(:disabled)')"), "followup approval")
                require(proxy.route_count("abandon") == 0 and receiver.js("""const text=document.querySelector('[aria-label="삭제 초안 사본 상태"]')?.textContent || '';
                    return text.includes(arguments[0]) && text.includes('순번 '+arguments[1]) && text.includes('generation '+arguments[2]) && text.includes(arguments[3]);""",
                    latest_followup[0], str(latest_followup[1]), str(latest_followup[2]), latest_followup[3]), "followup confirmation differs or was cancelled early")
                receiver.js("document.querySelector('[data-testid=deleted-copy-followup-approve]').click()")
                abandoned = wait(lambda: (row if (row := operation(followup[0])) and row[6] == "committed" and row[11] == "abandoned" and row[10] >= row[1] else None), "approved followup and confirmed ACK")
                retired_followup = wait(lambda: (rows if (rows := server_operation(followup[0])[1]) else None), "server retired approved followup")
                require(abandoned[0] == followup[0] and retired_followup == [(stream, str(followup[1]))] and proxy.route_count("abandon") >= 1, "wrong followup abandoned")
            wait(proxy.copy_put_held.is_set, "copy content response held")
            created = wait(lambda: (row if (row := operation(copy[6])) and row[6] == "committed" and row[10] >= row[1] else None), "copy create receipt and ACK")
            created_receipt = json.loads(created[8]); created_node = created_receipt.get("node", {})
            require(created_receipt.get("operation_id") == copy[6] and created_node.get("node_id") == copy[5]
                and created_node.get("parent_id") == parent_id and created_node.get("name") == "Recovered.md", "copy create receipt differs")
            linked = wait(lambda: (row if (row := copy_intent(dirty["node_id"])) and row[7] else None), "copy content operation")
            content = wait(lambda: operation(linked[7]), "copy content outbox")
            require(content[3] == copy[5] and content[4] == created_node.get("content_rev") and content[5] == selected_bytes and content[1] > created[1], "copy content differs from confirmed draft or created revision")
            stream = query(0, "SELECT device_stream_id FROM client_registrations WHERE status='active'")[0][0]
            with proxy.lock:
                proxy.ack_target = (stream, str(content[1])); proxy.hold_ack.set()
            proxy.release_copy_put.set()
            wait(proxy.ack_held.is_set, "copy content ACK held", timeout=15)
            with proxy.lock:
                observed = proxy.ack_observed
            require(observed and observed[:2] == (stream, str(content[1])) and 200 <= observed[2] < 300, "incorrect ACK held")
            before_ack = wait(lambda: (row if (row := operation(linked[7])) and row[6] == "committed" and row[9] >= row[1] and row[10] < row[1] else None), "content receipt before confirmed ACK", timeout=5)
            until = time.monotonic() + 1.3
            while time.monotonic() < until:
                check_watchdogs()
                require(operation(linked[7])[10] < before_ack[1] and "사본 저장 완료" not in receiver.js("return document.body.textContent"), "copy completed before ACK")
                time.sleep(0.1)
            proxy.release_ack.set()
            wait(lambda: (row if (row := operation(linked[7])) and row[6] == "committed" and row[10] >= row[1] else None), "copy content confirmed ACK")
            wait(lambda: receiver.js("return document.body.textContent.includes('사본 저장 완료')"), "copy saved display")
            require((notes / "Recovered.md").read_bytes() == selected_bytes and (notes / "Protected.md").read_bytes() == protected_bytes
                and server_node(dirty["node_id"])[8] == 1 and not (notes / "Dirty.md").exists()
                and (state / "objects" / "sha256" / deleted_node[6]).read_bytes() == deleted_bytes
                and checkpoint(dirty["node_id"])[0:2] == latest[0:2] and copy_intent(dirty["node_id"])[0:7] == copy[0:7]
                and receiver.js("return document.querySelector('.cm-content').cmTile.root.view.state.doc.toString()") == latest[1].decode("utf-8-sig")
                and not proxy.failure.is_set(), "copy or preserved data differs")
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
    print("M3 deleted draft copy WebView passed: sent PUT " + ("received" if args.received else "not received") + (", later edit preserved" if args.later_edit else "") + ", explicit copy and confirmed ACK")


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        print(f"M3 deleted draft copy WebView failed: {error}", file=sys.stderr)
        raise SystemExit(1)
