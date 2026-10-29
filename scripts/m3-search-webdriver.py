#!/usr/bin/env python3
"""Exercise local text search in an isolated Linux WebView with synthetic data."""

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

    with tempfile.TemporaryDirectory(prefix="notryx-m3-search-webdriver-") as temporary:
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
            """, "가나")
            wait(lambda: js("const b=document.querySelector('[aria-label=\"본문 검색\"] button[type=submit]'); return !!b && !b.disabled"), "search form")

            def search_diagnostic():
                return js("""
                    const done=arguments[arguments.length-1];
                    const panel=document.querySelector('[aria-label="본문 검색"]');
                    const input=panel?.querySelector('input');
                    const scope=panel?.querySelector('select')?.value;
                    const statuses=[...(panel?.querySelectorAll('[role=status]') || [])];
                    const submit=panel?.querySelector('button[type=submit]');
                    const snapshot={
                      panelPresent:!!panel,queryPresent:!!input?.value,
                      queryBytes:input ? new TextEncoder().encode(input.value).length : null,
                      scope:['workspace','recovery'].includes(scope) ? scope : scope==='' ? 'unselected' : 'other',
                      submitDisabled:submit ? submit.disabled : null,
                      busy:statuses.some(item=>item.textContent==='검색 중'),
                      failureNotice:statuses.some(item=>item.textContent.includes('검색을 완료하지 못했습니다')),
                      hasCoverage:!!panel && [...panel.querySelectorAll('p')].some(item=>item.textContent.includes('이번 구간에서 검색 가능한 문서:')),
                      resultCount:panel?.querySelectorAll('[data-search-node]').length ?? 0,
                      nextEnabled:!!panel && [...panel.querySelectorAll('button')].some(item=>item.textContent.trim()==='다음 결과' && !item.disabled),
                      syncStatus:'unavailable',
                    };
                    const status=window.__TAURI_INTERNALS__.invoke('m0_renderer_session')
                      .then(owner=>window.__TAURI_INTERNALS__.invoke('workspace_sync_status',{request:{renderer_session_id:owner}}))
                      .then(value=>['connecting','ready','offline','auth_required','gateway_auth_required','needs_review'].includes(value.status) ? value.status : 'other')
                      .catch(()=>'unavailable');
                    Promise.race([status,new Promise(resolve=>setTimeout(()=>resolve('timeout'),2000))])
                      .then(value=>{snapshot.syncStatus=value;done(snapshot);});
                """, asynchronous=True)

            def search_page():
                return js("""
                    const panel=document.querySelector('[aria-label="본문 검색"]');
                    if(!panel) return null;
                    const statuses=[...panel.querySelectorAll('[role=status]')];
                    const coverage=[...panel.querySelectorAll('p')].find(item=>item.textContent.startsWith('이번 구간에서 검색 가능한 문서:'));
                    const next=[...panel.querySelectorAll('button')].find(item=>item.textContent.trim()==='다음 결과');
                    return {busy:statuses.some(item=>item.textContent==='검색 중'),
                      failed:statuses.some(item=>item.textContent.includes('검색을 완료하지 못했습니다')),
                      coverage:coverage?.textContent ?? null,
                      results:[...panel.querySelectorAll('[data-search-node]')].map(item=>item.dataset.searchNode),
                      next:!!next && !next.disabled};
                """)

            def find_cached_match():
                js("document.querySelector('[aria-label=\"본문 검색\"] form').requestSubmit()")
                previous = None
                for _ in range(20):
                    def changed_page():
                        page = search_page()
                        if page and page["failed"]:
                            raise TestFailure("search UI reported an error")
                        if page and not page["busy"] and page["coverage"] is not None and page != previous:
                            return page
                        return None
                    try:
                        page = wait(changed_page, "search results render")
                    except TestFailure:
                        print("Search diagnostic:", json.dumps(search_diagnostic(), ensure_ascii=True), file=sys.stderr, flush=True)
                        raise
                    if fixtures[0][0] in page["results"]:
                        return
                    require(page["next"], "cached Korean match was not found")
                    previous = page
                    js("[...document.querySelectorAll('[aria-label=\"본문 검색\"] button')].find(b=>b.textContent.trim()==='다음 결과').click()")
                raise TestFailure("cached Korean match exceeded search page limit")

            find_cached_match()
            require(not js("return !!document.querySelector('[data-search-node=\"'+arguments[0]+'\"]')", fixtures[2][0]), "uncached document appeared in search")
            require(js("return [...document.querySelectorAll('[aria-label=\"본문 검색\"] p')].some(p=>p.textContent.includes('이번 구간에서 검색 가능한 문서:'))"), "search coverage was not shown")
            js("[...document.querySelectorAll('[aria-label=\"본문 검색\"] button')].find(b=>b.textContent.trim()==='검색 취소').click()")
            wait(lambda: js("const p=document.querySelector('[aria-label=\"본문 검색\"]'); return !!p && !p.querySelector('[data-search-node]') && [...p.querySelectorAll('[role=status]')].some(e=>e.textContent==='검색을 취소했습니다.')"), "cancelled search UI")
            find_cached_match()
            js("document.querySelector('[data-search-node=\"'+arguments[0]+'\"]').click()", fixtures[0][0])
            wait(lambda: js("return document.querySelector('[aria-current=page]')?.dataset.document===arguments[0] && document.querySelector('.cm-content')?.cmTile.root.view.state.doc.toString()===arguments[1]", fixtures[0][0], fixtures[0][2].decode()), "search result opening")

            js("[...document.querySelectorAll('button')].find(b=>b.textContent.trim()==='본문 검색' && b.getAttribute('aria-expanded')==='true')?.click()")
            wait(lambda: js("return !document.querySelector('[aria-label=\"본문 검색\"]')"), "search panel close")

            # The command uses the same session cursor until all slices are accounted for.
            coverage = js("""
                const done=arguments[arguments.length-1];
                (async()=>{
                  const invoke=window.__TAURI_INTERNALS__.invoke;
                  const owner=await invoke('m0_renderer_session');
                  const sync=await invoke('workspace_sync_status',{request:{renderer_session_id:owner}});
                  const requestId='9007199254740991';
                  let after=null, available=0, total=0, results=[];
                  for(let i=0;i<100;i++) {
                    const page=await invoke('workspace_search',{request:{renderer_session_id:owner,binding:sync.binding,request_id:requestId,query:'가나',scope:'workspace',after_id:after}});
                    available+=page.available_documents; total+=page.total_documents;
                    results.push(...page.results.map(result=>result.node_id));
                    after=page.next_after_id;
                    if(after===null) {
                      await invoke('workspace_cancel_search',{request:{renderer_session_id:owner,binding:sync.binding,request_id:requestId}});
                      return {available,total,results,cancelled:true};
                    }
                  }
                  return {error:'too many pages'};
                })().then(done).catch(()=>done({error:'search failed'}));
            """, asynchronous=True)
            require(coverage == {"available": 2, "total": 3, "results": [fixtures[0][0]], "cancelled": True}, "search coverage or Korean match was incorrect")
            require(cached_ids() == {fixtures[0][0], fixtures[1][0]}, "search changed the cache coverage")
            for _, name, contents in fixtures:
                require((notes / name).read_bytes() == contents, "search changed a synthetic source")
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

    print("M3 search WebView passed: local Korean search, partial coverage and result opening.", flush=True)


if __name__ == "__main__":
    try:
        main()
    except TestFailure as failure:
        print(f"M3 search WebView failed: {failure}", file=sys.stderr)
        raise SystemExit(1) from None
