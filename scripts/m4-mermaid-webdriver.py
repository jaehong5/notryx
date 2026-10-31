#!/usr/bin/env python3
import argparse
import base64
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import tempfile
import time
import urllib.error
import urllib.request


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--driver", default="WebKitWebDriver")
    parser.add_argument("--binary", default="target/debug/notryx-client")
    parser.add_argument("--output", default=".artifacts/m4-mermaid")
    args = parser.parse_args()
    output = Path(args.output).resolve()
    output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="notryx-mermaid-") as directory:
        environment = os.environ.copy()
        environment.update(TAURI_WEBVIEW_AUTOMATION="true", NOTRYX_M0_DATA_DIR=directory,
                           XDG_DATA_HOME=directory + "/data", XDG_CACHE_HOME=directory + "/cache")
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            port = sock.getsockname()[1]
        origin = f"http://127.0.0.1:{port}"
        session = None
        log = (output / "driver.log").open("w")
        process = subprocess.Popen([args.driver, f"--port={port}", "--host=127.0.0.1"], env=environment,
                                   stdout=log, stderr=log, start_new_session=True)

        def request(method, path, payload=None):
            data = None if payload is None else json.dumps(payload).encode()
            req = urllib.request.Request(origin + path, data=data, method=method,
                                         headers={"Content-Type": "application/json"})
            try:
                with urllib.request.urlopen(req, timeout=40) as response:
                    value = json.load(response)["value"]
            except urllib.error.HTTPError as error:
                raise RuntimeError(error.read().decode()) from error
            if isinstance(value, dict) and "error" in value and "ok" not in value:
                raise RuntimeError(value)
            return value

        def wait(check, seconds=25):
            deadline = time.monotonic() + seconds
            while time.monotonic() < deadline:
                result = check()
                if result:
                    return result
                time.sleep(0.1)
            raise AssertionError("Timed out waiting for Mermaid WebView state")

        def js(script, *values, asynchronous=False):
            return request("POST", f"/session/{session}/execute/{'async' if asynchronous else 'sync'}",
                           {"script": script, "args": list(values)})

        def native(command, arguments=None):
            return js("""const [command,args,done]=arguments;
                window.__TAURI_INTERNALS__.invoke(command,args)
                  .then(value=>done({ok:true,value})).catch(error=>done({ok:false,error:String(error)}));
                """, command, arguments or {}, asynchronous=True)

        def source(text):
            js("const view=document.querySelector('.cm-content').cmTile.root.view; view.dispatch({changes:{from:0,to:view.state.doc.length,insert:arguments[0]}})", text)

        try:
            for _ in range(100):
                try:
                    request("GET", "/status")
                    break
                except (OSError, RuntimeError):
                    if process.poll() is not None:
                        raise RuntimeError("WebDriver exited")
                    time.sleep(0.1)
            session = request("POST", "/session", {"capabilities": {"alwaysMatch": {
                "browserName": "wry", "webkitgtk:browserOptions": {"binary": str(Path(args.binary).resolve())}
            }}})["sessionId"]
            request("POST", f"/session/{session}/timeouts", {"script": 30000})
            wait(lambda: js("return !!document.querySelector('.cm-content[contenteditable=true]')"))
            main_window = request("GET", f"/session/{session}/window")
            diagrams = ["flowchart LR\n A[한글 시작] --> B[완료]", "sequenceDiagram\n A->>B: 한글 메시지",
                        "classDiagram\n A --> B", "stateDiagram-v2\n A --> B", "erDiagram\n A ||--o{ B : has"]
            for index, diagram in enumerate(diagrams):
                result = native("mermaid_render", {"source": diagram, "theme": "light"})
                assert result["ok"], result
                (output / f"diagram-{index}.svg").write_text(result["value"])
                text = f"# 합성 시험\n\n```mermaid\n{diagram}\n```\n"
                source(text)
                if index == 0:
                    js("document.querySelector('#preview-toggle').click()")
                wait(lambda: js("const img=document.querySelector('.mermaid-preview img');return document.querySelector('.mermaid-preview code')?.textContent === arguments[0] && !!img && img.complete && img.naturalWidth>0", diagram + "\n"))
                assert js("return document.querySelector('.cm-content').cmTile.root.view.state.doc.toString()") == text
                if index == 0:
                    (output / "preview.png").write_bytes(base64.b64decode(request("GET", f"/session/{session}/screenshot")))
                print(f"Rendered diagram {index + 1}; source preserved", flush=True)
            for bad, reason in [
                ('%%{init: {"htmlLabels":"false"}}%%\nflowchart LR\n A-->B', "설정 변경"),
                ('---\nconfig:\n  securityLevel: loose\n---\nflowchart LR\nA-->B', "설정 변경"),
                ('flowchart LR\n A[', "문법"),
                ('flowchart LR\n' + "x" * 65536, "64 KiB"),
                ('sequenceDiagram\n' + 'A->>B: message\n' * 501, "500개"),
                ('flowchart LR\nA@{img: "https://example.invalid/image.png", label: "remote"}', ""),
            ]:
                source(f"```mermaid\n{bad}\n```\n")
                wait(lambda: js("const p=document.querySelector('.mermaid-preview p');return document.querySelector('.mermaid-preview code')?.textContent === arguments[0] && !!p && !p.textContent.includes('그리는 중') && p.textContent.includes(arguments[1])", bad + "\n", reason))
                assert js("return !document.querySelector('.mermaid-preview img')")
            print("Configuration, invalid syntax and size/edge fallback passed", flush=True)

            large = "flowchart LR\n" + "\n".join(f"A --> B{i}" for i in range(500))
            started = time.monotonic()
            result = native("mermaid_render", {"source": large, "theme": "light"})
            assert time.monotonic() - started < 22
            assert result["ok"] or result["error"] == "mermaid_render_timeout", result
            print(f"500-edge render bounded: {'rendered' if result['ok'] else 'timeout fallback'}", flush=True)
            source(f"```mermaid\n{diagrams[0]}\n```\n")
            wait(lambda: js("const img=document.querySelector('.mermaid-preview img');return document.querySelector('.mermaid-preview code')?.textContent === arguments[0] && !!img && img.complete && img.naturalWidth>0", diagrams[0] + "\n"))

            probe = native("m0_open_isolation", {"mermaid": True})
            assert probe["ok"], probe
            windows = wait(lambda: (handles if len(handles := request("GET", f"/session/{session}/window/handles")) > 1 else None))
            request("POST", f"/session/{session}/window", {"handle": next(handle for handle in windows if handle != main_window)})
            wait(lambda: js("return location.pathname === '/mermaid.html' && !!window.__TAURI_INTERNALS__"))
            for command in ("m0_renderer_session", "workspace_local_drafts", "workspace_inventory", "mermaid_render"):
                result = native(command, {"source": "flowchart LR\nA-->B", "theme": "light"})
                assert not result["ok"] and any(word in result["error"].lower() for word in ("not allowed", "denied", "invalid_caller")), result
            assert not native("mermaid_complete", {"svg": "<svg/>", "error": None})["ok"]
            blocked = js("""const done=arguments[0]; const blocked=[];
              document.addEventListener('securitypolicyviolation',e=>blocked.push(e.effectiveDirective));
              fetch('https://example.invalid/notryx-probe').catch(()=>{});
              const img=new Image();img.src='https://example.invalid/image';document.body.append(img);
              const frame=document.createElement('iframe');frame.src='tauri://localhost/index.html';document.body.append(frame);
              setTimeout(()=>done(blocked),300);
              """, asynchronous=True)
            assert all(kind in blocked for kind in ("connect-src", "img-src", "frame-src")), blocked
            assert js("return window.opener === null")
            assert js("return window.open('https://example.invalid/') === null")
            request("POST", f"/session/{session}/window", {"handle": main_window})
            request("POST", f"/session/{session}/refresh", {})
            wait(lambda: len(request("GET", f"/session/{session}/window/handles")) == 1)
            print("Renderer privilege/network/frame denial and reload cleanup passed", flush=True)
            (output / "result.json").write_text(json.dumps({"passed": True, "platform": "Linux WebKitGTK / Xvfb",
                "diagrams": len(diagrams), "physical_input": "not tested"}, indent=2) + "\n")
        finally:
            if session:
                try:
                    request("DELETE", f"/session/{session}")
                except (OSError, RuntimeError):
                    pass
            try:
                os.killpg(process.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait(timeout=5)
            log.close()


if __name__ == "__main__":
    main()
