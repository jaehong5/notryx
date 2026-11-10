#!/usr/bin/env python3
"""Check compact layouts in Linux WebView; this is not an Android device test."""

import argparse
import base64
import importlib.util
import json
from pathlib import Path
import sqlite3
import tempfile


spec = importlib.util.spec_from_file_location("theme_webdriver", Path(__file__).with_name("m4-theme-webdriver.py"))
theme_webdriver = importlib.util.module_from_spec(spec)
spec.loader.exec_module(theme_webdriver)
Browser = theme_webdriver.Browser


def checkpoint(directory, document_id):
    with sqlite3.connect(f"file:{directory / 'drafts.sqlite3'}?mode=ro", uri=True) as db:
        return db.execute("SELECT generation, bytes FROM draft_checkpoints WHERE document_id = ?",
                          (document_id,)).fetchone()


def resize(browser, width, height):
    endpoint = f"/session/{browser.session}/window/rect"
    browser.request("POST", endpoint, {"width": width, "height": height})
    actual = browser.js("return {width:innerWidth,height:innerHeight}")
    if actual != {"width": width, "height": height}:
        outer = browser.request("GET", endpoint)
        browser.request("POST", endpoint, {
            "width": outer["width"] + width - actual["width"],
            "height": outer["height"] + height - actual["height"],
        })
    browser.wait(lambda: browser.js("return innerWidth===arguments[0] && innerHeight===arguments[1]",
                                    width, height))


def click(browser, selector):
    browser.wait(lambda: browser.js("const element=document.querySelector(arguments[0]);return element && !element.disabled", selector))
    browser.js("document.querySelector(arguments[0]).click()", selector)


def screen(browser, expected, preview=False):
    browser.wait(lambda: browser.js("return document.querySelector('main')?.dataset.mobileScreen===arguments[0]", expected))
    state = browser.js("""
        const shown=selector=>{
            const element=document.querySelector(selector);
            return !!element && element.getClientRects().length>0 && getComputedStyle(element).visibility!=='hidden';
        };
        return {
            compact:matchMedia('(max-width: 650px)').matches,
            nav:shown('nav'), document:shown('.document'), editor:shown('#editor'),
            preview:shown('.preview-pane'), navigation:shown('.mobile-navigation'),
            editable:document.querySelector('.cm-content')?.getAttribute('contenteditable'),
            overflow:Math.max(document.body.scrollWidth,document.documentElement.scrollWidth)-innerWidth
        };
    """)
    assert state["compact"] and state["overflow"] <= 1, state
    if expected == "list":
        assert state["nav"] and not state["document"] and not state["editor"] and not state["preview"], state
    else:
        assert not state["nav"] and state["document"] and state["navigation"], state
        assert state["preview"] == preview and state["editor"] != preview, state
        assert state["editable"] == ("true" if expected == "edit" else "false"), state
    return state


