// @vitest-environment jsdom
import { expect, test } from "vitest";
import { Compartment, EditorState } from "@codemirror/state";
import { history, undo, undoDepth } from "@codemirror/commands";
import { applyDocumentTheme, editorTheme, resolveTheme, themeStatus } from "./theme";

const custom = {
  id: "theme.test", schemaVersion: 1, name: "합성 테마", base: "dark",
  tokens: { "surface.background": "#111111", "text.primary": "#eeeeee", "border.default": "#444444", "status.conflict": "#ffcc66", "editor.fontSize": 18 },
};

test("uses built-in defaults for optional tokens and ignores custom data in safe mode", () => {
  const normal = themeStatus({ selection: "custom", custom, safe_mode: false, warning: null });
  const theme = resolveTheme(normal);
  expect(theme.base).toBe("dark");
  expect(theme.tokens["surface.background"]).toBe("#111111");
  expect(theme.tokens["editor.fontSize"]).toBe(18);
  expect(theme.tokens["editor.fontFamily"]).toBe("system-monospace");
  const safe = resolveTheme({ ...normal, safe_mode: true });
  expect(safe.base).toBe("light");
  expect(safe.tokens["surface.background"]).not.toBe("#111111");
});

test.each([
  { ...custom, css: "body{display:none}" },
  { ...custom, schemaVersion: 2 },
  { ...custom, tokens: { ...custom.tokens, "surface.background": "url(https://example.invalid)" } },
  { ...custom, tokens: { ...custom.tokens, "editor.fontFamily": "https://example.invalid/font" } },
  { ...custom, tokens: { ...custom.tokens, "editor.fontSize": 200 } },
  { ...custom, tokens: { ...custom.tokens, "spacing.scale": 100 } },
  { ...custom, tokens: { "surface.background": "#111111" } },
])("never applies an invalid native theme response", (value) => {
  expect(() => themeStatus({ selection: "custom", custom: value, safe_mode: false, warning: null })).toThrow();
});

test("writes only known CSS variables and removes previous values on switching", () => {
  const theme = resolveTheme(themeStatus({ selection: "custom", custom, safe_mode: false, warning: null }));
  applyDocumentTheme(theme);
  expect(document.documentElement.style.getPropertyValue("--surface-background")).toBe("#111111");
  expect(document.documentElement.dataset.theme).toBe("dark");
  applyDocumentTheme(resolveTheme({ selection: "light", custom: null, safe_mode: false, warning: null }));
  expect(document.documentElement.style.getPropertyValue("--surface-background")).not.toBe("#111111");
  expect(document.documentElement.style.cssText).not.toContain("url(");
});

test("reconfigures the editor without changing document, selection or undo history", () => {
  const compartment = new Compartment();
  let state = EditorState.create({ doc: "원문", extensions: [history(), compartment.of(editorTheme(resolveTheme({ selection: "light", custom: null, safe_mode: false, warning: null })))] });
  state = state.update({ changes: { from: 2, insert: " 편집" }, selection: { anchor: 1, head: 3 } }).state;
  const before = state;
  state = state.update({ effects: compartment.reconfigure(editorTheme(resolveTheme({ selection: "dark", custom: null, safe_mode: false, warning: null }))) }).state;
  expect(state.doc).toBe(before.doc);
  expect(state.selection.eq(before.selection)).toBe(true);
  expect(undoDepth(state)).toBe(1);
  expect(undo({ state, dispatch: (transaction) => { state = transaction.state; } })).toBe(true);
  expect(state.doc.toString()).toBe("원문");
});
