#!/usr/bin/env python3
"""Exercise credential restoration and outbox delivery with synthetic data."""

import argparse
import base64
import hashlib
import http.client
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


def mark_stage(stage):
    if stage not in {"startup", "pair", "checkpoint", "app_restart", "server_restart", "delivery", "ack", "complete"}:
        raise TestFailure("invalid scenario stage")
    Path("/work/scenario-stage").write_text(stage, encoding="ascii")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--driver", default="WebKitWebDriver")
    parser.add_argument("--binary", default="target/debug/notryx-client")
    parser.add_argument("--server", default="target/debug/notryx-server")
    args = parser.parse_args()
    require(os.environ.get("NOTRYX_TEST_SANDBOX") == "credential-integration", "sandbox marker missing")
    bus = os.environ.get("DBUS_SESSION_BUS_ADDRESS", "")
    require(bus == "unix:path=/work/run/bus.sock" or bus.startswith("unix:path=/work/run/bus.sock,"), "isolated bus missing")
    require(os.environ.get("TMPDIR") == "/work/tmp", "isolated temporary directory missing")
    require(helpers.xvfb_on_current_display(), "isolated Xvfb missing")
    mark_stage("startup")
    client_binary = str(Path(args.binary).resolve())
    server_binary = str(Path(args.server).resolve())
    server = driver = None
    session_id = None
    wd = None

    with tempfile.TemporaryDirectory(prefix="notryx-m3-credential-webdriver-") as temporary:
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

        def cached_ids():
            path = client_data / "drafts.sqlite3"
            if not path.exists():
                return set()
            with sqlite3.connect(f"file:{path}?mode=ro", uri=True, timeout=5) as db:
                return {row[0] for row in db.execute("""
                    SELECT cc.node_id FROM cached_content cc
                    JOIN client_registrations r ON r.id=cc.registration_id WHERE r.status='active'
                """)}

        def start_server():
            process = subprocess.Popen(
                [server_binary, "serve", "--config", str(config)], stdout=subprocess.PIPE,
                stderr=subprocess.DEVNULL, text=True, start_new_session=True,
            )
            ready, _, _ = select.select([process.stdout], [], [], 30)
            if not (ready and process.stdout.readline().strip() == "server_started" and helpers.leader_running(process)):
                helpers.stop(process)
                raise TestFailure("synthetic server did not start")
            return process

        def app_processes():
            found = set()
            for process in Path("/proc").iterdir():
                if not process.name.isdecimal():
                    continue
                try:
                    if str((process / "exe").resolve(strict=True)) != client_binary:
                        continue
                    stat = (process / "stat").read_text()
                    fields = stat[stat.rfind(")") + 2:].split()
                    found.add((int(process.name), int(fields[19])))
                except (OSError, ValueError, IndexError):
                    continue
            return found

        def process_still_running(identity):
            pid, starttime = identity
            try:
                stat = Path(f"/proc/{pid}/stat").read_text()
            except (FileNotFoundError, ProcessLookupError):
                return False
            fields = stat[stat.rfind(")") + 2:].split()
            require(len(fields) > 19, "application process state unavailable")
            return int(fields[19]) == starttime and fields[0] not in ("Z", "X")

        try:
            cli("init", "--config", str(config))
            server = start_server()
            status = admin("status")
            grant = admin("issue-pairing", status["owner_principal_id"])
            owner = server_request("POST", "/v1/pair", body=json.dumps({
                "protocol": "mdw-sync/1.2", "workspace_id": grant["workspace_id"],
                "pairing_secret": grant["pairing_secret"],
            }).encode(), headers={"Content-Type": "application/json"})
            authorization = {"Authorization": f"Bearer {owner['device_token']}"}
            info = server_request("GET", "/v1/server-info", headers=authorization)
            fixtures = [
                (helpers.operation_id(), "Cached match.md", "# 합성 문서\n가나 검색 결과\n".encode()),
                (helpers.operation_id(), "Cached other.md", b"# Synthetic other document\n"),
                (helpers.operation_id(), "Not cached.md", "# 합성 미캐시\n가나 검색 결과\n".encode()),
            ]
            for sequence, (node_id, name, contents) in enumerate(fixtures, 1):
                digest = base64.b64encode(hashlib.sha256(contents).digest()).decode()
                upload = server_request("POST", "/v1/uploads", body=contents, expected=201, headers={
                    **authorization, "Content-Type": "application/octet-stream", "Content-Digest": f"sha-256=:{digest}:",
                })
                server_request("POST", "/v1/nodes", body=json.dumps({
                    "node_id": node_id, "parent_id": info["root_id"], "name": name,
                    "kind": "file", "upload_id": upload["upload_id"],
                }).encode(), headers={
                    **authorization, "Content-Type": "application/json", "X-Workspace-Epoch": info["workspace_epoch"],
                    "X-Device-Stream": owner["device_stream_id"], "X-Operation-Seq": str(sequence),
                    "Idempotency-Key": helpers.operation_id(),
                })
            ui_grant = admin("issue-pairing", status["owner_principal_id"])
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
            initial_processes = wait(app_processes, "first application process")
            require(len(initial_processes) == 1, "unexpected application process count")

            def js(script, *values, asynchronous=False):
                mode = "async" if asynchronous else "sync"
                return wd("POST", f"/session/{session_id}/execute/{mode}", {"script": script, "args": list(values)})

            mark_stage("pair")
            wait(lambda: js("const button=document.querySelector('.workspace-connect button'); return !!button && !button.disabled"), "pairing form")
            js("""
                const form=document.querySelector('.workspace-connect');
                form.elements.namedItem('origin').value=arguments[0];
                form.elements.namedItem('workspace_id').value=arguments[1];
                form.elements.namedItem('pairing_secret').value=arguments[2];
                form.requestSubmit();
            """, f"http://127.0.0.1:{port}", ui_grant["workspace_id"], ui_grant["pairing_secret"])
            require(js("return document.querySelector('[name=pairing_secret]').value === ''"), "pairing secret remained in the form")

            def sync_ready():
                return js("""
                    const done=arguments[arguments.length-1];
                    window.__TAURI_INTERNALS__.invoke('m0_renderer_session')
                      .then(owner=>window.__TAURI_INTERNALS__.invoke('workspace_sync_status', {request:{renderer_session_id:owner}}))
                      .then(value=>done(value.status==='ready')).catch(()=>done(false));
                """, asynchronous=True)

            def secure_status():
                return js("""
                    const done=arguments[arguments.length-1];
                    window.__TAURI_INTERNALS__.invoke('m0_renderer_session')
                      .then(owner=>window.__TAURI_INTERNALS__.invoke('workspace_credential_status', {request:{renderer_session_id:owner}}))
                      .then(value=>done(value.status==='secure')).catch(()=>done(false));
                """, asynchronous=True)

            wait(sync_ready, "initial synchronization")
            wait(secure_status, "secure credential storage")
            for node_id, _, contents in fixtures[:2]:
                wait(lambda: js("return !!document.querySelector('[data-document=\"'+arguments[0]+'\"]:not(:disabled)')", node_id), "synthetic document in workspace")
                js("document.querySelector('[data-document=\"'+arguments[0]+'\"]').click()", node_id)
                wait(lambda: js("return document.querySelector('[aria-current=page]')?.dataset.document===arguments[0] && document.querySelector('.cm-content')?.cmTile.root.view.state.doc.toString()===arguments[1]", node_id, contents.decode()), "synthetic document open")
                wait(lambda: node_id in cached_ids(), "synthetic document cache")
            require(cached_ids() == {fixtures[0][0], fixtures[1][0]}, "fixture did not leave a document uncached")

            mark_stage("checkpoint")
            helpers.stop(server)
            server = None
            changed = fixtures[1][2].decode() + "offline draft\n"
            js("const view=document.querySelector('.cm-content').cmTile.root.view; view.dispatch({changes:{from:view.state.doc.length,insert:arguments[0]}})", "offline draft\n")
            js("document.querySelector('#save').click()")
            database = client_data / "drafts.sqlite3"

            def read_state():
                with sqlite3.connect(f"file:{database}?mode=ro", uri=True, timeout=5) as db:
                    drafts = {row[0]: (row[1], row[2], row[3]) for row in db.execute("""
                        SELECT d.document_id, c.bytes, b.bytes, b.revision FROM registered_documents d
                        JOIN client_registrations r ON r.id=d.registration_id
                        JOIN draft_checkpoints c ON c.document_id=d.storage_id
                        JOIN draft_bases b ON b.document_id=d.storage_id WHERE r.status='active'
                    """)}
                    outbox = list(db.execute("SELECT operation_id,operation_seq,status,bytes FROM client_outbox ORDER BY operation_seq"))
                    registration = db.execute("""
                        SELECT r.server_origin,r.workspace_id,r.workspace_epoch,r.device_id,r.device_stream_id
                        FROM client_registrations r WHERE r.status='active'
                    """).fetchone()
                return drafts, outbox, registration

            wait(lambda: read_state()[0].get(fixtures[1][0], (None,))[0] == changed.encode(), "pre-restart local checkpoint")
            wait(lambda: len(read_state()[1]) > 0, "pre-restart pending outbox")
            before_drafts, before_outbox, before_registration = read_state()
            require(before_drafts[fixtures[1][0]][1] == fixtures[1][2], "draft base changed")
            require(before_registration is not None and len(before_outbox) == 1, "unexpected registration or pending operations")
            operation_id, operation_seq, _, operation_bytes = before_outbox[0]
            require(operation_bytes == changed.encode(), "outbox payload differs from saved draft")
            mark_stage("app_restart")
            wd("DELETE", f"/session/{session_id}")
            session_id = None
            wait(lambda: all(not process_still_running(identity) for identity in initial_processes), "first application exit")
            closed_drafts, closed_outbox, closed_registration = read_state()
            require(closed_drafts == before_drafts, "closing changed the saved draft")
            require([(row[0], row[1], row[3]) for row in closed_outbox] == [(row[0], row[1], row[3]) for row in before_outbox], "closing changed pending operation identity or payload")
            require(closed_registration == before_registration, "closing changed registration")
            session_id = wd("POST", "/session", capabilities)["sessionId"]
            wd("POST", f"/session/{session_id}/timeouts", {"script": 30000})
            restarted_processes = wait(app_processes, "restarted application process")
            require(len(restarted_processes) == 1 and not (restarted_processes & initial_processes), "application process was not restarted")
            wait(lambda: js("return document.body.textContent.includes('서버에 연결되지 않음')"), "offline registration after application restart")
            for node_id, _, _ in fixtures[:2]:
                wait(lambda: js("return !!document.querySelector('[data-document=\"'+arguments[0]+'\"]:not(:disabled)')", node_id), "offline workspace document")
            restarted_drafts, restarted_outbox, restarted_registration = read_state()
            require(restarted_drafts == closed_drafts and restarted_registration == closed_registration, "restart changed draft or registration")
            require([(row[0], row[1], row[3]) for row in restarted_outbox] == [(row[0], row[1], row[3]) for row in closed_outbox], "restart changed pending operation identity or payload")
            js("document.querySelector('[data-document=\"'+arguments[0]+'\"]').click()", fixtures[1][0])
            wait(lambda: js("return document.querySelector('[aria-current=page]')?.dataset.document===arguments[0] && document.querySelector('.cm-content')?.cmTile.root.view.state.doc.toString()===arguments[1]", fixtures[1][0], changed), "existing draft after restart")
            for _, name, contents in fixtures:
                require((notes / name).read_bytes() == contents, "offline work changed a synthetic server file")

            mark_stage("server_restart")
            server = start_server()
            wait(secure_status, "restored secure credential", seconds=90)
            wait(sync_ready, "synchronization after credential restore", seconds=90)
            mark_stage("delivery")
            wait(lambda: (notes / fixtures[1][1]).read_bytes() == operation_bytes, "automatic outbox delivery", seconds=90)

            def confirmed():
                with sqlite3.connect(f"file:{database}?mode=ro", uri=True, timeout=5) as db:
                    row = db.execute("""
                        SELECT s.confirmed_through FROM outbox_streams s
                        JOIN client_registrations r ON r.id=s.registration_id WHERE r.status='active'
                    """).fetchone()
                    return row is not None and row[0] >= operation_seq

            mark_stage("ack")
            wait(confirmed, "client acknowledgement confirmation", seconds=90)
            require(read_state()[2] == before_registration, "automatic delivery changed registration")
            with sqlite3.connect(f"file:{state / 'state.db'}?mode=ro", uri=True, timeout=5) as db:
                changes = db.execute("SELECT COUNT(*) FROM change_sets WHERE operation_id=?", (operation_id,)).fetchone()[0]
                retired = db.execute("SELECT COUNT(*) FROM retired_operations WHERE operation_id=? AND stream_id=? AND operation_seq=?", (operation_id, before_registration[4], operation_seq)).fetchone()[0]
                stream = db.execute("SELECT last_seq, retired_through FROM operation_streams WHERE stream_id=?", (before_registration[4],)).fetchone()
            require(changes == 1 and retired == 1 and stream is not None and int(stream[0]) >= operation_seq and int(stream[1]) >= operation_seq, "server operation or acknowledgement mismatch")
        finally:
            if session_id and wd is not None:
                try:
                    wd("DELETE", f"/session/{session_id}")
                except Exception:
                    pass
            try:
                helpers.stop(driver)
            finally:
                helpers.stop(server)

    mark_stage("complete")
    print("M3 credential restart WebView passed: secure restore and existing outbox delivery.", flush=True)


if __name__ == "__main__":
    try:
        main()
    except TestFailure as failure:
        print(f"M3 credential restart WebView failed: {failure}", file=sys.stderr)
        raise SystemExit(1) from None
    except Exception:
        print("M3 credential restart WebView failed: unexpected test error", file=sys.stderr)
        raise SystemExit(1) from None