def screenshot(browser, path):
    path.write_bytes(base64.b64decode(browser.request("GET", f"/session/{browser.session}/screenshot")))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--driver", default="WebKitWebDriver")
    parser.add_argument("--binary", default="target/debug/notryx-client")
    parser.add_argument("--output", default=".artifacts/m5a-mobile")
    args = parser.parse_args()
    output = Path(args.output).resolve()
    output.mkdir(parents=True, exist_ok=True)
    evidence = {
        "passed": False, "platform": "Linux WebKitGTK / resized window",
        "android_device": "not tested", "physical_ime": "not tested",
        "keyboard_insets": "not tested", "viewports": [],
    }
    (output / "result.json").write_text(json.dumps(evidence, indent=2) + "\n", encoding="utf-8")
    with tempfile.TemporaryDirectory(prefix="notryx-mobile-webview-") as temporary:
        directory = Path(temporary)
        with Browser(args, directory, output) as browser:
            try:
                browser.js("document.querySelector('.theme-controls').open=false")
                browser.wait(lambda: browser.js("return document.querySelector('[data-document=welcome]')?.getAttribute('aria-current')==='page'"))
                original = browser.js("window.mobileTestView=document.querySelector('.cm-content').cmTile.root.view;return mobileTestView.state.doc.toString()")
                assert original.startswith("# Notryx\n"), "welcome fixture did not open"
                assert checkpoint(directory, "welcome") is None, "unchanged fixture already has a checkpoint"
                for width in (390, 320):
                    resize(browser, width, 780)
                    screen(browser, "list")
                    screenshot(browser, output / f"{width}-list.png")
                    click(browser, '[data-document="welcome"]')
                    screen(browser, "read", preview=True)
                    assert browser.js("return document.querySelector('.preview-pane').textContent.includes('Notryx')")
                    assert browser.js("return mobileTestView===document.querySelector('.cm-content').cmTile.root.view && mobileTestView.state.doc.toString()===arguments[0]", original)
                    screenshot(browser, output / f"{width}-read.png")
                    click(browser, '[data-testid="mobile-edit"]')
                    screen(browser, "edit")
                    addition = f"\n모바일 {width} 저장 시험 😀\n"
                    edited = original + addition
                    browser.js("""
                        mobileTestView.dispatch({changes:{from:mobileTestView.state.doc.length,insert:arguments[0]}});
                        window.dispatchEvent(new Event('android-back-requested', {cancelable:true}));
                    """, addition)
                    screen(browser, "read", preview=True)
                    saved = checkpoint(directory, "welcome")
                    assert saved and saved[1] == edited.encode(), "read screen appeared before the checkpoint was durable"
                    assert browser.js("return mobileTestView===document.querySelector('.cm-content').cmTile.root.view && mobileTestView.state.doc.toString()===arguments[0]", edited)
                    browser.wait(lambda: browser.js("return document.querySelector('.preview-pane')?.textContent.includes(arguments[0])", addition.strip()))
                    click(browser, '[data-testid="mobile-back"]')
                    screen(browser, "list")
                    assert checkpoint(directory, "welcome") == saved
                    click(browser, '[data-document="welcome"]')
                    screen(browser, "read", preview=True)
                    click(browser, '[data-testid="mobile-edit"]')
                    screen(browser, "edit")
                    assert browser.js("return mobileTestView===document.querySelector('.cm-content').cmTile.root.view")
                    screenshot(browser, output / f"{width}-edit.png")
                    browser.js("mobileTestView.focus();mobileTestView.contentDOM.dispatchEvent(new KeyboardEvent('keydown',{key:'z',code:'KeyZ',ctrlKey:true,bubbles:true,cancelable:true}))")
                    assert browser.js("return mobileTestView.state.doc.toString()") == original, "navigation lost undo history"
                    click(browser, '[data-testid="mobile-back"]')
                    screen(browser, "read", preview=True)
                    assert checkpoint(directory, "welcome")[1] == original.encode()
                    click(browser, '[data-testid="mobile-back"]')
                    screen(browser, "list")
                    evidence["viewports"].append({"width": width, "height": 780,
                        "list_read_edit": True, "single_pane": True, "horizontal_overflow": False,
                        "editor_identity_and_undo": True, "sqlite_checkpoint_before_read": True,
                        "system_back_event_before_read": True})
                    print(f"Compact {width}x780 passed: list/read/edit, one pane, undo and SQLite checkpoint.", flush=True)

                resize(browser, 320, 360)
                click(browser, '[data-document="welcome"]')
                screen(browser, "read", preview=True)
                click(browser, '[data-testid="mobile-edit"]')
                screen(browser, "edit")
                geometry = browser.js("""
                    const toolbar = document.querySelector('.toolbar').getBoundingClientRect();
                    const editor = document.querySelector('#editor').getBoundingClientRect();
                    const buttons = ['heading','list','code','link'].map(action => {
                        const r = document.querySelector(`[data-testid="markdown-${action}"]`).getBoundingClientRect();
                        return {left:r.left,right:r.right,top:r.top,bottom:r.bottom,height:r.height};
                    });
                    return {toolbarBottom:toolbar.bottom,editorHeight:editor.height,buttons};
                """)
                assert geometry["toolbarBottom"] <= 361 and geometry["editorHeight"] >= 80, geometry
                assert all(item["height"] >= 44 and item["left"] >= 0 and item["right"] <= 321
                           and item["bottom"] <= 361 for item in geometry["buttons"]), geometry
                foreground_window_focus = True
                for action, insertion in (("heading", "## "), ("list", "- "),
                                          ("code", "```\n코드\n```"), ("link", "[텍스트](https://)")):
                    browser.js("mobileTestView.dispatch({selection:{anchor:mobileTestView.state.doc.length}});mobileTestView.focus()")
                    before_focus = browser.js("return {editor:mobileTestView.hasFocus,document:document.hasFocus(),active:document.activeElement===mobileTestView.contentDOM}")
                    selector = f'[data-testid="markdown-{action}"]'
                    element = browser.request("POST", f"/session/{browser.session}/element", {"using": "css selector", "value": selector})
                    element_id = element["element-6066-11e4-a52e-4f735466cecf"]
                    browser.request("POST", f"/session/{browser.session}/element/{element_id}/click", {})
                    expected = original + insertion
                    assert browser.js("return mobileTestView.state.doc.toString()") == expected, action
                    after_focus = browser.js("return {editor:mobileTestView.hasFocus,document:document.hasFocus(),active:document.activeElement===mobileTestView.contentDOM,tag:document.activeElement?.tagName}")
                    # Xvfb may lack a foreground window; DOM focus must still remain on the editor.
                    assert before_focus["active"] and after_focus["active"], {"action": action, "before": before_focus, "after": after_focus}
                    if after_focus["document"]:
                        assert after_focus["editor"], after_focus
                    foreground_window_focus = foreground_window_focus and after_focus["document"]
                    click(browser, "#save")
                    browser.wait(lambda: (saved := checkpoint(directory, "welcome")) and saved[1] == expected.encode())
                    browser.js("mobileTestView.contentDOM.dispatchEvent(new KeyboardEvent('keydown',{key:'z',code:'KeyZ',ctrlKey:true,bubbles:true,cancelable:true}))")
                    assert browser.js("return mobileTestView.state.doc.toString()") == original, action
                    click(browser, "#save")
                    browser.wait(lambda: checkpoint(directory, "welcome")[1] == original.encode())
                screenshot(browser, output / "320-short-editor-tools.png")
                evidence["editing_tools"] = {"actions": ["heading", "list", "code", "link"],
                    "sqlite_checkpoint": True, "undo": True, "dom_focus": True,
                    "foreground_window_focus": foreground_window_focus,
                    "viewport": {"width": 320, "height": 360}, "geometry": geometry,
                    "physical_keyboard_resize": "not tested"}
                print("Editing tools passed: four native clicks, DOM focus, undo, SQLite and 320x360 layout.", flush=True)
                click(browser, '[data-testid="mobile-back"]')
                screen(browser, "read", preview=True)
                click(browser, '[data-testid="mobile-back"]')
                screen(browser, "list")
                resize(browser, 320, 780)

                stale = browser.native("mobile_background", {"request": {"renderer_session_id": "stale-mobile-probe"}})
                assert not stale["ok"] and "stale_renderer_session" in stale["error"], stale
                initial = browser.request("GET", f"/session/{browser.session}/window")
                click(browser, "#isolation-open")
                handles = browser.wait(lambda: (items if len(items := browser.request("GET", f"/session/{browser.session}/window/handles")) == 2 else None))
                isolated = next(handle for handle in handles if handle != initial)
                browser.request("POST", f"/session/{browser.session}/window", {"handle": isolated})
                browser.wait(lambda: browser.js("return !!window.__TAURI_INTERNALS__"))
                denied = browser.native("mobile_background", {"request": {"renderer_session_id": "isolated-mobile-probe"}})
                assert not denied["ok"] and "not allowed" in denied["error"], denied
                browser.request("POST", f"/session/{browser.session}/window", {"handle": initial})
                evidence["background_command_boundary"] = {"stale_owner_rejected": True, "isolated_webview_denied": True}
                print("Background command boundary passed: stale owner and isolated WebView rejected.", flush=True)

                saved_welcome = checkpoint(directory, "welcome")
                click(browser, '[data-document="5m"]')
                browser.wait(lambda: browser.js("""return document.querySelector('[data-document="5m"]')?.getAttribute('aria-current')==='page'"""))
                screen(browser, "read")
                assert browser.js("return new TextEncoder().encode(document.querySelector('.cm-content').cmTile.root.view.state.doc.toString()).length") == 5 * 1024 * 1024
                assert browser.js("return !document.querySelector('.preview-pane')"), "large source unexpectedly created a preview"
                assert checkpoint(directory, "5m") is None
                assert checkpoint(directory, "welcome") == saved_welcome
                screenshot(browser, output / "320-large-read.png")
                evidence["large_read_screen"] = {"bytes": 5 * 1024 * 1024,
                                                  "readonly_source_pane": True, "preview": False}
                print("5 MiB read screen passed: read-only source pane, no preview and no new checkpoint.", flush=True)
                evidence["passed"] = True
            except BaseException:
                try:
                    screenshot(browser, output / "failure.png")
                except Exception:
                    pass
                raise
            finally:
                (output / "result.json").write_text(json.dumps(evidence, indent=2) + "\n", encoding="utf-8")
    print("Linux compact WebView checks passed. Android devices, physical IME and keyboard insets were not tested.", flush=True)


if __name__ == "__main__":
    main()
