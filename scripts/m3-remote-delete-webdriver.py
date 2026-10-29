#!/usr/bin/env python3
"""Verify remote deletion and source recovery using two isolated clients."""
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
    args = parser.parse_args()
    repo = Path(args.repo).resolve()
    helpers = module(repo / "scripts/m2-workspace-webdriver.py", "m2_helpers")
    perf = module(repo / "scripts/m3-search-perf-webdriver.py", "perf_helpers")
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

        def pair(browser, secret):
            wait(lambda: browser.js("return !!document.querySelector('.workspace-connect button:not(:disabled)')"), "pairing form")
            browser.js("""const form=document.querySelector('.workspace-connect');
              form.elements.namedItem('origin').value=arguments[0];
              form.elements.namedItem('workspace_id').value=arguments[1];
              form.elements.namedItem('pairing_secret').value=arguments[2]; form.requestSubmit();""",
              f"http://127.0.0.1:{port}", secret["workspace_id"], secret["pairing_secret"])
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
            for directories in client_directories:
                environment = os.environ.copy()
                environment.update(TAURI_WEBVIEW_AUTOMATION="true", NOTRYX_M0_DATA_DIR=str(directories["data"]),
                    XDG_DATA_HOME=str(directories["xdg_data"]), XDG_CACHE_HOME=str(directories["xdg_cache"]), XDG_CONFIG_HOME=str(directories["xdg_config"]))
                browser = browser_helpers.Browser(perf, helpers, fixture, client_binary, args.driver, environment)
                browsers.append(browser)
                browser.start()
                if len(browsers) == 2:
                    browsers[0].link(browsers[1])
                pair(browser, grants[len(browsers) - 1])
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
            clean = wait(lambda: find_node(0, "Clean.md"), "clean metadata")
            dirty = wait(lambda: find_node(0, "Dirty.md"), "dirty metadata")
            peer_clean = wait(lambda: find_node(1, "Clean.md"), "peer clean metadata")
            peer_dirty = wait(lambda: find_node(1, "Dirty.md"), "peer dirty metadata")
            require(clean["node_id"] == peer_clean["node_id"] and dirty["node_id"] == peer_dirty["node_id"], "client inventories differ")
            focus_except(peer_window)
            verify_focus(receiver, peer)
            open_document(receiver, clean["node_id"])
            clean_text = clean_bytes.decode("utf-8-sig").replace("\r\n", "\n")
            require(editor_text(receiver) == clean_text and not receiver_outbox(clean["node_id"]), "clean editor differs from source")
            clean_before = checkpoint(clean["node_id"])
            focus_except(receiver_window)
            verify_focus(peer, receiver)
            delete_with_peer(peer, peer_clean)
            clean_head = server_head()
            clean_node = server_node(clean["node_id"])
            require(clean_node is not None and clean_node[8] == 1 and clean_node[6] == hashlib.sha256(clean_bytes).hexdigest()
                and (state / "objects" / "sha256" / clean_node[6]).read_bytes() == clean_bytes and not (notes / "Clean.md").exists(), "clean deletion lost the original object")
            focus_except(peer_window)
            verify_focus(receiver, peer)
            wait(lambda: cursor(0) is not None and cursor(0) >= clean_head and cached_deleted(0, clean["node_id"]), "receiver clean tombstone catch-up")
            wait(lambda: receiver.js("return [...document.querySelectorAll('[role=status]')].some(e=>e.textContent.includes('다른 기기에서 삭제됨'))"), "clean remote deletion notice")
            wait(lambda: receiver.js("return !document.querySelector('[data-document=\"'+arguments[0]+'\"]') && !!document.querySelector('.cm-content[contenteditable=true]')", clean["node_id"]), "clean row removed with editor retained")
            require(editor_text(receiver) == clean_text and checkpoint(clean["node_id"]) == clean_before
                and not receiver_outbox(clean["node_id"]), "clean source or local state changed after deletion")
            source_recovery(receiver, clean_bytes, "clean.md")
            receiver.js("document.querySelector('[data-testid=close-document]').click()")
            wait(lambda: receiver.js("return !document.querySelector('.cm-content[contenteditable=true]')"), "close clean editor")
            open_document(receiver, dirty["node_id"])
            dirty_text = dirty_bytes.decode("utf-8-sig").replace("\r\n", "\n")
            require(editor_text(receiver) == dirty_text and not receiver_outbox(dirty["node_id"]), "dirty fixture did not start clean")
            receiver.js("""const element=document.querySelector('.cm-content');
                element.dispatchEvent(new CompositionEvent('compositionstart',{bubbles:true}));
                const view=element.cmTile.root.view;
                view.dispatch({changes:{from:view.state.doc.length,insert:arguments[0]},userEvent:'input.type'});""", marker)
            saved = wait(lambda: (row if (row := checkpoint(dirty["node_id"])) and row[1] == dirty_expected else None), "dirty durable checkpoint")
            require(saved[2] == dirty_bytes and saved[3] == dirty["content_rev"] and not receiver_outbox(dirty["node_id"]), "dirty draft was transmitted or lost its base")
            focus_except(receiver_window)
            verify_focus(peer, receiver)
            delete_with_peer(peer, peer_dirty)
            dirty_head = server_head()
            dirty_node = server_node(dirty["node_id"])
            require(dirty_node is not None and dirty_node[8] == 1 and dirty_node[6] == hashlib.sha256(dirty_bytes).hexdigest()
                and (state / "objects" / "sha256" / dirty_node[6]).read_bytes() == dirty_bytes and not (notes / "Dirty.md").exists(), "dirty deletion lost the original object")
            focus_except(peer_window)
            verify_focus(receiver, peer)
            wait(lambda: cursor(0) is not None and cursor(0) >= dirty_head and cached_deleted(0, dirty["node_id"]), "receiver dirty tombstone catch-up")
            wait(lambda: receiver.js("return [...document.querySelectorAll('[role=status]')].some(e=>e.textContent.includes('다른 기기에서 삭제됨'))"), "dirty remote deletion notice")
            wait(lambda: receiver.js("return !document.querySelector('[data-document=\"'+arguments[0]+'\"]') && !!document.querySelector('.cm-content[contenteditable=true]')", dirty["node_id"]), "dirty row removed with editor retained")
            require(editor_text(receiver) == dirty_text + marker and checkpoint(dirty["node_id"]) == saved
                and not receiver_outbox(dirty["node_id"]), "dirty source, base or outbox changed after deletion")
            receiver.js("""const done=arguments[arguments.length-1];
                document.querySelector('.cm-content').dispatchEvent(new CompositionEvent('compositionend',{bubbles:true}));
                setTimeout(done,400);""", asynchronous=True)
            wait(lambda: receiver.js("return !document.querySelector('.cm-content').cmTile.root.view.composing"), "dirty composition completion")
            require(editor_text(receiver) == dirty_text + marker and checkpoint(dirty["node_id"]) == saved
                and receiver.js("return [...document.querySelectorAll('[role=status]')].some(e=>e.textContent.includes('다른 기기에서 삭제됨'))"), "dirty source or notice changed after composition")
            source_recovery(receiver, dirty_expected, "dirty.md")
            later_outbox = receiver_outbox(dirty["node_id"])
            require(checkpoint(dirty["node_id"]) == saved
                and all(row[2] != "committed" and row[3] == dirty_expected for row in later_outbox)
                and server_node(dirty["node_id"])[8] == 1 and not (notes / "Dirty.md").exists()
                and (state / "objects" / "sha256" / dirty_node[6]).read_bytes() == dirty_bytes
                and (notes / "Protected.md").read_bytes() == protected_bytes, "recovery changed preserved data")
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
    print("M3 remote deletion WebView passed: independent clients, clean/dirty notices, original objects, drafts, copy and export")


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        print(f"M3 remote deletion WebView failed: {error}", file=sys.stderr)
        raise SystemExit(1)
