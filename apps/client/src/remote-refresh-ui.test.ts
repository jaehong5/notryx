// @vitest-environment jsdom
import { undo } from "@codemirror/commands";
import { EditorView } from "@codemirror/view";
import { afterEach, beforeEach, expect, test, vi } from "vitest";

const fakes = vi.hoisted(() => ({
  listeners: new Map<string, () => void>(),
  root: undefined as { unmount(): void } | undefined,
  prepare: vi.fn(), commit: vi.fn(), inspect: vi.fn(), save: vi.fn(), queue: vi.fn(), transmit: vi.fn(), pair: vi.fn(),
}));
vi.mock("react-dom/client", async (original) => {
  const actual = await original<typeof import("react-dom/client")>();
  return { ...actual, createRoot: (...args: Parameters<typeof actual.createRoot>) => { const root = actual.createRoot(...args); fakes.root = root; return root; } };
});
vi.mock("@tauri-apps/api/event", () => ({ listen: async (name: string, callback: () => void) => {
  fakes.listeners.set(name, callback);
  return () => { fakes.listeners.delete(name); };
} }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
vi.mock("./migration-recovery", () => ({ StartupGate: ({ ready }: { ready: () => unknown }) => ready() }));
vi.mock("./ipc", async (original) => {
  const actual = await original<typeof import("./ipc")>();
  return { ...actual,
    localDrafts: async () => [], pendingPairStatus: async () => "none",
    workspaceSyncState: async () => ({ binding, status: "ready" }),
    workspaceInventory: async () => ({ nodes: [rootNode, node], next_after_id: null }),
    openWorkspaceDocument: async () => ({ binding, document_id: nodeId, edit_session_id: "open-session", generation: "0", bytes_base64: btoa("base"), readonly: false, local_draft: false, has_session: true, cache_warning: null, open_notice: null }),
    workspaceDocumentOutbox: async () => ({ binding, document_id: nodeId, first_content_conflict:null, has_conflict: false, has_failure: false, latest: null, confirmed_through: "0" }),
    prepareWorkspaceRemoteRefresh: fakes.prepare, commitWorkspaceRemoteRefresh: fakes.commit,
    inspectWorkspaceRemoteChange: fakes.inspect, pairWorkspace: fakes.pair,
    saveWorkspaceDocument: fakes.save, queueWorkspaceDocument: fakes.queue,
    sendNextWorkspaceOperation: fakes.transmit,
    closeWorkspaceDocument: async () => {},
  };
});

const binding = { server_origin: "https://example.invalid", workspace_id: "workspace", workspace_epoch: "epoch", device_id: "device", device_stream_id: "stream" };
const rootId = "019a1000-0000-7000-8000-000000000001", nodeId = "019a1000-0000-7000-8000-000000000002";
const rootNode = { node_id: rootId, parent_id: null, name: "", kind: "folder" as const, deleted: false, content_rev: null, content_etag: null, meta_rev: "meta", meta_etag: '"meta"', node_etag: '"node"', content_hash: null, size: null, children_generation: "0", last_change_seq: "1" };
const node = { ...rootNode, node_id: nodeId, parent_id: rootId, name: "synthetic.md", kind: "file" as const, content_rev: "content", content_etag: '"content"', content_hash: "sha256:" + "a".repeat(64), size: "4" };
const rangeRects = Object.getOwnPropertyDescriptor(Range.prototype, "getClientRects");
const rangeBounds = Object.getOwnPropertyDescriptor(Range.prototype, "getBoundingClientRect");

beforeEach(() => {
  Object.defineProperty(Range.prototype, "getClientRects", { configurable: true, value: () => [] });
  Object.defineProperty(Range.prototype, "getBoundingClientRect", { configurable: true, value: () => ({ left: 0, right: 0, top: 0, bottom: 0, width: 0, height: 0 }) });
  vi.resetModules();
  fakes.prepare.mockReset().mockResolvedValue(null);
  fakes.commit.mockReset().mockResolvedValue(true);
  fakes.inspect.mockReset().mockResolvedValue("same");
  fakes.save.mockReset().mockImplementation(async (request: { edit_session_id: string; generation: string }) => ({ edit_session_id: request.edit_session_id, generation: request.generation }));
  fakes.queue.mockReset();
  fakes.transmit.mockReset().mockResolvedValue({ status: "idle", binding });
  fakes.pair.mockReset().mockResolvedValue(undefined);
});

afterEach(async () => {
  fakes.root?.unmount(); fakes.root = undefined;
  await new Promise((resolve) => setTimeout(resolve, 0));
  fakes.listeners.clear(); document.body.replaceChildren();
  if (rangeRects) Object.defineProperty(Range.prototype, "getClientRects", rangeRects);
  else delete (Range.prototype as Partial<Range>).getClientRects;
  if (rangeBounds) Object.defineProperty(Range.prototype, "getBoundingClientRect", rangeBounds);
  else delete (Range.prototype as Partial<Range>).getBoundingClientRect;
});

async function openEditor(): Promise<EditorView> {
  document.body.innerHTML = '<div id="root"></div>';
  await import("./main");
  await vi.waitFor(() => expect(document.querySelector(`[data-document="${nodeId}"]`)).not.toBeNull());
  document.querySelector<HTMLButtonElement>(`[data-document="${nodeId}"]`)!.click();
  await vi.waitFor(() => expect(document.querySelector(".cm-editor")).not.toBeNull());
  await vi.waitFor(() => expect(fakes.prepare).toHaveBeenCalled());
  fakes.prepare.mockClear();
  return EditorView.findFromDOM(document.querySelector(".cm-editor")!)!;
}

test("remote replacement preserves selection and is not an edit or undo entry", async () => {
  const view = await openEditor();
  view.dispatch({ selection: { anchor: 1, head: 3 } });
  fakes.prepare.mockResolvedValueOnce({ token: "candidate", bytes_base64: btoa("remote text") });
  fakes.listeners.get("workspace-metadata-changed")?.();
  await vi.waitFor(() => expect(view.state.doc.toString()).toBe("remote text"));
  expect(view.state.selection.main.anchor).toBe(1);
  expect(view.state.selection.main.head).toBe(3);
  expect(undo({ state: view.state, dispatch: (transaction) => view.dispatch(transaction) })).toBe(false);
  expect(fakes.save).not.toHaveBeenCalled();
  expect(fakes.queue).not.toHaveBeenCalled();
});

test("remote BOM is included in the editing limit after replacement", async () => {
  const view = await openEditor();
  const text = "x".repeat(5 * 1024 * 1024 - 4);
  fakes.prepare.mockResolvedValueOnce({ token: "candidate", bytes_base64: btoa("\xef\xbb\xbf" + text) });
  fakes.listeners.get("workspace-metadata-changed")?.();
  await vi.waitFor(() => expect(view.state.doc.length).toBe(text.length));
  expect(fakes.save).not.toHaveBeenCalled();
  view.dispatch({ changes: { from: view.state.doc.length, insert: "y" } });
  expect(view.state.doc.length).toBe(text.length + 1);
  view.dispatch({ changes: { from: view.state.doc.length, insert: "z" } });
  expect(view.state.doc.length).toBe(text.length + 1);
});

test.each([false, true])("composition completion rechecks metadata and preserves edited input: %s", async (edit) => {
  const view = await openEditor();
  const content = document.querySelector<HTMLElement>(".cm-content")!;
  content.dispatchEvent(new CompositionEvent("compositionstart", { bubbles: true }));
  if (edit) view.dispatch({ changes: { from: view.state.doc.length, insert: "local" }, userEvent: "input.type" });
  fakes.prepare.mockResolvedValue({ token: "candidate", bytes_base64: btoa("remote") });
  fakes.listeners.get("workspace-metadata-changed")?.();
  content.dispatchEvent(new CompositionEvent("compositionend", { bubbles: true }));
  if (edit) {
    await new Promise((resolve) => setTimeout(resolve, 400));
    expect(view.state.doc.toString()).toBe("baselocal");
    expect(fakes.commit).not.toHaveBeenCalled();
  } else await vi.waitFor(() => expect(view.state.doc.toString()).toBe("remote"));
});

test("metadata and ready events update the remote notice without replacing local input", async () => {
  const view = await openEditor();
  view.dispatch({ changes: { from: view.state.doc.length, insert: " local" }, userEvent: "input.type" });
  fakes.inspect.mockResolvedValue("changed");
  fakes.listeners.get("workspace-metadata-changed")?.();
  await vi.waitFor(() => expect(document.body.textContent).toContain("다른 기기에서 변경됨"));
  expect(view.state.doc.toString()).toBe("base local");
  expect(fakes.save).not.toHaveBeenCalled();
  fakes.inspect.mockResolvedValue("same");
  fakes.listeners.get("workspace-sync-status-changed")?.();
  await vi.waitFor(() => expect(document.body.textContent).not.toContain("다른 기기에서 변경됨"));
  expect(view.state.doc.toString()).toBe("base local");
});

test("a successful local send rechecks and clears a no-longer-current remote notice", async () => {
  const view = await openEditor();
  view.dispatch({ changes: { from: view.state.doc.length, insert: " local" }, userEvent: "input.type" });
  fakes.inspect.mockResolvedValue("changed");
  fakes.listeners.get("workspace-metadata-changed")?.();
  await vi.waitFor(() => expect(document.body.textContent).toContain("다른 기기에서 변경됨"));
  fakes.queue.mockImplementation(async (documentId: string, _sessionId: string, generation: string) => ({ operation_id: "operation", document_id: documentId, generation, committed: false }));
  let committed = false;
  fakes.transmit.mockImplementation(async () => {
    if (!committed && fakes.queue.mock.calls.length) {
      committed = true;
      fakes.inspect.mockResolvedValue("same");
      return { status: "committed", binding, operation_id: "operation", document_id: nodeId, generation: "1" };
    }
    return { status: "idle", binding };
  });
  document.querySelector<HTMLButtonElement>("#save")!.click();
  await vi.waitFor(() => expect(committed).toBe(true));
  await vi.waitFor(() => expect(document.body.textContent).not.toContain("다른 기기에서 변경됨"));
  expect(view.state.doc.toString()).toBe("base local");
});

test("a delayed remote notice from a previous registration is ignored", async () => {
  await openEditor();
  fakes.inspect.mockClear();
  let resolve!: (value: string) => void;
  const previous = new Promise<string>((done) => { resolve = done; });
  fakes.inspect.mockReturnValueOnce(previous);
  fakes.listeners.get("workspace-metadata-changed")?.();
  await vi.waitFor(() => expect(fakes.inspect).toHaveBeenCalled());
  const form = document.querySelector<HTMLFormElement>(".workspace-connect")!;
  (form.elements.namedItem("origin") as HTMLInputElement).value = "https://new.example.invalid";
  (form.elements.namedItem("workspace_id") as HTMLInputElement).value = "new-workspace";
  (form.elements.namedItem("pairing_secret") as HTMLInputElement).value = "synthetic";
  form.dispatchEvent(new Event("submit", { bubbles: true, cancelable: true }));
  await vi.waitFor(() => expect(fakes.pair).toHaveBeenCalledOnce());
  resolve("changed");
  await new Promise((done) => setTimeout(done, 0));
  expect(document.body.textContent).not.toContain("다른 기기에서 변경됨");
});
