#!/usr/bin/env python3
import argparse
import base64
import json
import os
from pathlib import Path
import signal
import socket
import sqlite3
import subprocess
import tempfile
import time
import urllib.error
import urllib.request


class Browser:
    def __init__(self, args, directory, output, safe=False):
        self.args, self.directory, self.output, self.safe = args, directory, output, safe
        self.session = None

    def request(self, method, path, payload=None):
        data = None if payload is None else json.dumps(payload).encode()
        req = urllib.request.Request(self.origin + path, data=data, method=method,
                                     headers={"Content-Type": "application/json"})
        try:
            with urllib.request.urlopen(req, timeout=40) as response:
                value = json.load(response)["value"]
        except urllib.error.HTTPError as error:
            raise RuntimeError(error.read().decode()) from error
        if isinstance(value, dict) and "error" in value and "ok" not in value:
            raise RuntimeError(value)
        return value

    def js(self, script, *values, asynchronous=False):
        return self.request("POST", f"/session/{self.session}/execute/{'async' if asynchronous else 'sync'}",
                            {"script": script, "args": list(values)})

    def native(self, command, args=None):
        return self.js("""const [command,args,done]=arguments;
            window.__TAURI_INTERNALS__.invoke(command,args)
              .then(value=>done({ok:true,value})).catch(error=>done({ok:false,error:String(error)}));
            """, command, args or {}, asynchronous=True)

    def wait(self, check, seconds=20):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            value = check()
            if value:
                return value
            time.sleep(0.1)
        raise AssertionError("Timed out waiting for theme WebView state")

    def __enter__(self):
        environment = os.environ.copy()
        environment.update(TAURI_WEBVIEW_AUTOMATION="true", NOTRYX_M0_DATA_DIR=str(self.directory),
                           XDG_DATA_HOME=str(self.directory / "data"), XDG_CACHE_HOME=str(self.directory / "cache"),
                           NOTRYX_SAFE_MODE="1" if self.safe else "0")
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            port = sock.getsockname()[1]
        self.origin = f"http://127.0.0.1:{port}"
        self.log = (self.output / "driver.log").open("a")
        self.process = subprocess.Popen([self.args.driver, f"--port={port}", "--host=127.0.0.1"], env=environment,
                                        stdout=self.log, stderr=self.log, start_new_session=True)
        try:
            for _ in range(100):
                try:
                    self.request("GET", "/status")
                    break
                except (OSError, RuntimeError):
                    if self.process.poll() is not None:
                        raise RuntimeError("WebDriver exited")
                    time.sleep(0.1)
            self.session = self.request("POST", "/session", {"capabilities": {"alwaysMatch": {
                "browserName": "wry", "webkitgtk:browserOptions": {"binary": str(Path(self.args.binary).resolve())}
            }}})["sessionId"]
            self.request("POST", f"/session/{self.session}/timeouts", {"script": 30000})
            self.wait(lambda: self.js("return !!document.querySelector('.cm-content') && !document.querySelector('#theme-reset')?.disabled"))
            self.js("document.querySelector('.theme-controls').open=true")
            return self
        except BaseException:
            self.__exit__(None, None, None)
            raise

    def __exit__(self, *_):
        if self.session:
            try:
                self.request("DELETE", f"/session/{self.session}")
            except (OSError, RuntimeError):
                pass
        try:
            os.killpg(self.process.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        try:
            self.process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            os.killpg(self.process.pid, signal.SIGKILL)
            self.process.wait(timeout=5)
        self.log.close()

    def select(self, selection):
        self.js("const select=document.querySelector('#theme-select');select.value=arguments[0];select.dispatchEvent(new Event('change',{bubbles:true}))", selection)
        self.wait(lambda: self.js("return document.querySelector('#theme-select')?.value===arguments[0] && !document.querySelector('#theme-select').disabled", selection))

    def checkpoint(self):
        with sqlite3.connect(f"file:{self.directory / 'drafts.sqlite3'}?mode=ro", uri=True) as db:
            return db.execute("SELECT generation, bytes FROM draft_checkpoints WHERE document_id='welcome'").fetchone()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--driver", default="WebKitWebDriver")
    parser.add_argument("--binary", default="target/debug/notryx-client")
    parser.add_argument("--output", default=".artifacts/m4-theme")
    args = parser.parse_args()
    output = Path(args.output).resolve(); output.mkdir(parents=True, exist_ok=True)
    package = json.loads((Path(__file__).resolve().parent.parent / "examples/themes/ocean.json").read_text())
    source = "# 합성 테마 시험\n\n```mermaid\nflowchart LR\n A[한글] --> B[보존]\n```\n"
    edited = source + "\n편집 보존"
    with tempfile.TemporaryDirectory(prefix="notryx-theme-") as temporary:
        directory = Path(temporary)
        theme_path = directory / "theme.json"
        with Browser(args, directory, output) as browser:
            browser.js("const view=document.querySelector('.cm-content').cmTile.root.view;view.dispatch({changes:{from:0,to:view.state.doc.length,insert:arguments[0]}})", source)
            browser.wait(lambda: (row := browser.checkpoint()) and row[1] == source.encode())
            browser.js("window.themeView=document.querySelector('.cm-content').cmTile.root.view;themeView.dispatch({changes:{from:themeView.state.doc.length,insert:'\\n편집 보존'},selection:{anchor:1,head:3}})")
            browser.wait(lambda: (row := browser.checkpoint()) and row[1] == edited.encode())
            before = browser.checkpoint()
            browser.js("document.querySelector('#preview-toggle').click()")
            browser.wait(lambda: browser.js("const image=document.querySelector('.mermaid-preview img');return image?.complete && image.naturalWidth>0"))
            first_diagram = browser.js("return document.querySelector('.mermaid-preview img').src")
            browser.select("dark")
            browser.wait(lambda: browser.js("return document.documentElement.dataset.theme==='dark' && getComputedStyle(document.body).color==='rgb(231, 234, 240)'"))
            browser.wait(lambda: browser.js("const image=document.querySelector('.mermaid-preview img');return image?.complete && image.naturalWidth>0 && image.src!==arguments[0]", first_diagram))
            assert browser.js("return themeView===document.querySelector('.cm-content').cmTile.root.view && themeView.state.selection.main.anchor===1 && themeView.state.selection.main.head===3")
            assert browser.checkpoint() == before

            # The selected synthetic file enters the same File API handler as the OS picker.
            browser.js("const file=new File([arguments[0]],'ocean.json',{type:'application/json'});const transfer=new DataTransfer();transfer.items.add(file);const input=document.querySelector('#theme-file');input.files=transfer.files;input.dispatchEvent(new Event('change',{bubbles:true}))", json.dumps(package))
            browser.wait(lambda: browser.js("return document.querySelector('#theme-select').value==='custom' && getComputedStyle(document.documentElement).getPropertyValue('--surface-background').trim()==='#10202c'"))
            assert browser.js("return getComputedStyle(document.querySelector('.cm-editor')).fontSize") == "16px"
            assert browser.checkpoint() == before
            assert browser.js("return themeView===document.querySelector('.cm-content').cmTile.root.view && themeView.state.doc.toString()===arguments[0]", edited)
            (output / "dark-theme.png").write_bytes(base64.b64decode(browser.request("GET", f"/session/{browser.session}/screenshot")))

            browser.js("themeView.focus();themeView.contentDOM.dispatchEvent(new KeyboardEvent('keydown',{key:'z',code:'KeyZ',ctrlKey:true,bubbles:true,cancelable:true}))")
            assert browser.js("return themeView.state.doc.toString()") == source
            browser.js("themeView.contentDOM.dispatchEvent(new KeyboardEvent('keydown',{key:'y',code:'KeyY',ctrlKey:true,bubbles:true,cancelable:true}))")
            assert browser.js("return themeView.state.doc.toString()") == edited
            browser.wait(lambda: (row := browser.checkpoint()) and row[1] == edited.encode())
            saved = theme_path.read_bytes()
            for invalid in [dict(package, css="body{display:none}"), dict(package, tokens=dict(package["tokens"], **{"surface.background": "url(https://example.invalid)"})), dict(package, schemaVersion=99)]:
                response = browser.native("theme_import", {"json": json.dumps(invalid)})
                assert not response["ok"], response
                assert theme_path.read_bytes() == saved
            print("Theme switch/import preserves editor identity, selection, undo/redo and checkpoint; invalid imports rejected", flush=True)

            main_window = browser.request("GET", f"/session/{browser.session}/window")
            assert browser.native("m0_open_isolation", {"mermaid": True})["ok"]
            handles = browser.wait(lambda: (items if len(items := browser.request("GET", f"/session/{browser.session}/window/handles")) > 1 else None))
            browser.request("POST", f"/session/{browser.session}/window", {"handle": next(item for item in handles if item != main_window)})
            for command in ("theme_status", "theme_select", "theme_import", "theme_reset"):
                response = browser.native(command, {"selection": "dark", "json": json.dumps(package)})
                assert not response["ok"] and any(word in response["error"].lower() for word in ("not allowed", "denied", "invalid_caller")), response
            browser.request("POST", f"/session/{browser.session}/window", {"handle": main_window})
            print("Isolated renderer cannot read or change device theme settings", flush=True)

        with Browser(args, directory, output) as browser:
            assert browser.native("theme_status")["value"]["selection"] == "custom"
            assert browser.js("return getComputedStyle(document.documentElement).getPropertyValue('--surface-background').trim()") == "#10202c"
            assert browser.js("return document.querySelector('.cm-content').cmTile.root.view.state.doc.toString()") == edited
            print("Real application restart restores selected theme and draft", flush=True)
        with Browser(args, directory, output, safe=True) as browser:
            assert browser.js("return document.documentElement.dataset.theme") == "light"
            assert theme_path.read_bytes() == saved
            assert not browser.native("theme_select", {"selection": "dark"})["ok"]
            assert browser.js("return document.querySelector('#theme-file').disabled")
            browser.js("document.querySelector('#theme-reset').click()")
            browser.wait(lambda: json.loads(theme_path.read_text())["selection"] == "light")
            assert json.loads(theme_path.read_text())["custom"] is None
            print("Safe mode bypasses custom settings without deleting them and permits explicit reset", flush=True)
        theme_path.write_text("{invalid synthetic theme")
        with Browser(args, directory, output) as browser:
            assert browser.js("return document.documentElement.dataset.theme") == "light"
            assert browser.native("theme_status")["value"]["warning"] is not None
            assert theme_path.read_text() == "{invalid synthetic theme"
            assert browser.js("return document.querySelector('.cm-content').cmTile.root.view.state.doc.toString()") == edited
            browser.js("document.querySelector('#theme-reset').click()")
            browser.wait(lambda: browser.native("theme_status")["value"]["warning"] is None)
            print("Corrupt theme starts with defaults, preserves the draft, and can be reset", flush=True)
    (output / "result.json").write_text(json.dumps({"passed": True, "platform": "Linux WebKitGTK / Xvfb",
        "os_file_picker": "not tested; synthetic File API change event", "physical_ime": "not tested"}, indent=2) + "\n")


if __name__ == "__main__":
    main()
