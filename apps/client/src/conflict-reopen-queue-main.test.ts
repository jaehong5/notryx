// @vitest-environment jsdom
import { EditorView } from "@codemirror/view";
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import type { EditorController, EditorPorts } from "./editor-controller";
import type { TransmitScope } from "./auto-transmit";
import type { WorkspaceConflictResolutionState, WorkspaceOpenedDocument } from "../../../packages/protocol-types";

const fakes = vi.hoisted(() => ({
  listeners: new Map<string, () => void>(),
  root: undefined as { unmount(): void } | undefined,
  controller: undefined as EditorController | undefined,
  ports: undefined as EditorPorts | undefined,
  reopening: false,
  openedSessions: [] as string[],
  restoreSessions: [] as string[],
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
    constructor(ports: ConstructorParameters<typeof actual.EditorController>[0]) { super(ports); fakes.controller = this; fakes.ports = ports; }
  } };
});
vi.mock("./auto-transmit", async (original) => {
  const actual = await original<typeof import("./auto-transmit")>();
  return { ...actual, AutoTransmit: class extends actual.AutoTransmit {
    openSession(scope: TransmitScope) { fakes.openedSessions.push(scope.editSessionId); return super.openSession(scope); }
    restore(scope: TransmitScope, generation: string) { fakes.restoreSessions.push(scope.editSessionId); return super.restore(scope, generation); }
  } };
});
vi.mock("./ipc", async (original) => {
  const actual = await original<typeof import("./ipc")>();
  return { ...actual,
    localDrafts: async () => [], pendingPairStatus: async () => "none",
    workspaceSyncState: async () => ({ binding: fakes.binding, status: fakes.status }),
    workspaceInventory: async () => ({ nodes: [rootNode, node], next_after_id: null }),
    openWorkspaceDocument: async () => ({ binding, document_id: nodeId, edit_session_id: fakes.reopening ? "reopened-session" : "open-session", generation: fakes.reopening ? "2" : "0", bytes_base64: btoa(fakes.reopening ? "recovered" : "base"), readonly: false, local_draft: fakes.reopening, has_session: true, cache_warning: null, open_notice: null }),
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

beforeEach(() => {
  Object.defineProperty(Range.prototype, "getClientRects", { configurable: true, value: () => [] });
  Object.defineProperty(Range.prototype, "getBoundingClientRect", { configurable: true, value: () => ({ left: 0, right: 0, top: 0, bottom: 0, width: 0, height: 0 }) });
  vi.resetModules();
  fakes.binding = binding; fakes.status = "ready"; fakes.controller = undefined; fakes.ports = undefined; fakes.reopening = false; fakes.openedSessions = []; fakes.restoreSessions = [];
  fakes.save.mockReset().mockImplementation(async (request: { edit_session_id: string; generation: string }) => ({ edit_session_id: request.edit_session_id, generation: request.generation }));
  fakes.queue.mockReset().mockImplementation(async (document_id: string, _session: string, generation: string) => ({ operation_id: "019a1000-0000-7000-8000-000000000003", document_id, generation, committed: false }));
});

afterEach(async () => {
  fakes.root?.unmount(); fakes.root = undefined; fakes.controller = undefined; fakes.ports = undefined;
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

type ReopenResult = { state: WorkspaceConflictResolutionState; document: WorkspaceOpenedDocument };
type ConflictController = EditorController & {
  reopenConflictCheckpoint(action: (documentId: string, previousSessionId: string) => Promise<ReopenResult>): Promise<WorkspaceConflictResolutionState>;
};

test.each([
  { status: "completed", generation: "2", frozen: false },
  { status: "pending", version: "0", generation: "2", choice: "choose_remote", needs_review: false, frozen: false },
  { status: "not_found" },
] as WorkspaceConflictResolutionState[])("충돌 복구 $status session은 일반 초안 복원 queue를 자동 실행하지 않는다", async (state) => {
  const { controller } = await openEditor();
  const shown = controller.recoverySnapshot();
  if (state.status === "completed") await controller.withConflictSnapshot(shown, async () => ({ status: "completed", generation: "2" }));
  else await expect(controller.withConflictSnapshot(shown, async () => { throw new Error("response lost"); })).rejects.toThrow();
  fakes.queue.mockClear();
  fakes.reopening = true;
  const reopened = await (controller as ConflictController).reopenConflictCheckpoint(async (id) => {
    const document = await fakes.ports!.openDocument(id);
    return { state, document: document as WorkspaceOpenedDocument };
  });
  expect(reopened).toEqual(state);
  expect(fakes.openedSessions).toContain("reopened-session");
  expect(fakes.restoreSessions).not.toContain("reopened-session");
  expect(fakes.queue).not.toHaveBeenCalled();
  const view = EditorView.findFromDOM(document.querySelector(".cm-editor")!)!;
  expect(view.state.doc.toString()).toBe("recovered");
  expect(controller.recoverySnapshot().session).toBe("reopened-session");
});

test("일반 localDraft 열기는 기존 자동 queue를 유지한다", async () => {
  fakes.reopening = true;
  await openEditor();
  await vi.waitFor(() => expect(fakes.openedSessions).toContain("reopened-session"));
  expect(fakes.restoreSessions).toContain("reopened-session");
  await vi.waitFor(() => expect(fakes.queue).toHaveBeenCalled());
});

test("완료된 복구 session의 새 입력은 저장 뒤 새 generation을 queue한다", async () => {
  const { controller } = await openEditor();
  const shown = controller.recoverySnapshot();
  await controller.withConflictSnapshot(shown, async () => ({ status: "completed", generation: "2" }));
  fakes.queue.mockClear();
  fakes.reopening = true;
  await (controller as ConflictController).reopenConflictCheckpoint(async (id) => {
    const document = await fakes.ports!.openDocument(id);
    return { state: { status: "completed", generation: "2", frozen: false }, document: document as WorkspaceOpenedDocument };
  });
  expect(fakes.queue).not.toHaveBeenCalled();
  const view = EditorView.findFromDOM(document.querySelector(".cm-editor")!)!;
  view.dispatch({ changes: { from: view.state.doc.length, insert: " new" }, userEvent: "input.type" });
  await controller.save();
  expect(fakes.queue).toHaveBeenCalledWith(nodeId, "reopened-session", "3");
  expect(view.state.doc.toString()).toBe("recovered new");
});
