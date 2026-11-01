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
            raw = b'<mxGraphModel><root><mxCell id="0"/><mxCell id="1" parent="0"/></root></mxGraphModel>'
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
            require(not native("drawio_export_png", {"request": {"generation": "1", "png_base64": ""}})["ok"], "main window exported a diagram")
            require(not native("drawio_close")["ok"], "main window closed another session")

            def open_editor(node, check_duplicate=False):
                nonlocal stage
                stage = "opening bundled editor"
                if node["name"].endswith(".drawio"):
                    wait(lambda: js("const button=[...document.querySelectorAll('.markdown-preview button')].find(b=>b.textContent==='XML');if(!button)return false;window.confirm=()=>true;button.click();return true"))
                else:
                    alt = "exported" if node["name"] == "Exported.drawio.png" else "diagram"
                    wait(lambda: js("const button=[...document.querySelectorAll('.preview-image')].find(e=>e.querySelector('img')?.alt===arguments[0])?.querySelector('.diagram-trial');if(!button)return false;window.confirm=()=>true;button.click();return true", alt))
                handles = wait(lambda: (items if len(items := wd("GET", f"/session/{session}/window/handles")) == 2 else None))
                if check_duplicate:
                    js("document.querySelector('.diagram-trial').click()")
                    wait(lambda: js("return document.body.textContent.includes('이미 열린 다이어그램 편집 창을 닫고')"))
                    require(len(wd("GET", f"/session/{session}/window/handles")) == 2, "opened a second editor session")
                wd("POST", f"/session/{session}/window", {"handle": next(item for item in handles if item != main_window)})
                stage = "offline diagram load"
                wait(lambda: js("return document.querySelector('#status')?.textContent.includes('기기 초안을 편집할 수 있습니다.')"), seconds=45)

            open_editor(nodes["Diagram.drawio.png"], check_duplicate=True)
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
            js("document.querySelector('#close').click();document.querySelector('#discard-confirm').click()")
            wait(lambda: len(wd("GET", f"/session/{session}/window/handles")) == 1)
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
            require(js("return document.querySelector('.geDiagramContainer')?.textContent.includes('저장 재시도 확인')"), "saved diagram edit was not restored")
            wd("POST", f"/session/{session}/frame", {"id": None})
            js("document.querySelector('#close').click();document.querySelector('#discard-confirm').click()")
            wait(lambda: len(wd("GET", f"/session/{session}/window/handles")) == 1)
            wd("POST", f"/session/{session}/window", {"handle": main_window})
            open_editor(nodes["Original.drawio"])
            wd("POST", f"/session/{session}/window", {"handle": main_window})
            stage = "renderer reload cleanup"
            wd("POST", f"/session/{session}/refresh", {})
            wait(lambda: len(wd("GET", f"/session/{session}/window/handles")) == 1)
            require(checkpoint() == retried, "renderer reload changed the diagram draft")
            for name, content in originals.items():
                require((root / "notes" / name).read_bytes() == content, "trial editor modified original bytes")
            print("Local draft checkpoint, failure/retry, close/reopen recovery, raw XML load, reload preservation and original bytes passed", flush=True)
            (output / "result.json").write_text(json.dumps({"passed": True, "platform": "Linux WebKitGTK / Xvfb",
                "input": "synthetic graph model change; physical IME not tested", "saving": "local XML draft and editable PNG copy; server source unchanged",
                "export": "canvas failure preserves draft; native chooser, cancellation, no overwrite, embedded XML equality and actual reopen passed",
                "full_app_restart": "not tested; SQLite reopen is covered by store tests"}, indent=2) + "\n")
        except Exception:
            if session:
                try:
                    print("Editor failure state:", js("return {path:location.pathname,status:document.querySelector('#status')?.textContent,frame:document.querySelector('#diagram')?.getAttribute('src')}"), flush=True)
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
