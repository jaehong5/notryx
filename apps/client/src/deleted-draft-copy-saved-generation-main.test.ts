// @vitest-environment jsdom
import { EditorView } from "@codemirror/view";
import type { WorkspaceDeletedDraftCopyStatus, WorkspaceDraft, WorkspaceRegistration } from "../../../packages/protocol-types";
import { afterEach, beforeEach, expect, test, vi } from "vitest";

const fakes = vi.hoisted(() => ({
  listeners: new Map<string, () => void>(),
  root: undefined as { unmount(): void } | undefined,
  snapshot: vi.fn(), confirm: vi.fn(), copyStatus: vi.fn(), reopenCopy: vi.fn(), copyProgress: vi.fn(), folders: vi.fn(), openDraft: vi.fn(), drafts: [] as WorkspaceDraft[], active: undefined as WorkspaceRegistration | undefined,
  prepare: vi.fn(), commit: vi.fn(), inspect: vi.fn(), save: vi.fn(), queue: vi.fn(), transmit: vi.fn(), pair: vi.fn(), recover: vi.fn(), outbox: vi.fn(), generation:"0", deleted: false, close:vi.fn(),
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
    localDrafts: async () => fakes.drafts, openLocalDraft: fakes.openDraft, pendingPairStatus: async () => "none",
    workspaceSyncState: async () => ({ binding:fakes.active, status: "ready" }),
    workspaceInventory: async () => ({ nodes: fakes.deleted ? [rootNode] : [rootNode, node], next_after_id: null }),
    openWorkspaceDocument: async () => ({ binding, document_id: nodeId, edit_session_id: "open-session", generation: fakes.generation, bytes_base64: btoa("base"), readonly: false, local_draft: false, has_session: true, cache_warning: null, open_notice: null }),
    workspaceDocumentOutbox: fakes.outbox,
    prepareWorkspaceRemoteRefresh: fakes.prepare, commitWorkspaceRemoteRefresh: fakes.commit,
    inspectWorkspaceRemoteChange: fakes.inspect, pairWorkspace: fakes.pair,
    saveWorkspaceDocument: fakes.save, queueWorkspaceDocument: fakes.queue,
    sendNextWorkspaceOperation: fakes.transmit,
    closeWorkspaceDocument: fakes.close, recoverWorkspaceSource: fakes.recover,
    prepareWorkspaceDeletedDraftCopy: fakes.snapshot, confirmWorkspaceDeletedDraftCopy: fakes.confirm,
    workspaceDeletedDraftCopyStatus: fakes.copyStatus, reopenWorkspaceDeletedDraftCopy: fakes.reopenCopy,
    progressWorkspaceDeletedDraftCopy: fakes.copyProgress, workspaceCreateParents: fakes.folders,
  };
});

const binding = { server_origin: "https://example.invalid", workspace_id: "workspace", workspace_epoch: "epoch", device_id: "device", device_stream_id: "stream" };
const rootId = "019a1000-0000-7000-8000-000000000001", nodeId = "019a1000-0000-7000-8000-000000000002";
const rootNode = { node_id: rootId, parent_id: null, name: "", kind: "folder" as const, deleted: false, content_rev: null, content_etag: null, meta_rev: "meta", meta_etag: '"meta"', node_etag: '"node"', content_hash: null, size: null, children_generation: "0", last_change_seq: "1" };
const node = { ...rootNode, node_id: nodeId, parent_id: rootId, name: "synthetic.md", kind: "file" as const, content_rev: "content", content_etag: '"content"', content_hash: "sha256:" + "a".repeat(64), size: "4" };
// The native snapshot contains a hash, not local bytes; reopening is still a provisional port.
const copyNode = "019a1000-0000-7000-8000-000000000005";
const createOperation = "019a1000-0000-7000-8000-000000000006";
const snapshot = { binding, document_id:nodeId, edit_session_id:"open-session", generation:"1", local_hash:"sha256:cae662172fd450bb0cd710a769079c05bfc5d8e35efa6576edc7d0377afdd4a2", tombstone_seq:"3" };
const pendingCopy: WorkspaceDeletedDraftCopyStatus = { binding, document_id:nodeId, generation:"1", local_hash:snapshot.local_hash, tombstone_seq:"3", terminal_operation_id:null, parent_id:rootId, name:"recovered.md", node_id:copyNode, create_operation_id:createOperation, create_status:"not_queued", create_failure:null, content_status:"not_queued", content_operation_id:null, blocking_followup:null };
const rangeRects = Object.getOwnPropertyDescriptor(Range.prototype, "getClientRects");
const rangeBounds = Object.getOwnPropertyDescriptor(Range.prototype, "getBoundingClientRect");

