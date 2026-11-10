import { EditorState } from "@codemirror/state";
import { history, undo } from "@codemirror/commands";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import Markdown from "react-markdown";
import { expect, test, vi } from "vitest";
import { markdownEdit, type MarkdownAction } from "./markdown-edit";
import { documentSizeGuard } from "./document-size-guard";

function edit(text: string, from: number, to: number, action: MarkdownAction) {
  const state = EditorState.create({ doc: text, selection: { anchor: from, head: to } });
  const spec = markdownEdit(state, action);
  expect(spec).not.toBeNull();
  return state.update(spec!).state;
}

test("제목 삽입은 선택한 본문 줄에만 표시를 추가하고 앞의 메타데이터와 원문을 보존한다", () => {
  const original = "---\nunknown: keep\n---\nBody 한글😀\n<!-- keep -->\n";
  const from = original.indexOf("Body");
  const result = edit(original, from + 5, from + 5, "heading");
  expect(result.doc.toString()).toBe(original.slice(0, from) + "## " + original.slice(from));
  expect(result.selection.main.head).toBe(from + 8);
});

test("목록은 선택된 줄에만 추가하고 선택 끝에 걸친 다음 줄은 바꾸지 않는다", () => {
  const result = edit("first\nsecond\nlast", 0, 13, "list");
  expect(result.doc.toString()).toBe("- first\n- second\nlast");
  expect(result.selection.main.from).toBe(2);
  expect(result.selection.main.to).toBe(17);
});

test("코드 블록은 선택한 내용의 기존 fence보다 긴 fence로 감싼다", () => {
  const selected = "a\n```\nb";
  const result = edit("before " + selected + " after", 7, 7 + selected.length, "code");
  expect(result.doc.toString()).toBe("before \n````\n" + selected + "\n````\n after");
  expect(result.sliceDoc(result.selection.main.from, result.selection.main.to)).toBe(selected);
});

test("빈 코드 블록은 입력할 텍스트를 선택한다", () => {
  const result = edit("", 0, 0, "code");
  expect(result.doc.toString()).toBe("```\n코드\n```");
  expect(result.sliceDoc(result.selection.main.from, result.selection.main.to)).toBe("코드");
});

test.each(["선택한😀문장", ""])("링크는 선택 텍스트를 보존하고 다음 입력 위치를 선택한다 (%s)", (text) => {
  const result = edit(text, 0, text.length, "link");
  expect(result.doc.toString()).toBe(`[${text || "텍스트"}](https://)`);
  expect(result.sliceDoc(result.selection.main.from, result.selection.main.to)).toBe(text ? "https://" : "텍스트");
});

test.each(["a]b", "a[b", "[draft]", "C:\\notes\\", "한글\\[초안]😀"])("링크 이름의 대괄호와 역슬래시는 표시 텍스트와 URL 선택을 보존한다 (%s)", (text) => {
  const original = `앞 ${text} 뒤`;
  const result = edit(original, 2, 2 + text.length, "link");
  const selected = result.selection.main;
  expect(result.sliceDoc(selected.from, selected.to)).toBe("https://");
  const source = result.update({ changes: { from: selected.from, to: selected.to, insert: "https://example.invalid/" } }).state.doc.toString();
  const html = renderToStaticMarkup(createElement(Markdown, { children: source }));
  expect(html).toBe(`<p>앞 <a href="https://example.invalid/">${text}</a> 뒤</p>`);
});

test("도구 삽입은 앞선 입력과 분리된 한 번의 undo로 되돌릴 수 있다", () => {
  let state = EditorState.create({ doc: "base", extensions: history() });
  state = state.update({ changes: { from: 4, insert: "typed" }, selection: { anchor: 9 }, userEvent: "input.type" }).state;
  state = state.update(markdownEdit(state, "heading")!).state;
  expect(undo({ state, dispatch: (transaction) => { state = transaction.state; } })).toBe(true);
  expect(state.doc.toString()).toBe("basetyped");
  expect(undo({ state, dispatch: (transaction) => { state = transaction.state; } })).toBe(true);
  expect(state.doc.toString()).toBe("base");
});

test("삽입도 기존 BOM 포함 5 MiB 제한을 통과해야 하며 거절하면 원문과 선택을 보존한다", () => {
  const rejected = vi.fn();
  const text = "x".repeat(5 * 1024 * 1024 - 3);
  const state = EditorState.create({ doc: text, selection: { anchor: 5 }, extensions: documentSizeGuard(true, rejected) });
  const spec = markdownEdit(state, "heading");
  expect(spec).not.toBeNull();
  const result = state.update(spec!);
  expect(rejected).toHaveBeenCalledOnce();
  expect(result.docChanged).toBe(false);
  expect(result.state.doc.toString()).toBe(text);
  expect(result.state.selection.eq(state.selection)).toBe(true);
});

test("읽기 전용 문서는 도구로 변경할 수 없다", () => {
  const state = EditorState.create({ doc: "readonly", extensions: EditorState.readOnly.of(true) });
  for (const action of ["heading", "list", "code", "link"] as const) expect(markdownEdit(state, action)).toBeNull();
});
