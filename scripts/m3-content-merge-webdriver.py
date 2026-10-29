#!/usr/bin/env python3
"""Verify an explicitly reviewed merge with two isolated WebViews and a real server."""
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
    args.choose_remote_pending = True
    copy_retry = False
    copy_scenario = False
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
    fixture = Path(tempfile.mkdtemp(prefix="notryx-m3-content-merge-"))
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
            return rows[1] if len(rows) >= 2 else None

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

        def merge_intent(operation_id):
            rows = query(0, """SELECT i.generation,i.base_revision,i.base_bytes,i.local_bytes,i.remote_revision,
                i.remote_hash,i.remote_bytes,i.choice,i.merged_bytes,i.merged_hash,i.intent_version
                FROM content_conflict_intents i JOIN client_registrations r ON r.id=i.registration_id
                WHERE r.status='active' AND i.operation_id=?""", (operation_id,))
            return rows[0] if len(rows) == 1 else None

        def merge_link(operation_id):
            rows = query(0, """SELECT m.intent_version,m.content_operation_id,m.abandoned_through
                FROM content_conflict_merged_operations m JOIN client_registrations r ON r.id=m.registration_id
                WHERE r.status='active' AND m.operation_id=?""", (operation_id,))
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

        def pair(browser, secret, origin):
            wait(lambda: browser.js("return !!document.querySelector('.workspace-connect button:not(:disabled)')"), "pairing form")
            browser.js("""const form=document.querySelector('.workspace-connect');
              form.elements.namedItem('origin').value=arguments[0];
              form.elements.namedItem('workspace_id').value=arguments[1];
              form.elements.namedItem('pairing_secret').value=arguments[2]; form.requestSubmit();""",
              origin, secret["workspace_id"], secret["pairing_secret"])
            require(browser.js("return document.querySelector('[name=pairing_secret]').value === ''"), "pairing secret remained in form")
            wait(lambda: browser.js("""const done=arguments[arguments.length-1];
              window.__TAURI_INTERNALS__.invoke('m0_renderer_session')
              .then(owner=>window.__TAURI_INTERNALS__.invoke('workspace_sync_status',{request:{renderer_session_id:owner}}))
              .then(value=>done(value.status==='ready')).catch(()=>done(false));""", asynchronous=True), "initial sync")

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
            if copy_retry:
                (notes / "Conflict copy.md").write_bytes(occupied)
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
            initial = wait(server_document, "server source metadata")
            require((notes / "Shared.md").read_bytes() == base, "server source was changed before pairing")
            proxy = proxy_module.ContentConflictProxy(port, initial[0])
            proxy.start()
            principal = admin("status")["owner_principal_id"]
            grants = [admin("issue-pairing", principal), admin("issue-pairing", principal)]
            for index, directories in enumerate(client_directories):
                environment = os.environ.copy()
                environment.update(TAURI_WEBVIEW_AUTOMATION="true", NOTRYX_M0_DATA_DIR=str(directories["data"]),
                    XDG_DATA_HOME=str(directories["xdg_data"]), XDG_CACHE_HOME=str(directories["xdg_cache"]), XDG_CONFIG_HOME=str(directories["xdg_config"]))
                browser = browser_helpers.Browser(perf, helpers, fixture, client_binary, args.driver, environment)
                browsers.append(browser)
                browser.start()
                if len(browsers) == 2:
                    browsers[0].link(browsers[1])
                origin_port = proxy.server_address[1] if index == 0 else port
                pair(browser, grants[index], f"http://127.0.0.1:{origin_port}")
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
            require(receiver_id != peer_id, "client registrations are not independent")
            node_id, base_revision = initial[0], initial[1]
            receiver_node = wait(lambda: cached_node(0, node_id), "receiver metadata")
            peer_node = wait(lambda: cached_node(1, node_id), "peer metadata")
            require(receiver_node["content_rev"] == base_revision == peer_node["content_rev"], "clients did not start from same base revision")
            open_document(peer, node_id)
            require(editor_text(peer) == base_text, "peer initial editor differs from base")
            focus_except(peer_window); verify_focus(receiver, peer)
            open_document(receiver, node_id)
            require(editor_text(receiver) == base_text, "receiver initial editor differs from base")
            begin_edit(receiver, local_suffix)
            local_checkpoint = wait(lambda: (row if (row := checkpoint(0, node_id)) and row[1] == local_bytes else None), "receiver durable checkpoint")
            require(local_checkpoint[2] == base and local_checkpoint[3] == base_revision and not outbox(0, node_id), "receiver draft lost base or was sent early")
            focus_except(receiver_window); verify_focus(peer, receiver)
            begin_edit(peer, remote_first_suffix)
            peer_checkpoint = wait(lambda: (row if (row := checkpoint(1, node_id)) and row[1] == remote_first_bytes else None), "peer first durable checkpoint")
            require(peer_checkpoint[2] == base and peer_checkpoint[3] == base_revision and not outbox(1, node_id), "peer draft lost base or was sent early")
            end_edit(peer)
            peer.js("document.querySelector('#save').click()")
            first_operation = wait(lambda: peer_commit(node_id, peer_checkpoint[0]), "peer first commit and ACK")
            require(first_operation[3] == base_revision and (notes / "Shared.md").read_bytes() == remote_first_bytes, "peer first commit differs from expected bytes or base")
            first_server = server_document()
            require(first_server and first_server[0] == node_id and first_server[1] != base_revision, "first peer revision is invalid")
            first_revision, first_head = first_server[1], int(first_server[3])
            focus_except(peer_window); verify_focus(receiver, peer)
            wait(lambda: cursor(0) is not None and cursor(0) >= first_head, "receiver catch-up to peer first commit")
            require(checkpoint(0, node_id) == local_checkpoint and not outbox(0, node_id), "remote update replaced receiver draft or base")
            if args.choose_remote_pending or copy_scenario:
                proxy.hold_all_original_412.set()
                proxy.hold_original_412_lookup.set()
                require(not outbox(0, node_id), "receiver operation was queued before the 412 hold was armed")
            end_edit(receiver)
            receiver.js("document.querySelector('#save').click()")
            pending_followup = None
            if args.choose_remote_pending or copy_scenario:
                wait(proxy.put_412_held.is_set, "held original HTTP 412 response")
                first_sent = wait(lambda: (rows[0] if len(rows := outbox(0, node_id)) == 1 and rows[0][6] == "sent" else None),
                    "original content operation sent before its 412")
                proxy.original_operation_path = f"/v1/operations/{first_sent[0]}"
                followup_suffix = "\nAdditional local edit before 412\n"
                local_text += followup_suffix
                local_bytes = b"\xef\xbb\xbf" + local_text.encode("utf-8")
                begin_edit(receiver, followup_suffix)
                local_checkpoint = wait(lambda: (row if (row := checkpoint(0, node_id)) and row[1] == local_bytes else None),
                    "followup durable checkpoint while 412 is held")
                require(local_checkpoint[0] == first_sent[2] + 1 and local_checkpoint[2:] == (base, base_revision),
                    "followup checkpoint changed the original base")
                end_edit(receiver)
                receiver.js("document.querySelector('#save').click()")
                pending_followup = wait(lambda: (row if (row := followup_row(node_id)) and row[6] == "queued" else None),
                    "followup queued behind the in-flight content operation")
                require(pending_followup[1] == first_sent[1] + 1 and pending_followup[2] == local_checkpoint[0]
                    and pending_followup[3] is None and pending_followup[4] == local_bytes
                    and pending_followup[7] == first_sent[0] and pending_followup[8:10] == ("none", None),
                    "followup did not retain the original dependency, sequence and payload")
                proxy.release_put_412.set()
            failed = wait(lambda: conflict(node_id), "receiver conflict and ACK")
            require(failed[3] == base_revision and failed[4] == first_local_bytes and failed[7] == first_revision
                and failed[8] == '"' + first_revision + '"' and failed[9] is None, "412 did not preserve request and first remote revision")
            observed_put_412 = proxy.content_count("PUT", 412)
            require(observed_put_412 >= 1 and not proxy.failure.is_set(), "real content PUT HTTP 412 was not observed")
            require((notes / "Shared.md").read_bytes() == remote_first_bytes and checkpoint(0, node_id) == local_checkpoint, "conflict overwrote server or local data")
            focus_except(receiver_window); verify_focus(peer, receiver)
            begin_edit(peer, remote_second_suffix)
            peer_second_checkpoint = wait(lambda: (row if (row := checkpoint(1, node_id)) and row[1] == remote_second_bytes else None), "peer second durable checkpoint")
            require(peer_second_checkpoint[2] == remote_first_bytes and peer_second_checkpoint[3] == first_revision, "peer second base is incorrect")
            end_edit(peer)
            peer.js("document.querySelector('#save').click()")
            second_operation = wait(lambda: peer_commit(node_id, peer_second_checkpoint[0]), "peer second commit and ACK")
            require(second_operation[0] != first_operation[0] and second_operation[1] > first_operation[1]
                and second_operation[3] == first_revision and (notes / "Shared.md").read_bytes() == remote_second_bytes, "second peer commit differs from expected")
            current_server = server_document()
            require(current_server and current_server[0] == node_id and len({base_revision, first_revision, current_server[1]}) == 3, "second peer revision is invalid")
            current_revision, current_head = current_server[1], int(current_server[3])
            focus_except(peer_window); verify_focus(receiver, peer)
            wait(lambda: cursor(0) is not None and cursor(0) >= current_head, "receiver catch-up to peer second commit")
            before_workspace = wait(workspace_evidence, "workspace journal and protected node")
            require(int(before_workspace[0]) >= current_head and before_workspace[2][0] != node_id,
                "workspace head or protected node identity is invalid")
            before_outbox = outbox(0, node_id)
            expected_operations = 2 if args.choose_remote_pending or copy_scenario else 1
            require(len(before_outbox) == expected_operations and before_outbox[0] == failed
                and checkpoint(0, node_id) == local_checkpoint, "later peer change altered conflict evidence or local draft")
            if pending_followup:
                current_followup = followup_row(node_id)
                require(current_followup and current_followup[:10] == pending_followup[:10]
                    and current_followup[10] >= failed[1] and current_followup[11] >= failed[1],
                    "412 response changed the queued followup")
            wait(lambda: receiver.js("return !!document.querySelector('[data-testid=conflict-review-open]:not(:disabled)')"), "conflict comparison control")
            receiver.js("document.querySelector('[data-testid=conflict-review-open]').click()")
            wait(lambda: receiver.js("return !!document.querySelector('[aria-label=\"충돌 원문 비교\"] textarea[aria-label=\"조회 당시 서버 본문\"]')"), "conflict comparison")
            wait(lambda: receiver.js("return !!document.querySelector('[data-testid=conflict-copy-open]') && !!document.querySelector('[data-testid=conflict-choose-remote]')"), "conflict choices")
            shown = receiver.js("""const panel=document.querySelector('[aria-label=\"충돌 원문 비교\"]');
                const read=label=>{const el=panel.querySelector('textarea[aria-label="'+label+'"]'); return {value:el.value,readOnly:el.readOnly,label:el.parentElement.textContent};};
                return {base:read('기준 원문'),local:read('이 기기에 저장된 내 초안'),remote:read('조회 당시 서버 본문'),text:panel.textContent,
                  buttons:[...panel.querySelectorAll('button')].map(button=>button.textContent.trim())};""")
            require(shown["base"]["value"] == base_text and shown["local"]["value"] == local_text and shown["remote"]["value"] == remote_second_text
                and all(shown[name]["readOnly"] for name in ("base", "local", "remote")), "comparison texts differ or can be edited")
            require(base_revision in shown["base"]["label"] and current_revision in shown["remote"]["label"]
                and "BOM 포함" in shown["base"]["label"] and "줄바꿈: CRLF" in shown["base"]["label"]
                and "BOM 포함" in shown["local"]["label"] and "줄바꿈: LF" in shown["local"]["label"]
                and "BOM 포함" in shown["remote"]["label"] and "줄바꿈: LF" in shown["remote"]["label"]
                and "412 응답 당시 서버 revision: " + first_revision in shown["text"], "comparison revision or original-byte metadata differs")
            require("기기 초안을 합본으로 확인" in shown["buttons"] and "닫기" in shown["buttons"], "merge review controls unavailable")
            require(outbox(0, node_id) == before_outbox and checkpoint(0, node_id) == local_checkpoint
                and server_document() == current_server and (notes / "Shared.md").read_bytes() == remote_second_bytes
                and (notes / "Protected.md").read_bytes() == protected and workspace_evidence() == before_workspace
                and conflict_intent(failed[0]) is None,
                "comparison changed conflict data or server source")
            receiver.js("[...document.querySelectorAll('[aria-label=\"충돌 원문 비교\"] button')].find(button=>button.textContent.trim()==='닫기').click()")
            wait(lambda: receiver.js("return !document.querySelector('[aria-label=\"충돌 원문 비교\"]')"), "close comparison")
            merge_suffix = "\nSynthetic merged text 한글\n"
            merge_text = local_text + merge_suffix
            merge_bytes = b"\xef\xbb\xbf" + merge_text.encode("utf-8")
            begin_edit(receiver, merge_suffix)
            merge_checkpoint = wait(lambda: (row if (row := checkpoint(0, node_id)) and row[1] == merge_bytes else None), "merged durable checkpoint")
            require(merge_checkpoint[0] > local_checkpoint[0] and merge_checkpoint[2:] == (base, base_revision), "merge editor changed the conflict base")
            end_edit(receiver)
            receiver.js("document.querySelector('#save').click()")
            latest_followup = wait(lambda: (row if (row := followup_row(node_id)) and row[0] == pending_followup[0] and row[2] == merge_checkpoint[0] and row[4] == merge_bytes and row[6] == "queued" else None), "updated followup checkpoint")
            require(latest_followup[1] == pending_followup[1] and latest_followup[5] == "sha256:" + hashlib.sha256(merge_bytes).hexdigest()
                and latest_followup[7] == failed[0] and latest_followup[8:10] == ("none", None), "updated followup identity or bytes differs")
            wait(lambda: receiver.js("return !!document.querySelector('[data-testid=conflict-review-open]:not(:disabled)')"), "updated conflict comparison control")
            receiver.js("document.querySelector('[data-testid=conflict-review-open]').click()")
            wait(lambda: receiver.js("return document.querySelector('[aria-label=\"이 기기에 저장된 내 초안\"]')?.value===arguments[0]", merge_text), "reviewed merged bytes")
            remote_matches = receiver.js("return document.querySelector('[aria-label=\"조회 당시 서버 본문\"]')?.value === arguments[0]", remote_second_text)
            require(remote_matches, "merge comparison remote textarea differs from the expected synthetic server text")
            require(merge_intent(failed[0]) is None, "merge comparison created an intent before confirmation")
            require(merge_link(failed[0]) is None, "merge comparison created a merged operation before confirmation")
            compared_checkpoint = checkpoint(0, node_id)
            require(compared_checkpoint == merge_checkpoint, "merge comparison changed checkpoint: "
                f"exists={compared_checkpoint is not None}, "
                f"generation_matches={bool(compared_checkpoint and compared_checkpoint[0] == merge_checkpoint[0])}, "
                f"bytes_match={bool(compared_checkpoint and compared_checkpoint[1] == merge_checkpoint[1])}, "
                f"base_matches={bool(compared_checkpoint and compared_checkpoint[2:] == merge_checkpoint[2:])}")
            require((notes / "Shared.md").read_bytes() == remote_second_bytes, "merge comparison changed synthetic server file")
            compared_workspace = workspace_evidence()
            require(compared_workspace == before_workspace, "merge comparison changed workspace evidence: "
                f"before_head={before_workspace[0]}, after_head={compared_workspace[0] if compared_workspace else None}, "
                f"before_journal_count={len(before_workspace[1])}, after_journal_count={len(compared_workspace[1]) if compared_workspace else None}, "
                f"journal_matches={bool(compared_workspace and compared_workspace[1] == before_workspace[1])}, "
                f"protected_matches={bool(compared_workspace and compared_workspace[2] == before_workspace[2])}")
            wait(lambda: receiver.js("return !!document.querySelector('[data-testid=conflict-merge-review]:not(:disabled)')"), "merge review control")
            receiver.js("document.querySelector('[data-testid=conflict-merge-review]').click()")
            wait(lambda: receiver.js("return !!document.querySelector('[data-testid=conflict-merge-confirm]:not(:disabled)')"), "explicit merge confirmation")
            require(merge_intent(failed[0]) is None and merge_link(failed[0]) is None and proxy.route_count("abandon") == 0
                and checkpoint(0, node_id) == merge_checkpoint and followup_row(node_id)[:10] == latest_followup[:10]
                and server_document() == current_server and workspace_evidence() == before_workspace,
                "merge review changed data before confirmation")
            receiver.js("document.querySelector('[data-testid=conflict-merge-confirm]').click()")
            pending = wait(lambda: merge_intent(failed[0]), "durable merged choice")
            require(pending == (merge_checkpoint[0], base_revision, base, merge_bytes, current_revision,
                "sha256:" + current_server[2], remote_second_bytes, "save_merged", merge_bytes,
                "sha256:" + hashlib.sha256(merge_bytes).hexdigest(), 0), "stored merge differs from reviewed evidence")
            require(merge_link(failed[0]) is None and followup_row(node_id)[:10] == latest_followup[:10]
                and server_document() == current_server and (notes / "Shared.md").read_bytes() == remote_second_bytes
                and "합본 저장 완료" not in receiver.js("return document.body.textContent"), "merge confirmation sent or completed early")
            wait(lambda: receiver.js("return !!document.querySelector('[data-testid=conflict-merge-queue]:not(:disabled)')"), "merge preparation")
            receiver.js("document.querySelector('[data-testid=conflict-merge-queue]').click()")
            wait(lambda: receiver.js("return !!document.querySelector('[data-testid=conflict-merge-followup-confirm]:not(:disabled)')"), "explicit merge followup approval")
            require(receiver.js("""const text=document.querySelector('[aria-label="합본 저장 상태"]')?.textContent || '';
                return text.includes(arguments[0]) && text.includes('순번 '+arguments[1]) &&
                  text.includes('generation '+arguments[2]) && text.includes(arguments[3]);""",
                latest_followup[0], str(latest_followup[1]), str(latest_followup[2]), latest_followup[5]), "followup approval evidence differs")
            require(proxy.route_count("abandon") == 0 and merge_link(failed[0]) is None and followup_row(node_id)[:10] == latest_followup[:10]
                and merge_intent(failed[0]) == pending and checkpoint(0, node_id) == merge_checkpoint
                and workspace_evidence() == before_workspace and server_document() == current_server,
                "followup was processed before explicit approval")
            receiver.js("document.querySelector('[data-testid=conflict-merge-followup-confirm]').click()")
            def acknowledged_followup():
                row = followup_row(node_id)
                return row if row and row[0] == latest_followup[0] and row[6] == "committed" and row[8:10] == ("sent", "abandoned") and row[11] >= row[1] else None
            abandoned = wait(acknowledged_followup, "approved followup and confirmed ACK")
            require(abandoned[:6] == latest_followup[:6] and abandoned[7] == failed[0] and proxy.route_count("abandon") >= 1,
                "abandon changed selected followup")
            wait(lambda: (evidence if (evidence := server_followup(abandoned[0], abandoned[12]))[1] == (abandoned[12], str(abandoned[1])) and evidence[2] == 0 else None), "server retired approved followup")
            require(merge_link(failed[0]) is None and merge_intent(failed[0]) == pending
                and checkpoint(0, node_id) == merge_checkpoint and server_document() == current_server
                and workspace_evidence() == before_workspace, "followup approval changed merge or server document")
            proxy.copy_content_path = proxy.document_path
            proxy.hold_copy_put.set()
            wait(lambda: receiver.js("return !!document.querySelector('[data-testid=conflict-merge-queue]:not(:disabled)')"), "merge queue after followup ACK")
            receiver.js("document.querySelector('[data-testid=conflict-merge-queue]').click()")
            link = wait(lambda: merge_link(failed[0]), "queued merged operation")
            require(link[0] == 0 and link[1] not in (failed[0], abandoned[0]) and link[2] >= abandoned[1], "merged operation linkage differs")
            wait(proxy.copy_put_held.is_set, "merged PUT response held")
            merged = wait(lambda: operation(link[1]), "merged outbox operation")
            require(merged[1] > abandoned[1] and merged[2:6] == ("content", node_id, current_revision, merge_bytes)
                and merged[10] < merged[1] and (notes / "Shared.md").read_bytes() == merge_bytes,
                "merged PUT did not use reviewed bytes and conditional revision")
            require("합본 저장 완료" not in receiver.js("return document.body.textContent"), "merge completed before receipt")
            with proxy.lock:
                proxy.ack_target = (abandoned[12], str(merged[1])); proxy.hold_ack.set()
            proxy.release_copy_put.set()
            wait(proxy.ack_held.is_set, "merged ACK held", timeout=15)
            with proxy.lock:
                observed = proxy.ack_observed
            require(observed and observed[:2] == (abandoned[12], str(merged[1])) and 200 <= observed[2] < 300, "wrong merged ACK held")
            receipt = wait(lambda: (row if (row := operation(link[1])) and row[6] == "committed" and row[9] >= row[1] and row[10] < row[1] else None), "merged receipt before confirmed ACK", timeout=5)
            deadline = time.monotonic() + 1.3
            while time.monotonic() < deadline:
                check_watchdogs()
                require(operation(link[1])[10] < receipt[1] and proxy.ack_held.is_set()
                    and "합본 저장 완료" not in receiver.js("return document.body.textContent"), "merge completed before confirmed ACK")
                time.sleep(0.1)
            proxy.release_ack.set()
            confirmed = wait(lambda: (row if (row := operation(link[1])) and row[6] == "committed" and row[10] >= row[1] else None), "merged confirmed ACK")
            wait(lambda: receiver.js("return document.body.textContent.includes('합본 저장 완료')"), "merge saved display")
            final_server = server_document()
            final_workspace = workspace_evidence()
            require(final_server and final_server[0] == node_id and final_server[1] == confirmed[7]
                and final_server[2] == hashlib.sha256(merge_bytes).hexdigest() and (notes / "Shared.md").read_bytes() == merge_bytes
                and final_workspace and final_workspace[2] == before_workspace[2]
                and final_workspace[1] == before_workspace[1] + ((final_server[3], confirmed[0]),)
                and (notes / "Protected.md").read_bytes() == protected and merge_intent(failed[0]) == pending
                and outbox(0, node_id)[0][:10] == failed[:10] and followup_row(node_id)[:10] == abandoned[:10]
                and checkpoint(0, node_id)[1] == merge_bytes and not proxy.failure.is_set(), "merge completion changed preserved evidence")
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
    print("M3 content merge WebView passed: reviewed merge, explicit followup approval, conditional PUT and confirmed ACK")

if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        print(f"M3 content merge WebView failed: {error}", file=sys.stderr)
        raise SystemExit(1)