beforeEach(() => {
  Object.defineProperty(Range.prototype, "getClientRects", { configurable: true, value: () => [] });
  Object.defineProperty(Range.prototype, "getBoundingClientRect", { configurable: true, value: () => ({ left: 0, right: 0, top: 0, bottom: 0, width: 0, height: 0 }) });
  vi.resetModules();
  fakes.deleted = false; fakes.active = binding; fakes.drafts = [];
  fakes.generation = "1";
  fakes.snapshot.mockReset().mockResolvedValue(snapshot);
  fakes.confirm.mockReset().mockResolvedValue({ status:"pending", node_id:copyNode, create_operation_id:createOperation });
  fakes.copyStatus.mockReset().mockResolvedValue(null);
  fakes.reopenCopy.mockReset(); fakes.copyProgress.mockReset();
  fakes.folders.mockReset().mockResolvedValue({ nodes:[rootNode], next_after_id:null });
  fakes.openDraft.mockReset().mockResolvedValue({ binding, document_id:nodeId, edit_session_id:"open-session", generation:"1", bytes_base64:btoa("base"), readonly:false, local_draft:true, has_session:true, cache_warning:null, open_notice:null });
  fakes.outbox.mockReset().mockResolvedValue({ binding, document_id:nodeId, first_content_conflict:null, has_conflict:false, has_failure:false, latest:null, confirmed_through:"0" });
  fakes.close.mockReset().mockResolvedValue(undefined);
  fakes.recover.mockReset().mockResolvedValue(true);
  fakes.prepare.mockReset().mockResolvedValue(null);
  fakes.commit.mockReset().mockResolvedValue(true);
  fakes.inspect.mockReset().mockResolvedValue("same");
  fakes.save.mockReset().mockImplementation(async (request: { edit_session_id: string; generation: string }) => ({ edit_session_id: request.edit_session_id, generation: request.generation }));
  fakes.queue.mockReset().mockImplementation(async (document_id:string, _session:string, generation:string) => ({ operation_id:"019a1000-0000-7000-8000-000000000004", document_id, generation, committed:false }));
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

const contentOperation = "019a1000-0000-7000-8000-000000000007";

async function restorePending(copy: WorkspaceDeletedDraftCopyStatus = pendingCopy): Promise<EditorView> {
  fakes.deleted = true; fakes.inspect.mockResolvedValue("deleted");
  fakes.drafts = [{ binding, document_id:nodeId, generation:"1", current_registration:true, has_session:true }];
  fakes.copyStatus.mockResolvedValue(copy);
  document.body.innerHTML = '<div id="root"></div>';
  await import("./main");
  await vi.waitFor(() => expect(document.querySelector(`[data-draft-document="${nodeId}"]`)).not.toBeNull());
  document.querySelector<HTMLButtonElement>(`[data-draft-document="${nodeId}"]`)!.click();
  await vi.waitFor(() => expect(fakes.openDraft).toHaveBeenCalled());
  await vi.waitFor(() => expect(fakes.copyStatus).toHaveBeenCalledWith(binding, nodeId));
  await vi.waitFor(() => expect(document.body.textContent).toContain("recovered.md"));
  return EditorView.findFromDOM(document.querySelector(".cm-editor")!)!;
}

test("사본 저장 완료는 확인된 초안 generation을 표시하고 나중 편집은 별도로 남긴다", async () => {
  const saved: WorkspaceDeletedDraftCopyStatus = { ...pendingCopy, generation:"1", create_status:"created", content_status:"saved", content_operation_id:contentOperation };
  const view = await restorePending(saved);
  view.dispatch({ changes:{ from:4, insert:" later" }, userEvent:"input.type" });
  await vi.waitFor(() => expect(view.state.doc.toString()).toBe("base later"));
  await vi.waitFor(() => expect(document.body.textContent).toContain("사본 저장 완료"));
  const status = document.querySelector('[aria-label="삭제 초안 사본 상태"]');
  expect(status?.textContent).toMatch(/1/);
  expect(status?.textContent).toMatch(/확인한 초안|확인 당시/);
  expect(status?.textContent).toMatch(/이후 편집|나중 편집/);
  expect(fakes.copyProgress).not.toHaveBeenCalled();
});
