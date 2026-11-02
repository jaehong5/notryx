#!/usr/bin/env python3
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
import struct
import subprocess
import tempfile
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
import zlib


ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("workspace_webdriver", ROOT / "scripts/m2-workspace-webdriver.py")
WORKSPACE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(WORKSPACE)
require, free_port, stop = WORKSPACE.require, WORKSPACE.free_port, WORKSPACE.stop


def diagram_with_large_xml(png):
    chunks = [png[:8]]
    cursor = 8
    enlarged = False
    while cursor < len(png):
        length = struct.unpack(">I", png[cursor:cursor + 4])[0]
        kind = png[cursor + 4:cursor + 8]
        data = png[cursor + 8:cursor + 8 + length]
        if kind == b"tEXt" and data.startswith(b"mxfile\0"):
            xml = urllib.parse.unquote(data[7:].decode()).replace("<mxfile>", "<mxfile>" + " " * 6000)
            require(len(xml.encode()) > 4096, "fixture must exceed the host control-message limit")
            data = b"mxfile\0" + urllib.parse.quote(xml).encode()
            enlarged = True
        chunks.append(struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data)))
        cursor += length + 12
    require(enlarged, "fixture must include diagram XML")
    return b"".join(chunks)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--driver", default="WebKitWebDriver")
    parser.add_argument("--binary", default="target/debug/notryx-client")
    parser.add_argument("--server", default="target/debug/notryx-server")
    parser.add_argument("--output", default=".artifacts/m4-drawio")
    args = parser.parse_args()
    require(WORKSPACE.xvfb_on_current_display(), "run this test on an isolated Xvfb display")
    require(os.environ.get("NOTRYX_TEST_SECRET_BUS_PREFIX"), "use scripts/m3-secret-no-service.sh")
    output = Path(args.output).resolve()
    output.mkdir(parents=True, exist_ok=True)
    server_binary, client_binary = str(Path(args.server).resolve()), str(Path(args.binary).resolve())
    server = driver = None
    session = None
    stage = "startup"

    with tempfile.TemporaryDirectory(prefix="notryx-drawio-") as directory:
        root = Path(directory)
        for name in ("notes", "state", "run", "client", "data", "cache", "config"):
            (root / name).mkdir(mode=0o700)
        server_port, driver_port = free_port(), free_port()
        config = root / "server.toml"
        config.write_text(f'[server]\nlisten = "127.0.0.1:{server_port}"\nadmin_socket = "{root}/run/admin.sock"\n'
                          f'[workspace]\nnotes_root = "{root}/notes"\nstate_root = "{root}/state"\n')
        config.chmod(0o600)
        local = urllib.request.build_opener(urllib.request.ProxyHandler({}))
        log = (output / "driver.log").open("w")

        def cli(*arguments):
            result = subprocess.run([server_binary, *arguments], capture_output=True, timeout=30, check=True)
            return result.stdout.decode()

        def admin(*arguments):
            return json.loads(cli("admin", "--config", str(config), *arguments))

        def server_request(method, path, data=None, headers=None, expected=200):
            connection = http.client.HTTPConnection("127.0.0.1", server_port, timeout=20)
            try:
                connection.request(method, path, data, headers or {})
                response = connection.getresponse()
                require(response.status == expected, "synthetic server request failed")
                return json.loads(response.read())
            finally:
                connection.close()

        def wait(check, seconds=30):
            deadline = time.monotonic() + seconds
            while time.monotonic() < deadline:
                value = check()
                if value:
                    return value
                time.sleep(0.1)
            raise WORKSPACE.TestFailure(f"timed out: {stage}")

        def wd(method, path, payload=None):
            request = urllib.request.Request(f"http://127.0.0.1:{driver_port}" + path, method=method,
                                             data=None if payload is None else json.dumps(payload).encode(),
                                             headers={"Content-Type": "application/json"})
            try:
                with local.open(request, timeout=40) as response:
                    value = json.load(response)["value"]
                if isinstance(value, dict) and "error" in value and "ok" not in value:
                    raise WORKSPACE.TestFailure(f"WebDriver error at {stage}: {value['error']}")
                return value
            except urllib.error.HTTPError as error:
                raise WORKSPACE.TestFailure(f"WebDriver HTTP {error.code} at {stage}") from None

        def js(script, *values, asynchronous=False):
            return wd("POST", f"/session/{session}/execute/{'async' if asynchronous else 'sync'}",
                      {"script": script, "args": list(values)})

        def native(command, arguments=None):
            return js("""const [command,args,done]=arguments;
                window.__TAURI_INTERNALS__.invoke(command,args)
                  .then(value=>done({ok:true,value})).catch(error=>done({ok:false,error:String(error)}));
                """, command, arguments or {}, asynchronous=True)

        def invoke(command, arguments=None):
            result = native(command, arguments)
            require(result["ok"], f"native request failed: {command}")
            return result.get("value")

        try:
            cli("init", "--config", str(config))
            server = subprocess.Popen([server_binary, "serve", "--config", str(config)], stdout=subprocess.PIPE,
                                      stderr=subprocess.DEVNULL, text=True, start_new_session=True)
            ready, _, _ = select.select([server.stdout], [], [], 30)
            require(ready and server.stdout.readline().strip() == "server_started", "synthetic server startup failed")
            status = admin("status")
            grant = admin("issue-pairing", status["owner_principal_id"])
            pair = server_request("POST", "/v1/pair", json.dumps({"protocol": "mdw-sync/1.2", "workspace_id": grant["workspace_id"],
                        "pairing_secret": grant["pairing_secret"]}).encode(), {"Content-Type": "application/json"})
            authorization = {"Authorization": f"Bearer {pair['device_token']}"}
            info = server_request("GET", "/v1/server-info", headers=authorization)
            png = diagram_with_large_xml((ROOT / "crates/client-runtime/tests/fixtures/drawio-valid.png").read_bytes())
            raw = b'<mxGraphModel><root><mxCell id="0"/><mxCell id="1" parent="0"/><mxCell id="2" value="XML original" vertex="1" parent="1"><mxGeometry x="40" y="40" width="120" height="60" as="geometry"/></mxCell></root></mxGraphModel>'
            nodes = {}
            originals = {"Example.md": b"# Synthetic diagram\n\n![diagram](Diagram.drawio.png)\n\n[XML](Original.drawio)\n", "Diagram.drawio.png": png,
                         "Original.drawio": raw,
                         "Export.md": b"# Exported copy\n\n![exported](Exported.drawio.png)\n"}
            for sequence, (name, content) in enumerate(originals.items(), start=1):
                upload = server_request("POST", "/v1/uploads", content, {**authorization, "Content-Type": "application/octet-stream",
                              "Content-Digest": "sha-256=:" + base64.b64encode(hashlib.sha256(content).digest()).decode() + ":"}, 201)
                node_id = WORKSPACE.operation_id()
                server_request("POST", "/v1/nodes", json.dumps({"node_id": node_id, "parent_id": info["root_id"], "name": name,
                     "kind": "file", "upload_id": upload["upload_id"]}).encode(), {**authorization,
                     "Content-Type": "application/json", "X-Workspace-Epoch": info["workspace_epoch"],
                     "X-Device-Stream": pair["device_stream_id"], "X-Operation-Seq": str(sequence),
                     "Idempotency-Key": WORKSPACE.operation_id()})
                nodes[name] = server_request("GET", f"/v1/nodes/{node_id}", headers=authorization)
            ui_grant = admin("issue-pairing", status["owner_principal_id"])
            environment = os.environ.copy()
            environment.update(TAURI_WEBVIEW_AUTOMATION="true", NOTRYX_M0_DATA_DIR=str(root / "client"),
                               XDG_DATA_HOME=str(root / "data"), XDG_CACHE_HOME=str(root / "cache"), XDG_CONFIG_HOME=str(root / "config"))
            driver = subprocess.Popen([args.driver, f"--port={driver_port}", "--host=127.0.0.1"], env=environment,
                                      stdout=log, stderr=log, start_new_session=True)
            wait(lambda: WORKSPACE.driver_owns_port(driver.pid, driver_port))
            session = wd("POST", "/session", {"capabilities": {"alwaysMatch": {"browserName": "wry",
                         "webkitgtk:browserOptions": {"binary": client_binary}}}})["sessionId"]
            wd("POST", f"/session/{session}/timeouts", {"script": 30000})
            stage = "initial screen"
            wait(lambda: js("return !!document.querySelector('.cm-content[contenteditable=true]')"))
            wait(lambda: js("return !!document.querySelector('.workspace-connect button:not(:disabled)')"))
            js("""const form=document.querySelector('.workspace-connect');
                form.elements.namedItem('origin').value=arguments[0];
                form.elements.namedItem('workspace_id').value=arguments[1];
                form.elements.namedItem('pairing_secret').value=arguments[2];
                form.requestSubmit();""", f"http://127.0.0.1:{server_port}", ui_grant["workspace_id"], ui_grant["pairing_secret"])
            wait(lambda: js("return !!document.querySelector('[data-document=\"'+arguments[0]+'\"]:not(:disabled)')", nodes["Example.md"]["node_id"]))
            owner = invoke("m0_renderer_session")
            stage = "initial synchronization"
            wait(lambda: invoke("workspace_sync_status", {"request": {"renderer_session_id": owner}})["status"] == "ready")
            js("document.querySelector('[data-document=\"'+arguments[0]+'\"]').click()", nodes["Example.md"]["node_id"])
            wait(lambda: js("return document.querySelector('.cm-content')?.textContent.includes('Synthetic diagram')"))
            js("document.querySelector('#preview-toggle').click()")
            wait(lambda: js("return !!document.querySelector('.diagram-trial')"))
            js("window.confirm=()=>false;document.querySelector('.diagram-trial').click()")
            require(len(wd("GET", f"/session/{session}/window/handles")) == 1, "cancelled trial opened a diagram")
            main_window = wd("GET", f"/session/{session}/window")
            require(not native("drawio_load")["ok"], "main window read diagram XML")
            require(not native("drawio_checkpoint", {"request": {"generation": "0", "xml": raw.decode()}})["ok"], "main window wrote a diagram draft")
            require(not native("drawio_save", {"request": {"generation": "1"}})["ok"], "main window queued diagram content")
            require(not native("drawio_save_status")["ok"], "main window accessed diagram save status")
            require(not native("drawio_prepare_conflict_remote", {"request": {"generation": "1"}})["ok"], "main window prepared a diagram conflict choice")
            require(not native("drawio_choose_conflict_remote", {"request": {"generation": "1", "operation_id": WORKSPACE.operation_id(), "remote_revision": "invalid"}})["ok"], "main window resolved a diagram conflict")
            require(not native("drawio_export_png", {"request": {"generation": "1", "png_base64": ""}})["ok"], "main window exported a diagram")
            require(not native("drawio_close")["ok"], "main window closed another session")

            def edit_label(value):
                wd("POST", f"/session/{session}/frame", {"id": js("return document.querySelector('#diagram')")})
                require(js("""const original=mxGraph.prototype.dblClick;let graph;
                    mxGraph.prototype.dblClick=function(...args){graph=this;return original.apply(this,args);};
                    const target=document.querySelector('.geDiagramContainer svg text, .geDiagramContainer svg foreignObject');
                    if(!target)return false;
                    const rect=target.getBoundingClientRect();
                    target.dispatchEvent(new MouseEvent('dblclick',{bubbles:true,clientX:rect.x+2,clientY:rect.y+2}));
                    mxGraph.prototype.dblClick=original;
                    if(!graph)return false;graph.stopEditing(true);
                    graph.cellLabelChanged(graph.getModel().getCell('2'),arguments[0]);return true;""", value), "diagram graph edit failed")
                wd("POST", f"/session/{session}/frame", {"id": None})

            def open_editor(node, check_duplicate=False):
                nonlocal stage
                stage = "opening bundled editor"
                if node["name"].endswith(".drawio"):
                    wait(lambda: js("const button=[...document.querySelectorAll('.markdown-preview button')].find(b=>b.textContent==='XML');if(!button)return false;window.confirm=()=>true;button.click();return true"))
                else:
                    alt = "exported" if node["name"] == "Exported.drawio.png" else "diagram"
                    wait(lambda: js("const button=[...document.querySelectorAll('.diagram-trial')].find(b=>b.parentElement?.querySelector('img')?.alt===arguments[0]||(b.textContent==='기기 초안 열기'&&b.parentElement?.textContent.startsWith(arguments[0]+' (')));if(!button)return false;window.confirm=()=>true;button.click();return true", alt))
                handles = wait(lambda: (items if len(items := wd("GET", f"/session/{session}/window/handles")) == 2 else None))
                if check_duplicate:
                    js("document.querySelector('.diagram-trial').click()")
                    wait(lambda: js("return document.body.textContent.includes('이미 열린 다이어그램 편집 창을 닫고')"))
                    require(len(wd("GET", f"/session/{session}/window/handles")) == 2, "opened a second editor session")
                wd("POST", f"/session/{session}/window", {"handle": next(item for item in handles if item != main_window)})
                stage = "offline diagram load"
                wait(lambda: js("return document.querySelector('#status')?.textContent.includes('기기 초안을 편집할 수 있습니다.')"), seconds=45)

            open_editor(nodes["Diagram.drawio.png"], check_duplicate=True)
            require(js("return !document.querySelector('#save-server').hidden"), "PNG source cannot save to server")
            require(not native("drawio_save", {"request": {"generation": "1"}})["ok"], "PNG source accepted XML-only save")
            require(not native("drawio_load")["ok"], "replayed consumed diagram input")
            for command in ("m0_renderer_session", "workspace_inventory", "workspace_local_drafts", "drawio_open"):
                denied = native(command, {"request": {}})
                require(not denied["ok"] and any(word in denied["error"].lower() for word in ("not allowed", "denied", "invalid_caller")),
                        f"editor wrapper accessed main command: {command}")
            require(js("return window.opener === null && window.open('https://example.invalid/') === null"), "editor opened another window")
            frame = js("return document.querySelector('#diagram')")
            wd("POST", f"/session/{session}/frame", {"id": frame})
            stage = "sandbox and diagram editing"
            require(js("try { void parent.document.body; return false; } catch { return true; }"), "diagram could access parent DOM")
            require(js("return !window.__TAURI_INTERNALS__"), "diagram received Tauri IPC")
            require(js("return document.querySelector('.geDiagramContainer')?.textContent.includes('한글')"), "Korean diagram label missing")
            edited = js("""const original=mxGraph.prototype.dblClick;
                let graph;
                mxGraph.prototype.dblClick=function(...args){graph=this;return original.apply(this,args);};
                const target=document.querySelector('.geDiagramContainer svg text, .geDiagramContainer svg foreignObject');
                if (!target) return false;
                const rect=target.getBoundingClientRect();
                target.dispatchEvent(new MouseEvent('dblclick',{bubbles:true,clientX:rect.x+2,clientY:rect.y+2}));
                mxGraph.prototype.dblClick=original;
                if (!graph) return false;
                window.__notryxGraph=graph;
                graph.stopEditing(true);
                graph.cellLabelChanged(graph.getModel().getCell('2'),'한글 수정 확인');
                return graph.getModel().getCell('2').value === '한글 수정 확인';
                """)
            require(edited, "synthetic diagram edit failed")
            wait(lambda: js("return document.querySelector('.geDiagramContainer')?.textContent.includes('한글 수정 확인')"))
            blocked = js("""const done=arguments[0], blocked=[];
                document.addEventListener('securitypolicyviolation',e=>blocked.push(e.effectiveDirective));
                fetch('https://example.invalid/probe').catch(()=>{});
                fetch('http://127.0.0.1:1420/probe').catch(()=>{});
                const image=new Image(); image.src='https://example.invalid/image'; document.body.append(image);
                const frame=document.createElement('iframe'); frame.src='https://example.invalid/frame';document.body.append(frame);
                setTimeout(()=>done(blocked),300);""", asynchronous=True)
            require(all(kind in blocked for kind in ("connect-src", "img-src", "frame-src")) and blocked.count("connect-src") >= 2, "editor allowed external resources")
            wd("POST", f"/session/{session}/frame", {"id": None})
            stage = "durable local draft"
            wait(lambda: js("return document.querySelector('#status')?.textContent.includes('기기에 초안을 저장했습니다.')"))

            def checkpoint():
                with sqlite3.connect(f"file:{root / 'client/drafts.sqlite3'}?mode=ro", uri=True) as db:
                    return db.execute("SELECT generation,xml,base_xml,base_revision FROM diagram_drafts WHERE node_id=?",
                                      (nodes["Diagram.drawio.png"]["node_id"],)).fetchone()

            saved = checkpoint()
            require(saved and saved[0] >= 1 and saved[1] != saved[2], "diagram checkpoint was not persisted")
            editor_window = wd("GET", f"/session/{session}/window")
            wd("POST", f"/session/{session}/window", {"handle": main_window})
            js("document.querySelector('[data-testid=close-document]').click()")
            wait(lambda: js("return !document.querySelector('[data-testid=close-document]')"))
            stage = "open diagram blocks re-registration"
            prior_binding = invoke("workspace_sync_status", {"request": {"renderer_session_id": owner}})["binding"]
            blocked_grant = admin("issue-pairing", status["owner_principal_id"])

            def submit_pair(grant):
                js("""const form=document.querySelector('.workspace-connect');
                    form.elements.namedItem('origin').value=arguments[0];
                    form.elements.namedItem('workspace_id').value=arguments[1];
                    form.elements.namedItem('pairing_secret').value=arguments[2];
                    form.requestSubmit();""", f"http://127.0.0.1:{server_port}", grant["workspace_id"], grant["pairing_secret"])

            submit_pair(blocked_grant)
            wait(lambda: js("return document.body.textContent.includes('열린 문서와 다이어그램 편집 창을 닫은 뒤 다시 연결해 주세요.')"))
            require(invoke("workspace_sync_status", {"request": {"renderer_session_id": owner}})["binding"] == prior_binding,
                    "rejected pairing changed the active registration")
            require(checkpoint() == saved and len(wd("GET", f"/session/{session}/window/handles")) == 2,
                    "rejected pairing changed the draft or closed the editor")
            wd("POST", f"/session/{session}/window", {"handle": editor_window})
            stage = "save failure keeps editor and previous draft"
            with sqlite3.connect(root / "client/drafts.sqlite3") as db:
                db.execute("CREATE TRIGGER hold_diagram_save BEFORE UPDATE ON diagram_drafts BEGIN SELECT RAISE(ABORT,'synthetic checkpoint failure'); END")
            wd("POST", f"/session/{session}/frame", {"id": frame})
            js("const graph=window.__notryxGraph;graph.cellLabelChanged(graph.getModel().getCell('2'),'저장 재시도 확인')")
            wd("POST", f"/session/{session}/frame", {"id": None})
            wait(lambda: js("return document.querySelector('#status')?.textContent.includes('초안을 저장하지 못했습니다.')"))
            js("document.querySelector('#close').click();document.querySelector('#discard-confirm').click()")
            wait(lambda: js("return !document.querySelector('#discard').open && !document.querySelector('#discard-confirm').disabled"))
            require(len(wd("GET", f"/session/{session}/window/handles")) == 2 and checkpoint() == saved,
                    "failed checkpoint closed the editor or overwrote the previous draft")
            with sqlite3.connect(root / "client/drafts.sqlite3") as db:
                db.execute("DROP TRIGGER hold_diagram_save")
            js("document.querySelector('#retry').click()")
            wait(lambda: js("return document.querySelector('#status')?.textContent.includes('기기에 초안을 저장했습니다.')"))
            retried = checkpoint()
            require(retried[0] > saved[0] and retried[1] != saved[1] and retried[2:] == saved[2:], "retry lost the edit or changed its base")
            js("const done=arguments[0];requestAnimationFrame(()=>requestAnimationFrame(()=>done(true)))", asynchronous=True)
            (output / "editor.png").write_bytes(base64.b64decode(wd("GET", f"/session/{session}/screenshot")))
            print("Offline PNG load, Korean synthetic editing and sandbox restrictions passed", flush=True)

            stage = "editable PNG export"
            export_dir = root / "exports"
            export_dir.mkdir(mode=0o700)

            wd("POST", f"/session/{session}/frame", {"id": frame})
            js("window.__savedDataURL=HTMLCanvasElement.prototype.toDataURL;window.__canvasFailed=false;HTMLCanvasElement.prototype.toDataURL=function(){window.__canvasFailed=true;throw new Error('synthetic canvas failure')}")
            wd("POST", f"/session/{session}/frame", {"id": None})
            js("document.querySelector('#export-png').click()")
            wait(lambda: js("return !document.querySelector('#export-png').disabled && document.querySelector('#status').textContent.includes('PNG를 내보내지 못했습니다.')"))
            require(not list(export_dir.iterdir()) and checkpoint()[1:] == retried[1:], "canvas failure lost the saved draft")
            js("document.querySelector('#save-server').click()")
            wait(lambda: js("return !document.querySelector('#save-server').disabled && document.querySelector('#server-status').textContent.includes('서버 저장을 요청하지 못했습니다.')"))
            require(checkpoint()[1:] == retried[1:] and (root / "notes/Diagram.drawio.png").read_bytes() == png,
                    "failed PNG export changed the source or lost the XML draft")
            with sqlite3.connect(f"file:{root / 'client/drafts.sqlite3'}?mode=ro", uri=True) as db:
                require(db.execute("SELECT count(*) FROM client_outbox WHERE document_id=?",
                                   (nodes["Diagram.drawio.png"]["node_id"],)).fetchone()[0] == 0,
                        "PNG export failure queued a server request")
            wd("POST", f"/session/{session}/frame", {"id": frame})
            require(js("return window.__canvasFailed"), "test did not reach the canvas failure")
            js("HTMLCanvasElement.prototype.toDataURL=window.__savedDataURL")
            wd("POST", f"/session/{session}/frame", {"id": None})

            def export(destination):
                failures = []

                def interact():
                    try:
                        WORKSPACE.respond_to_recovery_dialog(destination)
                    except Exception:
                        failures.append(True)

                worker = threading.Thread(target=interact, daemon=True)
                worker.start()
                try:
                    js("document.querySelector('#export-png').click()")
                finally:
                    worker.join(timeout=20)
                require(not worker.is_alive() and not failures, "diagram export dialog interaction failed")
                wait(lambda: js("return !document.querySelector('#export-png').disabled"))

            export(None)
            require(not list(export_dir.iterdir()) and checkpoint()[1:] == retried[1:], "cancelled export changed the draft or wrote a file")
            exported_path = export_dir / "edited.drawio.png"
            export(exported_path)
            require(exported_path.exists(), "editable PNG was not exported")
            exported_png = exported_path.read_bytes()
            require(exported_png.startswith(b"\x89PNG\r\n\x1a\n"), "export is not a PNG")
            cursor, exported_xml = 8, None
            while cursor < len(exported_png):
                length = struct.unpack(">I", exported_png[cursor:cursor + 4])[0]
                kind = exported_png[cursor + 4:cursor + 8]
                data = exported_png[cursor + 8:cursor + 8 + length]
                if kind == b"tEXt" and data.startswith(b"mxfile\0"):
                    exported_xml = urllib.parse.unquote(data[7:].decode())
                cursor += length + 12
            require(exported_xml == checkpoint()[1], "exported PNG did not preserve the checkpoint XML")
            existing = export_dir / "existing.png"
            existing.write_bytes(b"existing synthetic bytes")
            export(existing)
            require(existing.read_bytes() == b"existing synthetic bytes" and checkpoint()[1:] == retried[1:], "failed export changed existing data")
            require(set(export_dir.iterdir()) == {exported_path, existing}, "export left temporary files")
            retried = checkpoint()
            (output / "exported.drawio.png").write_bytes(exported_png)

            stage = "close confirmation"
            js("document.querySelector('#close').click()")
            require(js("return document.querySelector('#discard').open"), "close discarded edits without confirmation")
            js("document.querySelector('#keep').click()")
            require(js("return !document.querySelector('#discard').open"), "keep editing failed")
            wd("POST", f"/session/{session}/frame", {"id": frame})
            active_label = js("""const graph=window.__notryxGraph,cell=graph.getModel().getCell('2');
                graph.startEditingAtCell(cell);
                const input=graph.cellEditor.textarea;
                input.textContent='닫기 직전 입력';
                input.dispatchEvent(new InputEvent('input',{bubbles:true,inputType:'insertText',data:'닫기 직전 입력'}));
                return graph.isEditing() && cell.value === '저장 재시도 확인';""")
            require(active_label, "close fixture must have uncommitted label input")
            wd("POST", f"/session/{session}/frame", {"id": None})
            js("document.querySelector('#close').click();document.querySelector('#discard-confirm').click()")
            wait(lambda: len(wd("GET", f"/session/{session}/window/handles")) == 1)
            closed = checkpoint()
            require(closed[0] > retried[0] and closed[1] != retried[1] and closed[2:] == retried[2:],
                    "closing lost active label input or changed the original base")
            retried = closed
            wd("POST", f"/session/{session}/window", {"handle": main_window})
            stage = "reopen exported PNG"
            upload = server_request("POST", "/v1/uploads", exported_png, {**authorization, "Content-Type": "application/octet-stream",
                "Content-Digest": "sha-256=:" + base64.b64encode(hashlib.sha256(exported_png).digest()).decode() + ":"}, 201)
            exported_id = WORKSPACE.operation_id()
            server_request("POST", "/v1/nodes", json.dumps({"node_id": exported_id, "parent_id": info["root_id"], "name": "Exported.drawio.png",
                "kind": "file", "upload_id": upload["upload_id"]}).encode(), {**authorization, "Content-Type": "application/json",
                "X-Workspace-Epoch": info["workspace_epoch"], "X-Device-Stream": pair["device_stream_id"],
                "X-Operation-Seq": str(len(originals) + 1), "Idempotency-Key": WORKSPACE.operation_id()})
            wait(lambda: js("return !!document.querySelector('[data-document=\"'+arguments[0]+'\"]')", exported_id))
            js("document.querySelector('[data-document=\"'+arguments[0]+'\"]').click()", nodes["Export.md"]["node_id"])
            wait(lambda: js("return document.querySelector('.cm-content')?.textContent.includes('Exported copy')"))
            if not js("return !!document.querySelector('.markdown-preview')"):
                js("document.querySelector('#preview-toggle').click()")
            wait(lambda: js("return !!document.querySelector('.diagram-trial')"))
            open_editor({"name": "Exported.drawio.png"})
            wd("POST", f"/session/{session}/frame", {"id": js("return document.querySelector('#diagram')")})
            require(js("return document.querySelector('.geDiagramContainer')?.textContent.includes('저장 재시도 확인')"), "exported PNG cannot reopen its edited model")
            wd("POST", f"/session/{session}/frame", {"id": None})
            js("document.querySelector('#close').click();document.querySelector('#discard-confirm').click()")
            wait(lambda: len(wd("GET", f"/session/{session}/window/handles")) == 1)
            wd("POST", f"/session/{session}/window", {"handle": main_window})
            js("document.querySelector('[data-document=\"'+arguments[0]+'\"]').click()", nodes["Example.md"]["node_id"])
            wait(lambda: js("return document.querySelector('.cm-content')?.textContent.includes('Synthetic diagram')"))
            if not js("return !!document.querySelector('.markdown-preview')"):
                js("document.querySelector('#preview-toggle').click()")
            wait(lambda: js("return !!document.querySelector('.diagram-trial')"))
            open_editor(nodes["Diagram.drawio.png"])
            require(js("return document.querySelector('#status').textContent.includes('기기 초안을 복구했습니다.')"), "draft recovery was not reported")
            wd("POST", f"/session/{session}/frame", {"id": js("return document.querySelector('#diagram')")})
            require(js("return document.querySelector('.geDiagramContainer')?.textContent.includes('닫기 직전 입력')"), "active label input was not restored after close")
            wd("POST", f"/session/{session}/frame", {"id": None})
            js("document.querySelector('#close').click();document.querySelector('#discard-confirm').click()")
            wait(lambda: len(wd("GET", f"/session/{session}/window/handles")) == 1)
            require(checkpoint()[2:] == retried[2:], "closing the restored draft changed its original base")
            retried = checkpoint()
            wd("POST", f"/session/{session}/window", {"handle": main_window})
            open_editor(nodes["Diagram.drawio.png"])
            stage = "PNG original conditional server save"
            current_draft = checkpoint()
            require(not native("drawio_save", {"request": {"generation": str(current_draft[0]),
                        "png_base64": base64.b64encode(png).decode()}})["ok"],
                    "PNG save accepted editing data different from the durable checkpoint")
            require(checkpoint() == current_draft and (root / "notes/Diagram.drawio.png").read_bytes() == png,
                    "rejected PNG proposal changed the draft or server source")
            js("document.querySelector('#save-server').click()")
            wait(lambda: js("return document.querySelector('#server-status').textContent==='서버에 저장했습니다.' || document.querySelector('#server-status').textContent.includes('요청하지 못했습니다')"), seconds=45)
            require(js("return document.querySelector('#server-status').textContent==='서버에 저장했습니다.'"), "PNG server save failed")
            saved_png_draft = checkpoint()
            saved_png = (root / "notes/Diagram.drawio.png").read_bytes()
            require(saved_png.startswith(b"\x89PNG\r\n\x1a\n") and saved_png != png, "PNG save converted format or retained old bytes")
            require(saved_png_draft[1] == saved_png_draft[2] and saved_png_draft[3] != retried[3], "PNG receipt did not update XML base")
            with sqlite3.connect(f"file:{root / 'client/drafts.sqlite3'}?mode=ro", uri=True) as db:
                payload = db.execute("SELECT bytes FROM client_outbox WHERE document_id=? ORDER BY operation_seq DESC LIMIT 1",
                                     (nodes["Diagram.drawio.png"]["node_id"],)).fetchone()[0]
            require(payload == saved_png, "PNG bytes changed between durable queue and server")
            js("document.querySelector('#close').click();document.querySelector('#discard-confirm').click()")
            wait(lambda: len(wd("GET", f"/session/{session}/window/handles")) == 1)
            wd("POST", f"/session/{session}/window", {"handle": main_window})
            stage = "PNG device draft reopen without server"
            stop(server)
            server = None
            js("document.querySelector('#preview-toggle').click()")
            wait(lambda: js("return !document.querySelector('.markdown-preview')"))
            js("document.querySelector('#preview-toggle').click()")
            wait(lambda: js("return [...document.querySelectorAll('.diagram-trial')].some(b=>b.textContent==='기기 초안 열기')"))
            open_editor(nodes["Diagram.drawio.png"])
            require(js("return document.querySelector('#status').textContent.includes('기기 초안을 복구했습니다.')"), "offline PNG draft recovery was not reported")
            require(checkpoint()[1:] == saved_png_draft[1:], "offline PNG reopen changed the saved draft or base")
            offline_copy = export_dir / "offline.drawio.png"
            export(offline_copy)
            require(offline_copy.read_bytes().startswith(b"\x89PNG\r\n\x1a\n") and checkpoint()[2:] == saved_png_draft[2:]
                    and (root / "notes/Diagram.drawio.png").read_bytes() == saved_png,
                    "offline export lost the base or changed the server original")
            server = subprocess.Popen([server_binary, "serve", "--config", str(config)], stdout=subprocess.PIPE,
                                      stderr=subprocess.DEVNULL, text=True, start_new_session=True)
            ready, _, _ = select.select([server.stdout], [], [], 30)
            require(ready and server.stdout.readline().strip() == "server_started", "synthetic server restart failed")
            wait(lambda: js("return document.querySelector('#server-status').textContent==='서버에 저장했습니다.'"))
            wd("POST", f"/session/{session}/frame", {"id": js("return document.querySelector('#diagram')")})
            require(js("return document.querySelector('.geDiagramContainer')?.textContent.includes('닫기 직전 입력')"), "saved PNG could not reopen its editing data")
            wd("POST", f"/session/{session}/frame", {"id": None})
            stage = "revoked authentication preserves diagram edits"
            require(admin("revoke-device", prior_binding["device_id"])["kind"] == "done", "synthetic device revoke failed")
            edit_label("인증 확인 전 편집")
            js("document.querySelector('#save-server').click()")
            wait(lambda: js("return document.querySelector('#server-status').textContent.includes('메인 창에서 다시 연결')"))
            denied_draft = checkpoint()
            require("인증 확인 전 편집" in denied_draft[1] and denied_draft[2:] == saved_png_draft[2:],
                    "authentication rejection changed the base or lost the draft")
            require((root / "notes/Diagram.drawio.png").read_bytes() == saved_png, "authentication rejection changed server PNG")
            with sqlite3.connect(f"file:{root / 'client/drafts.sqlite3'}?mode=ro", uri=True) as db:
                pending = db.execute("SELECT bytes,status FROM client_outbox WHERE document_id=? ORDER BY operation_seq DESC LIMIT 1",
                                     (nodes["Diagram.drawio.png"]["node_id"],)).fetchone()
            require(pending and pending[0].startswith(b"\x89PNG\r\n\x1a\n") and pending[1] in ("queued", "sent"),
                    "authentication rejection discarded the queued PNG")
            edit_label("인증 확인 중 추가 편집")
            wait(lambda: checkpoint()[0] > denied_draft[0] and "인증 확인 중 추가 편집" in checkpoint()[1])
            require(js("return document.querySelector('#server-status').textContent.includes('메인 창에서 다시 연결')"),
                    "local editing hid authentication recovery guidance")
            js("document.querySelector('#close').click();document.querySelector('#discard-confirm').click()")
            wait(lambda: len(wd("GET", f"/session/{session}/window/handles")) == 1)
            retried = checkpoint()
            wd("POST", f"/session/{session}/window", {"handle": main_window})
            stage = "re-registration after closing the editor"
            wait(lambda: js("return !!document.querySelector('.workspace-connect button:not(:disabled)')"))
            submit_pair(admin("issue-pairing", status["owner_principal_id"]))
            wait(lambda: invoke("workspace_sync_status", {"request": {"renderer_session_id": owner}})["binding"] not in (None, prior_binding))
            require(checkpoint() == retried, "successful re-registration discarded the old diagram draft")
            wait(lambda: js("const button=document.querySelector('[data-document=\"'+arguments[0]+'\"]:not(:disabled)');if(!button)return false;button.click();return true", nodes["Example.md"]["node_id"]))
            wait(lambda: js("return document.querySelector('.cm-content')?.textContent.includes('Synthetic diagram')"))
            if not js("return !!document.querySelector('.markdown-preview')"):
                js("document.querySelector('#preview-toggle').click()")
            wait(lambda: js("return !!document.querySelector('.diagram-trial')"))
            open_editor(nodes["Original.drawio"])
            stage = "XML conditional server save"
            require(js("return !document.querySelector('#save-server').hidden"), "XML source cannot save to server")
            def xml_checkpoint():
                with sqlite3.connect(f"file:{root / 'client/drafts.sqlite3'}?mode=ro", uri=True) as db:
                    return db.execute("SELECT generation,xml,base_xml,base_revision FROM diagram_drafts WHERE node_id=?",
                                      (nodes["Original.drawio"]["node_id"],)).fetchone()

            edit_label("XML server saved")
            stop(server)
            server = None
            js("document.querySelector('#save-server').click()")
            wait(lambda: js("return document.querySelector('#server-status').textContent.includes('서버 전송 대기')"))
            pending_xml = xml_checkpoint()
            require("XML server saved" in pending_xml[1] and (root / "notes/Original.drawio").read_bytes() == raw,
                    "offline save did not preserve the local draft and unchanged server file")
            with sqlite3.connect(f"file:{root / 'client/drafts.sqlite3'}?mode=ro", uri=True) as db:
                pending = db.execute("SELECT bytes,status FROM client_outbox WHERE document_id=? ORDER BY operation_seq DESC LIMIT 1",
                                     (nodes["Original.drawio"]["node_id"],)).fetchone()
            require(pending and pending[0] == pending_xml[1].encode() and pending[1] in ("queued", "sent"),
                    "offline save was not durably queued")
            stage = "XML device draft reopen without server"
            js("document.querySelector('#close').click();document.querySelector('#discard-confirm').click()")
            wait(lambda: len(wd("GET", f"/session/{session}/window/handles")) == 1)
            wd("POST", f"/session/{session}/window", {"handle": main_window})
            open_editor(nodes["Original.drawio"])
            require(js("return document.querySelector('#status').textContent.includes('기기 초안을 복구했습니다.')"), "offline XML draft recovery was not reported")
            require(xml_checkpoint()[1:] == pending_xml[1:], "offline XML reopen lost the draft or base")
            edit_label("XML edited after offline reopen")
            wait(lambda: "XML edited after offline reopen" in xml_checkpoint()[1])
            require(xml_checkpoint()[2:] == pending_xml[2:], "offline XML edit changed its original base")
            server = subprocess.Popen([server_binary, "serve", "--config", str(config)], stdout=subprocess.PIPE,
                                      stderr=subprocess.DEVNULL, text=True, start_new_session=True)
            ready, _, _ = select.select([server.stdout], [], [], 30)
            require(ready and server.stdout.readline().strip() == "server_started", "synthetic server restart failed")
            stage = "XML save after reconnect"
            wait(lambda: js("return document.querySelector('#server-status').textContent.startsWith('이전 저장 요청은 서버에 반영됐습니다.')"), seconds=60)
            require("XML edited after offline reopen" in xml_checkpoint()[1], "late ACK replaced the offline edit")
            js("document.querySelector('#save-server').click()")
            wait(lambda: js("return document.querySelector('#server-status').textContent==='서버에 저장했습니다.'"), seconds=60)
            saved_xml = xml_checkpoint()
            require(saved_xml[1] == saved_xml[2] and saved_xml[3] != nodes["Original.drawio"]["content_rev"], "XML receipt did not advance its base")
            require((root / "notes/Original.drawio").read_bytes() == saved_xml[1].encode(), "server did not preserve XML checkpoint bytes")
            js("document.querySelector('#close').click();document.querySelector('#discard-confirm').click()")
            wait(lambda: len(wd("GET", f"/session/{session}/window/handles")) == 1)
            wd("POST", f"/session/{session}/window", {"handle": main_window})
            open_editor(nodes["Original.drawio"])
            wait(lambda: js("return document.querySelector('#server-status').textContent==='서버에 저장했습니다.'"))
            stage = "XML concurrent server edit preserves both copies"
            edit_label("XML local conflict")
            current = server_request("GET", f"/v1/nodes/{nodes['Original.drawio']['node_id']}", headers=authorization)
            remote_xml = raw.replace(b"XML original", b"XML remote edit")
            server_request("PUT", f"/v1/nodes/{current['node_id']}/content", remote_xml, {**authorization,
                "Content-Type": "application/octet-stream", "If-Match": current["content_etag"],
                "Content-Digest": "sha-256=:" + base64.b64encode(hashlib.sha256(remote_xml).digest()).decode() + ":",
                "X-Workspace-Epoch": info["workspace_epoch"], "X-Device-Stream": pair["device_stream_id"],
                "X-Operation-Seq": str(len(originals) + 2), "Idempotency-Key": WORKSPACE.operation_id()})
            js("document.querySelector('#save-server').click()")
            wait(lambda: js("return document.querySelector('#server-status').textContent.includes('서버 원본이 바뀌어')"))
            conflict_xml = xml_checkpoint()
            require("XML local conflict" in conflict_xml[1] and conflict_xml[2:] == saved_xml[2:], "conflict discarded local XML or changed base")
            require((root / "notes/Original.drawio").read_bytes() == remote_xml, "conflict overwrote remote XML")
            stage = "diagram conflict requires a durable local PNG copy"
            require(js("return document.querySelector('#choose-remote').disabled"), "remote choice was enabled before saving a copy")
            require(not native("drawio_prepare_conflict_remote", {"request": {"generation": str(conflict_xml[0])}})["ok"], "native accepted remote choice without a copy")
            export(None)
            require(js("return document.querySelector('#choose-remote').disabled"), "cancelled export allowed conflict resolution")
            export(existing)
            require(js("return document.querySelector('#choose-remote').disabled"), "failed export allowed conflict resolution")
            require(xml_checkpoint()[1:] == conflict_xml[1:] and (root / "notes/Original.drawio").read_bytes() == remote_xml,
                    "unsuccessful conflict export lost a draft or changed the server")
            conflict_copy = export_dir / "conflict.drawio.png"
            export(conflict_copy)
            require(conflict_copy.exists(), "conflict copy was not saved")
            wait(lambda: js("return !document.querySelector('#choose-remote').disabled"))
            require(invoke("drawio_save_status")["status"] == "conflict", "copy export silently resolved the conflict")
            copied = xml_checkpoint()
            js("document.querySelector('#choose-remote').click()")
            wait(lambda: js("return document.querySelector('#conflict-remote').open"))
            js("document.querySelector('#remote-cancel').click()")
            require(xml_checkpoint()[1:] == copied[1:] and (root / "notes/Original.drawio").read_bytes() == remote_xml,
                    "cancelled remote choice changed either copy")
            stage = "diagram conflict rejects changed remote content"
            js("document.querySelector('#choose-remote').click()")
            wait(lambda: js("return document.querySelector('#conflict-remote').open"))
            newer = server_request("GET", f"/v1/nodes/{current['node_id']}", headers=authorization)
            newer_xml = raw.replace(b"XML original", b"XML newer remote")
            server_request("PUT", f"/v1/nodes/{current['node_id']}/content", newer_xml, {**authorization,
                "Content-Type": "application/octet-stream", "If-Match": newer["content_etag"],
                "Content-Digest": "sha-256=:" + base64.b64encode(hashlib.sha256(newer_xml).digest()).decode() + ":",
                "X-Workspace-Epoch": info["workspace_epoch"], "X-Device-Stream": pair["device_stream_id"],
                "X-Operation-Seq": str(len(originals) + 3), "Idempotency-Key": WORKSPACE.operation_id()})
            js("document.querySelector('#remote-confirm').click()")
            wait(lambda: js("return !document.querySelector('#conflict-remote').open && !document.querySelector('#export-png').disabled"))
            require(xml_checkpoint()[1:] == copied[1:] and (root / "notes/Original.drawio").read_bytes() == newer_xml,
                    "stale confirmation discarded local or remote data")
            require(invoke("drawio_save_status")["status"] == "conflict", "stale confirmation resolved the conflict")
            stage = "diagram conflict remote choice resumes editing"
            wait(lambda: js("return !document.querySelector('#choose-remote').disabled"))
            js("document.querySelector('#choose-remote').click()")
            wait(lambda: js("return document.querySelector('#conflict-remote').open"))
            js("document.querySelector('#remote-confirm').click()")
            wait(lambda: js("return !document.querySelector('#save-server').disabled && document.querySelector('#status').textContent.includes('서버 원본을 다시 불러왔습니다.')"))
            restored_xml = xml_checkpoint()
            require(restored_xml[1] == restored_xml[2] and "XML newer remote" in restored_xml[1], "remote choice did not update the durable draft and base")
            require((root / "notes/Original.drawio").read_bytes() == newer_xml, "remote choice wrote the server source")
            with sqlite3.connect(f"file:{root / 'client/drafts.sqlite3'}?mode=ro", uri=True) as db:
                require(db.execute("SELECT count(*) FROM client_outbox WHERE document_id=? AND status='conflict'",
                                   (nodes["Original.drawio"]["node_id"],)).fetchone()[0] == 1,
                        "remote choice changed the historical conflict receipt")
            edit_label("XML editing resumed")
            js("document.querySelector('#save-server').click()")
            wait(lambda: js("return document.querySelector('#server-status').textContent==='서버에 저장했습니다.'"), seconds=45)
            conflict_xml = xml_checkpoint()
            require("XML editing resumed" in conflict_xml[1] and conflict_xml[1] == conflict_xml[2]
                    and (root / "notes/Original.drawio").read_bytes() == conflict_xml[1].encode(), "saving after conflict resolution did not resume")
            wd("POST", f"/session/{session}/window", {"handle": main_window})
            stage = "renderer reload cleanup"
            previous_owner = invoke("m0_renderer_session")
            # Reload destroys a second webview; avoid WebDriver's navigation wait and verify completion below.
            js("setTimeout(() => location.reload(), 0)")
            wait(lambda: len(wd("GET", f"/session/{session}/window/handles")) == 1)
            wait(lambda: js("return document.readyState === 'complete' && !!window.__TAURI_INTERNALS__"))
            require(invoke("m0_renderer_session") != previous_owner, "renderer reload did not replace its owner")
            require(checkpoint() == retried, "renderer reload changed the diagram draft")
            require(xml_checkpoint() == conflict_xml, "renderer reload changed the conflicting XML draft")
            for name, content in originals.items():
                if name == "Original.drawio":
                    continue
                if name == "Diagram.drawio.png":
                    content = saved_png
                require((root / "notes" / name).read_bytes() == content, "trial editor modified original bytes")
            print("Local draft recovery, PNG export and original save/reopen, XML save/reconnect, conflict preservation and exact server bytes passed", flush=True)
            (output / "result.json").write_text(json.dumps({"passed": True, "platform": "Linux WebKitGTK / Xvfb",
                "input": "synthetic graph model change; physical IME not tested", "saving": "PNG original exact-byte save/reopen; XML save after offline queue/reconnect and concurrent conflict preservation",
                "export": "canvas failure preserves draft; native chooser, cancellation, no overwrite, embedded XML equality and actual reopen passed",
                "authentication": "revoked device stops transfer, shows reconnection guidance and preserves queued PNG plus later local edits",
                "conflict_resolution": "local PNG copy required; cancellation, failed export and changed remote preserve both copies; explicit remote choice and later conditional save passed",
                "close": "active label input checkpointed before closing and recovered on reopen",
                "offline_reopen": "stopped server: PNG draft opened from failed preview and exported; XML draft reopened, edited and saved after reconnect without losing its base or pending request",
                "re_registration": "blocked while editor is open; allowed after close and old draft remains",
                "full_app_restart": "not tested; SQLite reopen is covered by store tests"}, indent=2) + "\n")
        except Exception:
            print("Failure stage:", stage, flush=True)
            if session:
                try:
                    print("Editor failure state:", js("return {path:location.pathname,status:document.querySelector('#status')?.textContent,server:document.querySelector('#server-status')?.textContent,frame:document.querySelector('#diagram')?.getAttribute('src')}"), flush=True)
                    (output / "failure.png").write_bytes(base64.b64decode(wd("GET", f"/session/{session}/screenshot")))
                except (OSError, WORKSPACE.TestFailure):
                    pass
            raise
        finally:
            if session:
                try:
                    wd("DELETE", f"/session/{session}")
                except (OSError, WORKSPACE.TestFailure):
                    pass
            stop(driver)
            stop(server)
            log.close()


if __name__ == "__main__":
    main()
