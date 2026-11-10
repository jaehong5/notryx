import { isolateHistory } from "@codemirror/commands";
import { EditorSelection, type ChangeSpec, type EditorState, type TransactionSpec } from "@codemirror/state";

export type MarkdownAction = "heading" | "list" | "code" | "link";

export function markdownEdit(state: EditorState, action: MarkdownAction): TransactionSpec | null {
  if (state.readOnly) return null;
  const range = state.selection.main;
  let spec: TransactionSpec;
  if (action === "heading" || action === "list") {
    const first = state.doc.lineAt(range.from);
    const end = state.doc.lineAt(range.to);
    const last = !range.empty && range.to === end.from ? end.number - 1 : end.number;
    const changes: ChangeSpec[] = [];
    for (let line = first.number; line <= last; line++) {
      changes.push({ from: state.doc.line(line).from, insert: action === "heading" ? "## " : "- " });
    }
    const changeSet = state.changes(changes);
    spec = { changes: changeSet, selection: EditorSelection.single(
      changeSet.mapPos(range.anchor, 1), changeSet.mapPos(range.head, 1),
    ) };
  } else {
    const selected = state.sliceDoc(range.from, range.to);
    let insert: string, anchor: number, head: number;
    if (action === "link") {
      const label = (selected || "텍스트").replace(/[\\[\]]/gu, "\\$&");
      insert = `[${label}](https://)`;
      anchor = range.from + (selected ? label.length + 3 : 1);
      head = anchor + (selected ? "https://".length : label.length);
    } else {
      const text = selected || "코드";
      let fenceLength = 3;
      for (const run of text.matchAll(/`+/g)) fenceLength = Math.max(fenceLength, run[0].length + 1);
      const fence = "`".repeat(fenceLength);
      const before = range.from === state.doc.lineAt(range.from).from ? "" : "\n";
      const after = range.to === state.doc.lineAt(range.to).to ? "" : "\n";
      insert = `${before}${fence}\n${text}${text.endsWith("\n") ? "" : "\n"}${fence}${after}`;
      anchor = range.from + before.length + fence.length + 1;
      head = anchor + text.length;
    }
    spec = { changes: { from: range.from, to: range.to, insert }, selection: EditorSelection.single(anchor, head) };
  }
  return { ...spec, annotations: isolateHistory.of("full"), userEvent: "input.markdown", scrollIntoView: true };
}
