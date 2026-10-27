#!/usr/bin/env python3
import argparse
import base64
import http.server
import json
import os
import re
from pathlib import Path
import signal
import socket
import sqlite3
import subprocess
import tempfile
import threading
from concurrent.futures import ThreadPoolExecutor
import time
import urllib.error
import urllib.request


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--driver", default="WebKitWebDriver")
    parser.add_argument("--binary", default="target/debug/notryx-client")
    parser.add_argument("--output", default=".artifacts/m0-webview")
    parser.add_argument("--measure", action="store_true")
    args = parser.parse_args()
    output = Path(args.output).resolve()
    output.mkdir(parents=True, exist_ok=True)
    directory = tempfile.TemporaryDirectory(prefix="notryx-m0-webview-")
    environment = os.environ.copy()
    environment.update(TAURI_WEBVIEW_AUTOMATION="true", NOTRYX_M0_DATA_DIR=directory.name)
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        port = sock.getsockname()[1]
    origin = f"http://127.0.0.1:{port}"
    log = (output / "driver.log").open("w")
    process = subprocess.Popen([args.driver, f"--port={port}", "--host=127.0.0.1"], env=environment, stdout=log, stderr=log, start_new_session=True)
    session = None

    def request(method, path, payload=None):
        data = None if payload is None else json.dumps(payload).encode()
        req = urllib.request.Request(origin + path, data=data, method=method, headers={"Content-Type": "application/json"})
        try:
            with urllib.request.urlopen(req, timeout=60) as response:
                result = json.load(response)
        except urllib.error.HTTPError as error:
            raise RuntimeError(error.read().decode()) from error
        if isinstance(result.get("value"), dict) and "error" in result["value"]:
            raise RuntimeError(result["value"])
        return result["value"]

    def wait(check, seconds=15):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            result = check()
            if result:
                return result
            time.sleep(0.1)
        raise AssertionError("Timed out waiting for expected WebView state")

    def js(script, *values, asynchronous=False):
        endpoint = "async" if asynchronous else "sync"
        return request("POST", f"/session/{session}/execute/{endpoint}", {"script": script, "args": list(values)})

    def native_result(command, arguments=None):
        return js("""
          const [command,args,done]=arguments;
          window.__TAURI_INTERNALS__.invoke(command,args)
            .then(value=>done({ok:true,value}))
            .catch(error=>done({ok:false,failure:String(error)}));
        """, command, arguments or {}, asynchronous=True)

    def native(command, arguments=None):
        result = native_result(command, arguments)
        assert result["ok"], result
        return result.get("value")

    def click(selector):
        js("document.querySelector(arguments[0]).click()", selector)

    def open_document(document_id):
        wait(lambda: js("""
            const button = document.querySelector('[data-document="' + arguments[0] + '"]');
            if (!button || button.disabled) return false;
            button.click();
            return true;
        """, document_id))
        wait(lambda: js("return document.querySelector('[aria-current=page]')?.dataset.document === arguments[0]", document_id))
        assert not js("return document.querySelector('[role=alert]')?.textContent")

    def checkpoint(document_id):
        path = Path(directory.name) / "drafts.sqlite3"
        with sqlite3.connect(f"file:{path}?mode=ro", uri=True) as db:
            return db.execute("SELECT generation, bytes FROM draft_checkpoints WHERE document_id = ?", (document_id,)).fetchone()

    try:
        for _ in range(100):
            try:
                request("GET", "/status")
                break
            except (OSError, RuntimeError):
                if process.poll() is not None:
                    raise RuntimeError("WebKitWebDriver exited; see driver.log")
                time.sleep(0.1)
        capabilities = {"capabilities": {"alwaysMatch": {"browserName": "wry", "webkitgtk:browserOptions": {"binary": str(Path(args.binary).resolve())}}}}
        session = request("POST", "/session", capabilities)["sessionId"]
        request("POST", f"/session/{session}/timeouts", {"script": 30000})
        wait(lambda: js("return !!document.querySelector('.cm-content[contenteditable=true]')"))
        print("Linux WebView editor opened", flush=True)
        evidence = {"webview": "WebKitGTK", "display": "Xvfb", "tests": []}

        for document_id in ("unicode", "mixed", "non-utf8"):
            open_document(document_id)
            expected_editable = "false" if document_id == "non-utf8" else "true"
            assert js("return document.querySelector('.cm-content').getAttribute('contenteditable')") == expected_editable
            click("#save")
            time.sleep(0.3)
            assert checkpoint(document_id) is None
        evidence["tests"].append("unchanged fixtures do not create rewritten checkpoints; CRLF and mixed UTF-8 are editable; non-UTF-8 remains read-only")

        open_document("unicode")
        text = "한글e\u0301 시험"
        element = request("POST", f"/session/{session}/element", {"using": "css selector", "value": ".cm-content"})
        element_id = element["element-6066-11e4-a52e-4f735466cecf"]
        request("POST", f"/session/{session}/element/{element_id}/value", {"text": text})
        js("document.querySelector('.cm-content').focus(); document.execCommand('insertText',false,'EMOJI😀PASTE')")
        wait(lambda: (saved := checkpoint("unicode")) is not None and text.encode() in saved[1] and "EMOJI😀PASTE".encode() in saved[1])
        saved = checkpoint("unicode")[1]
        assert saved.startswith(b"\xef\xbb\xbf")
        assert text.encode() in saved
        assert "EMOJI😀PASTE".encode() in saved
        assert b"\n" in saved and b"\r" not in saved
        assert b"custom: preserve\n---\n" in saved
        assert "```unknown\n[[합성 문서]]\n```".encode() in saved
        assert b"<!-- keep -->" in saved
        evidence["tests"].append("actual editor input saves LF and retains BOM, Korean, emoji, combining marks and unknown syntax")
        evidence["input_scope"] = "WebKit send_keys dropped non-BMP emoji; Korean/combining keys and emoji insertText were tested separately. Physical IME remains untested."

        open_document("mixed")
        js("const view=document.querySelector('.cm-content').cmTile.root.view; view.dispatch({changes:{from:view.state.doc.length,insert:'혼합 입력😀'}})")
        wait(lambda: checkpoint("mixed") is not None)
        assert checkpoint("mixed")[1] == "# 혼합 줄바꿈\n원본\n보존\n혼합 입력😀".encode()
        evidence["tests"].append("editing a mixed-newline UTF-8 document saves LF without changing the existing text")

        open_document("welcome")
        with ThreadPoolExecutor(max_workers=1) as executor:
            continuous = executor.submit(js, """
              const done=arguments[arguments.length-1];
              const view=document.querySelector('.cm-content').cmTile.root.view;
              let n=0;
              const timer=setInterval(() => {
                view.dispatch({changes:{from:0,to:1,insert:n%2?'a':'b'}});
                if(++n===40){clearInterval(timer);done(n);}
              },50);
            """, asynchronous=True)
            deadline = time.monotonic() + 1.8
            during_typing = None
            while time.monotonic() < deadline and not continuous.done():
                during_typing = checkpoint("welcome")
                if during_typing:
                    break
                time.sleep(0.02)
            assert during_typing and not continuous.done(), "continuous input delayed persistence until typing stopped"
            assert continuous.result() == 40
        wait(lambda: checkpoint("welcome")[0] == 40)
        evidence["tests"].append("continuous 50ms edits commit while typing; final generation 40 becomes durable")

        durable_before_reload = checkpoint("welcome")
        previous_owner = native("m0_renderer_session")
        previous_session = native("m0_open", {"request": {"document_id": "100k-b", "renderer_session_id": previous_owner}})
        assert checkpoint("100k-b") is None
        request("POST", f"/session/{session}/refresh", {})
        wait(lambda: js("return document.querySelector('[aria-current=page]')?.dataset.document === 'welcome' && !!document.querySelector('.cm-content[contenteditable=true]')"))
        assert not js("return document.querySelector('[role=alert]')?.textContent")
        current_owner = native("m0_renderer_session")
        assert current_owner != previous_owner
        restored_text = js("return document.querySelector('.cm-content').cmTile.root.view.state.doc.toString()")
        assert restored_text.encode() == durable_before_reload[1]
        assert checkpoint("welcome") == durable_before_reload

        stale_requests = [
            ("m0_open", {"document_id": "100k-b", "renderer_session_id": previous_owner}, "stale_renderer_session"),
            ("m0_save", {"document_id": "100k-b", "edit_session_id": previous_session["edit_session_id"], "generation": "1", "bytes_base64": previous_session["bytes_base64"]}, "StaleSession"),
            ("m0_close", {"document_id": "100k-b", "edit_session_id": previous_session["edit_session_id"]}, "StaleSession"),
        ]
        evidence["renderer_session_rejections"] = []
        for command, payload, expected in stale_requests:
            result = native_result(command, {"request": payload})
            assert result == {"ok": False, "failure": expected}, result
            evidence["renderer_session_rejections"].append({"command": command, "error": result["failure"]})
        assert checkpoint("100k-b") is None

        resumed_text = "재로드 후 편집\n"
        js("const view=document.querySelector('.cm-content').cmTile.root.view; view.dispatch({changes:{from:view.state.doc.length,insert:arguments[0]}})", resumed_text)
        wait(lambda: checkpoint("welcome")[0] == 41)
        assert checkpoint("welcome")[1] == durable_before_reload[1] + resumed_text.encode()
        evidence["tests"].append("reload restores generation 40 bytes, changes the renderer owner, rejects old open/save/close requests and resumes editing at generation 41")

        validation_session = native("m0_open", {"request": {"document_id": "100k-b", "renderer_session_id": current_owner}})
        evidence["source_rejections"] = []
        for content, expected in ((b"invalid\r\nsource\n", "non_lf_source"), (b"\xff", "invalid_utf8")):
            result = native_result("m0_save", {"request": {
                "document_id": "100k-b",
                "edit_session_id": validation_session["edit_session_id"],
                "generation": "1",
                "bytes_base64": base64.b64encode(content).decode(),
            }})
            assert result == {"ok": False, "failure": expected}, result
            evidence["source_rejections"].append({"command": "m0_save", "error": result["failure"]})
        assert checkpoint("100k-b") is None
        native("m0_close", {"request": {"document_id": "100k-b", "edit_session_id": validation_session["edit_session_id"]}})
        evidence["tests"].append("native save rejects CR-containing UTF-8 and invalid UTF-8 without creating a checkpoint")

        if args.measure:
            measurements = []
            for document_id, size in (("100k", 100 * 1024), ("5m", 5 * 1024 * 1024)):
                open_document(document_id)
                measured = js("""
                  const [size,done]=arguments;
                  const view=document.querySelector('.cm-content').cmTile.root.view;
                  const frame=()=>new Promise(resolve=>requestAnimationFrame(()=>requestAnimationFrame(resolve)));
                  const sleep=ms=>new Promise(resolve=>setTimeout(resolve,Math.max(0,ms)));
                  (async()=>{
                    const result=[];
                    for(const phase of ['typing','paste']){
                      const values=[]; const start=performance.now();
                      const count=phase==='typing'?40:10;
                      for(let i=0;i<count;i++){
                        if(phase==='typing') await sleep(start+i*250-performance.now());
                        let changes;
                        if(phase==='typing') changes={from:0,to:1,insert:i%2?'a':'b'};
                        else {
                          const line=(i%2?'p':'q').repeat(63)+'\\n';
                          const text=line.repeat(Math.floor(size/64))+'x'.repeat(size%64);
                          changes={from:0,to:view.state.doc.length,insert:text};
                        }
                        const before=performance.now(); view.dispatch({changes}); await frame();
                        values.push(performance.now()-before);
                      }
                      values.sort((a,b)=>a-b);
                      result.push({phase,document_bytes:size,samples:count,p50_ms:values[Math.ceil(count*.5)-1],p95_ms:values[Math.ceil(count*.95)-1],max_ms:values[count-1]});
                    }
                    done(result);
                  })().catch(error=>done({error:String(error)}));
                """, size, asynchronous=True)
                assert isinstance(measured, list), measured
                click("#save")
                wait(lambda: js("return document.querySelector('#save-status').textContent === '이 기기에 저장됨'"))
                assert len(checkpoint(document_id)[1]) == size
                measurements.extend(measured)
                transitions = []
                active = document_id
                for index in range(20):
                    js("document.querySelector('.cm-content').cmTile.root.view.dispatch({changes:{from:0,to:1,insert:arguments[0]}})", "s" if index % 2 else "t")
                    target = document_id + "-b" if active == document_id else document_id
                    before = time.monotonic()
                    open_document(target)
                    js("const done=arguments[arguments.length-1]; requestAnimationFrame(()=>requestAnimationFrame(()=>done(true)))", asynchronous=True)
                    transitions.append((time.monotonic() - before) * 1000)
                    assert len(checkpoint(active)[1]) == size
                    active = target
                transitions.sort()
                measurements.append({"phase":"switch", "document_bytes":size, "samples":20, "p50_ms":transitions[9], "p95_ms":transitions[18], "max_ms":transitions[19]})
            evidence["ui_measurements"] = measurements
            evidence["ui_measurement_scope"] = "CodeMirror transactions to two animation frames in real WebKitGTK/Xvfb; not physical keyboard or GPU display latency"

        failures = js("""
          const owner = arguments[0];
          const done = arguments[arguments.length - 1];
          Promise.all([
            ['plugin:fs|read_text_file', {path:'/notryx-probe-must-not-exist'}],
            ['plugin:shell|execute', {program:'notryx-probe-must-not-exist'}],
            ['plugin:sql|load', {db:'sqlite:probe.db'}],
            ['m0_open', {request:{document_id:'welcome', renderer_session_id:owner, extra:true}}]
          ].map(async ([command,args]) => {
            try { await window.__TAURI_INTERNALS__.invoke(command,args); return {command,denied:false}; }
            catch(error) { return {command,denied:true,error:String(error)}; }
          })).then(done);
        """, current_owner, asynchronous=True)
        for result in failures:
            assert result["denied"], result
            if result["command"] == "m0_open":
                assert "unknown field" in result["error"], result
            else:
                assert re.search(r"not allowed|^plugin (fs|shell|sql) not found$", result["error"], re.IGNORECASE), result
        evidence["native_rejections"] = failures

        requests = []

        class PreviewProbe(http.server.BaseHTTPRequestHandler):
            def do_GET(self):
                requests.append(self.path)
                self.send_response(204)
                self.end_headers()

            def log_message(self, format, *args):
                pass

        probe_server = http.server.HTTPServer(("127.0.0.1", 0), PreviewProbe)
        probe_thread = threading.Thread(target=probe_server.serve_forever, daemon=True)
        probe_thread.start()
        try:
            open_document("welcome")
            probe_url = f"http://127.0.0.1:{probe_server.server_port}/preview-probe"
            malicious = "\n\n".join([
                "# 미리보기 보안 시험",
                "<script>window.previewExecuted = true</script>",
                "<svg onload='window.previewExecuted = true'></svg>",
                f"![외부 이미지]({probe_url})",
                "[위험 링크](javascript:alert(1))",
            ])
            js("const v=document.querySelector('.cm-content').cmTile.root.view; v.dispatch({changes:{from:0,to:v.state.doc.length,insert:arguments[0]}})", malicious)
            click("#preview-toggle")
            wait(lambda: js("return document.querySelector('.markdown-preview h1')?.textContent === '미리보기 보안 시험'"))
            assert js("return !window.previewExecuted && !document.querySelector('.markdown-preview script, .markdown-preview svg, .markdown-preview img, .markdown-preview a')")
            time.sleep(0.5)
            assert not requests, requests
            click("#preview-toggle")
        finally:
            probe_server.shutdown()
            probe_thread.join(timeout=5)
            probe_server.server_close()
        evidence["tests"].append("basic preview blocks raw HTML, SVG, unsafe links and external image requests in the Linux WebView")

        click("#isolation-open")
        handles = wait(lambda: (handles if len(handles := request("GET", f"/session/{session}/window/handles")) == 2 else None))
        initial = request("GET", f"/session/{session}/window")
        probe_handle = next(handle for handle in handles if handle != initial)
        request("POST", f"/session/{session}/window", {"handle": probe_handle})
        probe = wait(lambda: js("const t=document.querySelector('#result')?.textContent; return t?.startsWith('{') ? JSON.parse(t) : null;"))
        assert probe["bridgePresent"] and probe["ipcBlocked"] and probe["networkBlocked"], probe
        evidence["separate_webview"] = probe
        request("POST", f"/session/{session}/window", {"handle": initial})
        frame = request("POST", f"/session/{session}/element", {"using": "css selector", "value": "#sandbox-probe"})
        request("POST", f"/session/{session}/frame", {"id": frame})
        iframe = wait(lambda: js("const t=document.querySelector('#result')?.textContent; return t?.startsWith('{') ? JSON.parse(t) : null;"))
        assert iframe["parentBlocked"] and iframe["networkBlocked"] and (iframe["ipcBlocked"] or not iframe["bridgePresent"]), iframe
        evidence["sandboxed_iframe"] = iframe
        request("POST", f"/session/{session}/frame", {"id": None})

        screenshot = request("GET", f"/session/{session}/screenshot")
        (output / "editor.png").write_bytes(base64.b64decode(screenshot))
        (output / "result.json").write_text(json.dumps(evidence, ensure_ascii=False, indent=2) + "\n")
        print(json.dumps(evidence, ensure_ascii=False, indent=2), flush=True)
    except Exception:
        if session:
            try:
                diagnostic = js("return {url:location.href,body:document.body.innerText,html:document.documentElement.outerHTML.slice(0,12000)}")
                (output / "failure.json").write_text(json.dumps(diagnostic, ensure_ascii=False, indent=2))
                print(json.dumps({"url": diagnostic["url"], "body": diagnostic["body"][:1000]}, ensure_ascii=False), flush=True)
            except Exception:
                pass
        raise
    finally:
        if session:
            try:
                request("DELETE", f"/session/{session}")
            except Exception:
                pass
        try:
            os.killpg(process.pid, signal.SIGTERM)
            process.wait(timeout=10)
        except ProcessLookupError:
            pass
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait()
        log.close()
        directory.cleanup()


if __name__ == "__main__":
    main()
