import { EditorState, Transaction } from "@codemirror/state";
import { history, undo } from "@codemirror/commands";
import { describe, expect, test, vi } from "vitest";
import { documentSizeGuard } from "./document-size-guard";

const limit = 5 * 1024 * 1024;

describe("document editing byte limit", () => {
  test("rejects a paste beyond 5 MiB before changing the document", () => {
    const rejected = vi.fn();
    const state = EditorState.create({ doc: "existing", extensions: documentSizeGuard(false, rejected) });
    const result = state.update({ changes: { from: 0, to: state.doc.length, insert: "x".repeat(64 * 1024 * 1024 + 1) }, userEvent: "input.paste" });
    expect(result.docChanged).toBe(false);
    expect(result.state.doc.toString()).toBe("existing");
    expect(rejected).toHaveBeenCalledOnce();
  });

  test("counts the original BOM and UTF-8 bytes at the exact boundary", () => {
    const rejected = vi.fn();
    const initial = "x".repeat(limit - 6);
    const state = EditorState.create({ doc: initial, extensions: documentSizeGuard(true, rejected) });
    const accepted = state.update({ changes: { from: initial.length, insert: "가" } });
    expect(accepted.docChanged).toBe(true);
    const denied = accepted.state.update({ changes: { from: accepted.state.doc.length, insert: "a" } });
    expect(denied.docChanged).toBe(false);
    expect(denied.state.doc.toString()).toBe(initial + "가");
    expect(rejected).toHaveBeenCalledOnce();
  });

  test("accounts for multiple changes in one transaction", () => {
    const rejected = vi.fn();
    const doc = "a".repeat(limit - 1);
    const state = EditorState.create({ doc, extensions: documentSizeGuard(false, rejected) });
    const result = state.update({ changes: [{ from: 0, insert: "x" }, { from: doc.length, insert: "y" }] });
    expect(result.docChanged).toBe(false);
    expect(result.state.doc.length).toBe(doc.length);
    expect(rejected).toHaveBeenCalledOnce();
  });

  test("handles a surrogate pair formed across a deletion boundary", () => {
    const rejected = vi.fn();
    const prefix = "x".repeat(limit - 8);
    const doc = prefix + "\ud83d" + "x" + "\ude00";
    const state = EditorState.create({ doc, extensions: documentSizeGuard(false, rejected) });
    const joined = state.update({ changes: { from: prefix.length + 1, to: prefix.length + 2 } });
    expect(joined.docChanged).toBe(true);
    expect(new TextEncoder().encode(joined.state.doc.toString()).length).toBe(limit - 4);
    const denied = joined.state.update({ changes: { from: joined.state.doc.length, insert: "abcde" } });
    expect(denied.docChanged).toBe(false);
    expect(rejected).toHaveBeenCalledOnce();
  });

  test("rejects splitting a valid surrogate pair when the resulting bytes exceed the limit", () => {
    const rejected = vi.fn();
    const doc = "x".repeat(limit - 4) + "😀";
    const state = EditorState.create({ doc, extensions: documentSizeGuard(false, rejected) });
    const result = state.update({ changes: { from: doc.length - 1, insert: "x" } });
    expect(result.docChanged).toBe(false);
    expect(result.state.doc.toString()).toBe(doc);
    expect(rejected).toHaveBeenCalledOnce();
  });

  test("matches a full UTF-8 count for consecutive changes near the limit", () => {
    const rejected = vi.fn();
    const doc = "x".repeat(limit - 18) + "가😀e\u0301";
    let baseline = EditorState.create({ doc });
    let guarded = EditorState.create({ doc, extensions: documentSizeGuard(true, rejected) });
    const inserts = ["가", "😀", "a", "\ud83d", "\ude00", "", "e\u0301"];
    let expectedRejected = 0;
    for (let index = 0; index < 24; index++) {
      const from = Math.max(0, baseline.doc.length - 1 - (index * 7 % 13));
      const to = Math.min(baseline.doc.length, from + index % 3);
      const changes = { from, to, insert: inserts[index % inserts.length] };
      const candidate = baseline.update({ changes });
      const allowed = new TextEncoder().encode(candidate.newDoc.toString()).length + 3 <= limit;
      const result = guarded.update({ changes });
      if (allowed) baseline = candidate.state;
      else expectedRejected++;
      guarded = result.state;
      expect(guarded.doc.toString()).toBe(baseline.doc.toString());
    }
    expect(rejected).toHaveBeenCalledTimes(expectedRejected);
    expect(expectedRejected).toBeGreaterThan(0);
  });

  test("accepts normal composition edits and undo after a rejected paste", () => {
    const rejected = vi.fn();
    let state = EditorState.create({ doc: "한", extensions: [history(), documentSizeGuard(false, rejected)] });
    state = state.update({ changes: { from: state.doc.length, insert: "글😀" }, annotations: Transaction.userEvent.of("input.type.compose") }).state;
    const denied = state.update({ changes: { from: state.doc.length, insert: "x".repeat(limit) }, userEvent: "input.paste" });
    expect(denied.docChanged).toBe(false);
    state = denied.state;
    expect(undo({ state, dispatch: (transaction) => { state = transaction.state; } })).toBe(true);
    expect(state.doc.toString()).toBe("한");
    expect(rejected).toHaveBeenCalledOnce();
  });
});
