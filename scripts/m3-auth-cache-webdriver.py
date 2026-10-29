#!/usr/bin/env python3
"""Exercise local access after device revocation in an isolated Linux WebView with synthetic data."""

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


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--driver", default="WebKitWebDriver")
    parser.add_argument("--binary", default="target/debug/notryx-client")
    parser.add_argument("--server", default="target/debug/notryx-server")
    args = parser.parse_args()
    require(helpers.xvfb_on_current_display(), "search test requires isolated Xvfb")
    client_binary = str(Path(args.binary).resolve())
    server_binary = str(Path(args.server).resolve())
    server = driver = None
    session_id = None
    wd = None

    with tempfile.TemporaryDirectory(prefix="notryx-m3-auth-webdriver-") as temporary:
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

        try:
            cli("init", "--config", str(config))
            server = subprocess.Popen(
                [server_binary, "serve", "--config", str(config)], stdout=subprocess.PIPE,
                stderr=subprocess.DEVNULL, text=True, start_new_session=True,
            )
            ready, _, _ = select.select([server.stdout], [], [], 30)
            require(bool(ready) and server.stdout.readline().strip() == "server_started" and helpers.leader_running(server), "synthetic server did not start")
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
            """, f"http://127.0.0.1:{port}", ui_grant["workspace_id"], ui_grant["pairing_secret"])
            require(js("return document.querySelector('[name=pairing_secret]').value === ''"), "pairing secret remained in the form")

            def sync_ready():
                return js("""
                    const done=arguments[arguments.length-1];
                    window.__TAURI_INTERNALS__.invoke('m0_renderer_session')
                      .then(owner=>window.__TAURI_INTERNALS__.invoke('workspace_sync_status', {request:{renderer_session_id:owner}}))
                      .then(value=>done(value.status==='ready')).catch(()=>done(false));
                """, asynchronous=True)

            wait(sync_ready, "initial synchronization")
            for node_id, _, contents in fixtures[:2]:
                wait(lambda: js("return !!document.querySelector('[data-document=\"'+arguments[0]+'\"]:not(:disabled)')", node_id), "synthetic document in workspace")
                js("document.querySelector('[data-document=\"'+arguments[0]+'\"]').click()", node_id)
                wait(lambda: js("return document.querySelector('[aria-current=page]')?.dataset.document===arguments[0] && document.querySelector('.cm-content')?.cmTile.root.view.state.doc.toString()===arguments[1]", node_id, contents.decode()), "synthetic document open")
                wait(lambda: node_id in cached_ids(), "synthetic document cache")
            require(cached_ids() == {fixtures[0][0], fixtures[1][0]}, "fixture did not leave a document uncached")

            identity = js("""
                const done=arguments[arguments.length-1];
                window.__TAURI_INTERNALS__.invoke('m0_renderer_session')
                  .then(owner=>window.__TAURI_INTERNALS__.invoke('workspace_sync_status', {request:{renderer_session_id:owner}}))
                  .then(value=>done(value.binding.device_id)).catch(()=>done(null));
            """, asynchronous=True)
            require(isinstance(identity, str), "could not identify the synthetic UI registration")
            require(admin("revoke-device", identity)["kind"] == "done", "synthetic device revoke failed")
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
                return drafts, outbox
            before_drafts, before_outbox = read_state()
            js("document.querySelector('[data-document=\"'+arguments[0]+'\"]').click()", fixtures[0][0])
            wait(lambda: js("return document.querySelector('[aria-current=page]')?.dataset.document===arguments[0] && document.querySelector('.cm-content')?.cmTile.root.view.state.doc.toString()===arguments[1]", fixtures[0][0], fixtures[0][2].decode()), "cached content after device revocation")
            wait(lambda: js("return document.body.textContent.includes('기기 인증 필요')"), "revoked-device status")
            new_contents = fixtures[0][2] + b"local after revoke\n"
            js("const view=document.querySelector('.cm-content').cmTile.root.view; view.dispatch({changes:{from:view.state.doc.length,insert:arguments[0]}})", "local after revoke\n")
            js("document.querySelector('#save').click()")
            wait(lambda: read_state()[0].get(fixtures[0][0], (None,))[0] == new_contents, "local checkpoint after revocation")
            after_drafts, after_outbox = read_state()
            require(after_drafts[fixtures[0][0]][1] == fixtures[0][2], "local edit did not preserve its cached base")
            for document_id, previous in before_drafts.items():
                require(after_drafts.get(document_id) == previous, "revocation changed an unrelated local draft")
            require(after_outbox == before_outbox, "revoked device created or changed an outbox operation")
            for _, name, contents in fixtures:
                require((notes / name).read_bytes() == contents, "revoked device changed a synthetic server file")
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

    print("M3 revoked-device WebView passed: verified local cache, local edit and blocked transmission.", flush=True)


if __name__ == "__main__":
    try:
        main()
    except TestFailure as failure:
        print(f"M3 revoked-device WebView failed: {failure}", file=sys.stderr)
        raise SystemExit(1) from None
