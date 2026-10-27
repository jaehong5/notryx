import { describe, expect, test } from "vitest";
import { EditorState } from "@codemirror/state";
import { openSource, saveSource } from "./source";

const encode = (text: string) => new TextEncoder().encode(text);

describe("AT-01 to AT-05 source preservation", () => {
  test("generated Unicode documents retain bytes through CodeMirror", () => {
    let seed = 173;
    const next = () => { seed = (seed * 1664525 + 1013904223) >>> 0; return seed; };
    const pieces = ["한글", "😀", "e\u0301", "[[문서]]", "<!-- unknown -->", "```custom", "---", " ", "\t"];
    for (let sample = 0; sample < 256; sample++) {
      const newline = next() % 2 ? "\n" : "\r\n";
      let text = next() % 3 === 0 ? "\uFEFF" : "";
      for (let line = 0; line < 12; line++) text += pieces[next() % pieces.length] + newline;
      if (next() % 2) text = text.slice(0, -newline.length);
      const original = encode(text);
      const source = openSource(original);
      const editor = EditorState.create({ doc: source.text });
      expect(saveSource(source, editor.doc.toString())).toEqual(original);
    }
  });

  for (const text of ["", "# 한글\n", "# 한글", "\uFEFF# 한글\r\n", "---\r\ncustom: yes\r\n---\r\n```unknown\r\n[[자료]]\r\n```\r\n<!-- keep -->"] ) {
    test(`unchanged bytes round trip ${JSON.stringify(text)}`, () => {
      const bytes = encode(text);
      const source = openSource(bytes);
      const state = EditorState.create({ doc: source.text });
      expect(saveSource(source, state.doc.toString())).toEqual(bytes);
    });
  }

  test("edits use LF and retain BOM, unknown syntax and final newline", () => {
    const source = openSource(encode("\uFEFF---\r\nx: 1\r\n---\r\n한글😀e\u0301\r\n<!-- keep -->"));
    const state = EditorState.create({ doc: source.text });
    const position = state.doc.toString().indexOf("😀") + 2;
    const changed = state.update({ changes: { from: position, insert: "추가" } }).state;
    expect(saveSource(source, changed.doc.toString())).toEqual(encode("\uFEFF---\nx: 1\n---\n한글😀추가e\u0301\n<!-- keep -->"));
  });

  test("mixed newlines preserve unmodified bytes and use LF after editing", () => {
    const bytes = encode("a\r\nb\nc\r\n");
    const source = openSource(bytes);
    expect(source.readonly).toBe(false);
    expect(saveSource(source, source.text)).toEqual(bytes);
    expect(saveSource(source, source.text + "x")).toEqual(encode("a\nb\nc\nx"));
  });

  test("invalid UTF-8 is read only and preserves original bytes", () => {
    const bytes = new Uint8Array([0xff, 0xfe, 0x00, 0xd8]);
    const source = openSource(bytes);
    expect(source.readonly).toBe(true);
    expect(saveSource(source, source.text)).toEqual(bytes);
    expect(() => saveSource(source, "replacement")).toThrow();
  });

  test("lone carriage returns preserve unmodified bytes and use LF after editing", () => {
    const source = openSource(encode("a\rb"));
    expect(source.readonly).toBe(false);
    expect(saveSource(source, source.text)).toEqual(encode("a\rb"));
    expect(saveSource(source, source.text + "\nc")).toEqual(encode("a\nb\nc"));
  });

  test("resuming a one-line checkpoint uses the same LF policy as the original session", () => {
    const original = openSource(encode("a\r\nb\r\n"));
    const checkpoint = saveSource(original, "ab");
    const resumed = openSource(checkpoint);
    expect(saveSource(original, "ab\nc")).toEqual(encode("ab\nc"));
    expect(saveSource(resumed, "ab\nc")).toEqual(encode("ab\nc"));
  });

  test("lone surrogate edits are rejected before UTF-8 replacement", () => {
    const source = openSource(encode("safe"));
    expect(() => saveSource(source, "bad\ud800")).toThrow();
  });

  test("large-document limit is measured in bytes", () => {
    const source = openSource(encode(""));
    expect(() => saveSource(source, "가".repeat(2 * 1024 * 1024))).toThrow();
  });

  test("the 5 MiB UTF-8 byte boundary separates editing from read-only viewing", () => {
    const text = "x".repeat(5 * 1024 * 1024 - 3) + "가";
    const bytes = encode(text);
    expect(bytes.length).toBe(5 * 1024 * 1024);
    const editable = openSource(bytes);
    expect(editable.readonly).toBe(false);
    const checkpoint = saveSource(editable, text, true);
    expect(checkpoint.length).toBe(bytes.length);
    expect(checkpoint.every((byte, index) => byte === bytes[index])).toBe(true);
    expect(() => saveSource(editable, text + "x", true)).toThrow();

    const larger = encode(text + "x");
    const readonly = openSource(larger);
    expect(readonly.readonly).toBe(true);
    const unchanged = saveSource(readonly, readonly.text);
    expect(unchanged.length).toBe(larger.length);
    expect(unchanged.every((byte, index) => byte === larger[index])).toBe(true);
    expect(() => saveSource(readonly, readonly.text + "x", true)).toThrow();
  });

  test("the 5 MiB editing limit includes the UTF-8 BOM", () => {
    const bytes = new Uint8Array(5 * 1024 * 1024);
    bytes.set([0xef, 0xbb, 0xbf]);
    bytes.fill(0x61, 3);
    const editable = openSource(bytes);
    expect(editable.bom).toBe(true);
    expect(editable.readonly).toBe(false);
    const checkpoint = saveSource(editable, editable.text, true);
    expect(checkpoint.length).toBe(bytes.length);
    expect(checkpoint.every((byte, index) => byte === bytes[index])).toBe(true);
    expect(() => saveSource(editable, editable.text + "x", true)).toThrow();

    const larger = new Uint8Array(bytes.length + 1);
    larger.set(bytes);
    larger[bytes.length] = 0x61;
    const readonly = openSource(larger);
    expect(readonly.bom).toBe(true);
    expect(readonly.readonly).toBe(true);
    const unchanged = saveSource(readonly, readonly.text);
    expect(unchanged.length).toBe(larger.length);
    expect(unchanged.every((byte, index) => byte === larger[index])).toBe(true);
  });
});
