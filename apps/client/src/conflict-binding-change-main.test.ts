// @vitest-environment jsdom
import { EditorState } from "@codemirror/state";
import { EditorView } from "@codemirror/view";
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import type { EditorController } from "./editor-controller";
import type { M0SavedCheckpoint, WorkspaceContentConflictChooseRemoteResponse } from "../../../packages/protocol-types";

const fakes = vi.hoisted(() => ({
  listeners: new Map<string, () => void>(),
  root: undefined as { unmount(): void } | undefined,
  controller: undefined as EditorController | undefined,
  binding: undefined as undefined | { server_origin: string; workspace_id: string; workspace_epoch: string; device_id: string; device_stream_id: string },
  status: "ready",
  save: vi.fn(), queue: vi.fn(),
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
vi.mock("./editor-controller", async (original) => {
  const actual = await original<typeof import("./editor-controller")>();
  return { ...actual, EditorController: class extends actual.EditorController {
    constructor(ports: ConstructorParameters<typeof actual.EditorController>[0]) { super(ports); fakes.controller = this; }
  } };
});
vi.mock("./ipc", async (original) => {
  const actual = await original<typeof import("./ipc")>();
  return { ...actual,
    localDrafts: async () => [], pendingPairStatus: async () => "none",
    workspaceSyncState: async () => ({ binding: fakes.binding, status: fakes.status }),
    workspaceInventory: async () => ({ nodes: [rootNode, node], next_after_id: null }),
    openWorkspaceDocument: async () => ({ binding, document_id: nodeId, edit_session_id: "open-session", generation: "0", bytes_base64: btoa("base"), readonly: false, local_draft: false, has_session: true, cache_warning: null, open_notice: null }),
    workspaceDocumentOutbox: async () => ({ binding, document_id: nodeId, first_content_conflict:null, has_conflict: false, has_failure: false, latest: null, confirmed_through: "0" }),
    prepareWorkspaceRemoteRefresh: async () => null, inspectWorkspaceRemoteChange: async () => "same",
    saveWorkspaceDocument: fakes.save, queueWorkspaceDocument: fakes.queue,
    sendNextWorkspaceOperation: async () => ({ status: "idle", binding }), closeWorkspaceDocument: async () => {},
  };
});

const binding = { server_origin: "https://example.invalid", workspace_id: "workspace", workspace_epoch: "epoch", device_id: "device", device_stream_id: "stream" };
const rootId = "019a1000-0000-7000-8000-000000000001", nodeId = "019a1000-0000-7000-8000-000000000002";
const rootNode = { node_id: rootId, parent_id: null, name: "", kind: "folder" as const, deleted: false, content_rev: null, content_etag: null, meta_rev: "meta", meta_etag: '"meta"', node_etag: '"node"', content_hash: null, size: null, children_generation: "0", last_change_seq: "1" };
const node = { ...rootNode, node_id: nodeId, parent_id: rootId, name: "synthetic.md", kind: "file" as const, content_rev: "content", content_etag: '"content"', content_hash: "sha256:" + "a".repeat(64), size: "4" };
const rangeRects = Object.getOwnPropertyDescriptor(Range.prototype, "getClientRects");
const rangeBounds = Object.getOwnPropertyDescriptor(Range.prototype, "getBoundingClientRect");

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((done) => { resolve = done; });
  return { promise, resolve };
}

beforeEach(() => {
  Object.defineProperty(Range.prototype, "getClientRects", { configurable: true, value: () => [] });
  Object.defineProperty(Range.prototype, "getBoundingClientRect", { configurable: true, value: () => ({ left: 0, right: 0, top: 0, bottom: 0, width: 0, height: 0 }) });
  vi.resetModules();
  fakes.binding = binding; fakes.status = "ready"; fakes.controller = undefined;
  fakes.save.mockReset().mockImplementation(async (request: { edit_session_id: string; generation: string }) => ({ edit_session_id: request.edit_session_id, generation: request.generation }));
  fakes.queue.mockReset().mockImplementation(async (document_id: string, _session: string, generation: string) => ({ operation_id: "019a1000-0000-7000-8000-000000000003", document_id, generation, committed: false }));
});

afterEach(async () => {
  fakes.root?.unmount(); fakes.root = undefined; fakes.controller = undefined;
  await new Promise((resolve) => setTimeout(resolve, 0));
  fakes.listeners.clear(); document.body.replaceChildren();
  if (rangeRects) Object.defineProperty(Range.prototype, "getClientRects", rangeRects); else delete (Range.prototype as Partial<Range>).getClientRects;
  if (rangeBounds) Object.defineProperty(Range.prototype, "getBoundingClientRect", rangeBounds); else delete (Range.prototype as Partial<Range>).getBoundingClientRect;
});

async function openEditor() {
  document.body.innerHTML = '<div id="root"></div>';
  await import("./main");
  await vi.waitFor(() => expect(document.querySelector(`[data-document="${nodeId}"]`)).not.toBeNull());
  document.querySelector<HTMLButtonElement>(`[data-document="${nodeId}"]`)!.click();
  await vi.waitFor(() => expect(document.querySelector(".cm-editor")).not.toBeNull());
  const view = EditorView.findFromDOM(document.querySelector(".cm-editor")!)!;
  view.dispatch({ changes: { from: view.state.doc.length, insert: " local" }, userEvent: "input.type" });
  return { view, controller: fakes.controller! };
}

async function replaceBinding() {
  fakes.binding = { ...binding, workspace_epoch: "new-epoch" };
  fakes.status = "connecting";
  fakes.listeners.get("workspace-sync-status-changed")?.();
  await vi.waitFor(() => expect(document.querySelector("#workspace-sync-status")?.textContent).toContain("서버 동기화 중"));
}

test("실제 sync 상태 갱신으로 등록이 바뀐 뒤 늦은 pending 응답은 이전 editor를 다시 활성화하지 않는다", async () => {
  const { view, controller } = await openEditor();
  const snapshot = controller.recoverySnapshot();
  const pending = deferred<WorkspaceContentConflictChooseRemoteResponse>();
  const action = vi.fn(() => pending.promise);
  const choosing = controller.withConflictSnapshot(snapshot, action).then(() => null, (cause: unknown) => cause);
  await vi.waitFor(() => expect(action).toHaveBeenCalledOnce());
  await replaceBinding();
  pending.resolve({ status: "pending", version: "0" });
  expect(await choosing).toBeInstanceOf(Error);
  expect(view.state.facet(EditorState.readOnly)).toBe(true);
  expect(view.state.doc.toString()).toBe("base local");
});

test("checkpoint 저장 중 sync 등록이 바뀌면 선택을 시작하지 않고 이전 editor를 다시 활성화하지 않는다", async () => {
  const saved = deferred<M0SavedCheckpoint>();
  fakes.save.mockReturnValueOnce(saved.promise);
  const { view, controller } = await openEditor();
  const snapshot = controller.recoverySnapshot();
  const action = vi.fn(async () => ({ status: "pending" as const, version: "0" }));
  const choosing = controller.withConflictSnapshot(snapshot, action).then(() => null, (cause: unknown) => cause);
  await vi.waitFor(() => expect(fakes.save).toHaveBeenCalledOnce());
  await replaceBinding();
  saved.resolve({ edit_session_id: snapshot.session, generation: snapshot.generation });
  expect(await choosing).toBeInstanceOf(Error);
  expect(action).not.toHaveBeenCalled();
  expect(view.state.facet(EditorState.readOnly)).toBe(true);
  expect(view.state.doc.toString()).toBe("base local");
});
