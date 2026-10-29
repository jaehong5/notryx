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

async function clickProgress() {
  await vi.waitFor(() => expect(document.querySelector('[data-testid="deleted-copy-progress"]')).not.toBeNull());
  document.querySelector<HTMLButtonElement>('[data-testid="deleted-copy-progress"]')!.click();
  await vi.waitFor(() => expect(fakes.copyProgress).toHaveBeenCalled());
}

async function refreshStatus() {
  await vi.waitFor(() => expect(document.querySelector<HTMLButtonElement>('[data-testid="deleted-copy-status-refresh"]')?.disabled).toBe(false));
  const calls = fakes.copyStatus.mock.calls.length;
  document.querySelector<HTMLButtonElement>('[data-testid="deleted-copy-status-refresh"]')!.click();
  await vi.waitFor(() => expect(fakes.copyStatus.mock.calls.length).toBeGreaterThan(calls));
}

test("재시작 후 저장된 사본 대상은 복원하지만 명시 진행 전 작업을 만들지 않는다", async () => {
  const view = await restorePending();
  expect(fakes.confirm).not.toHaveBeenCalled();
  expect(fakes.copyProgress).not.toHaveBeenCalled();
  expect(document.body.textContent).not.toContain("사본 저장 완료");
  expect(view.state.doc.toString()).toBe("base");
});

test("명시 진행은 고정된 새 node와 create ID를 사용하고 서버 전송을 깨운다", async () => {
  await restorePending();
  fakes.copyProgress.mockResolvedValue({ operation_id:createOperation });
  fakes.transmit.mockClear();
  await clickProgress();
  expect(fakes.copyProgress).toHaveBeenCalledExactlyOnceWith(binding, nodeId, copyNode, createOperation);
  await vi.waitFor(() => expect(fakes.transmit).toHaveBeenCalled(), { timeout:2500 });
  expect(document.body.textContent).not.toContain("사본 저장 완료");
  expect(fakes.confirm).not.toHaveBeenCalled();
});

test("빈 사본 생성 이후 본문이 전송 대기인 동안 저장 완료라고 표시하지 않는다", async () => {
  const copy: WorkspaceDeletedDraftCopyStatus = { ...pendingCopy, create_status:"created", content_status:"pending", content_operation_id:contentOperation };
  const view = await restorePending(copy);
  expect(document.body.textContent).toMatch(/사본.*(대기|보류|확인 중)/);
  expect(document.body.textContent).not.toContain("사본 저장 완료");
  expect(fakes.copyProgress).not.toHaveBeenCalled();
  expect(view.state.doc.toString()).toBe("base");
});

test("본문 receipt와 서버 ACK가 검증된 saved에서만 사본 저장 완료를 표시한다", async () => {
  const copy: WorkspaceDeletedDraftCopyStatus = { ...pendingCopy, create_status:"created", content_status:"saved", content_operation_id:contentOperation };
  const view = await restorePending(copy);
  await vi.waitFor(() => expect(document.body.textContent).toContain("사본 저장 완료"));
  expect(document.body.textContent).toMatch(/원래.*삭제|삭제.*유지/);
  expect(fakes.confirm).not.toHaveBeenCalled(); expect(fakes.copyProgress).not.toHaveBeenCalled();
  expect(view.state.doc.toString()).toBe("base");
});

test("대상 이름 충돌은 다른 사본을 자동 생성하지 않고 원문 보존을 안내한다", async () => {
  const copy: WorkspaceDeletedDraftCopyStatus = { ...pendingCopy, create_status:"needs_review", create_failure:"destination_exists" };
  const view = await restorePending(copy);
  await vi.waitFor(() => expect(document.body.textContent).toMatch(/같은 이름|이름.*충돌/));
  expect(document.body.textContent).not.toContain("사본 저장 완료");
  expect(fakes.confirm).not.toHaveBeenCalled(); expect(fakes.copyProgress).not.toHaveBeenCalled();
  expect(view.state.doc.toString()).toBe("base");
});

test("진행 결과가 불명이면 상태를 조회할 때까지 재전송이나 완료를 추정하지 않는다", async () => {
  const view = await restorePending();
  fakes.copyProgress.mockRejectedValue(new Error("response lost"));
  await clickProgress();
  await vi.waitFor(() => expect(document.body.textContent).toMatch(/사본.*결과.*확인하지 못/));
  expect(document.body.textContent).not.toContain("사본 저장 완료");
  fakes.copyStatus.mockResolvedValue({ ...pendingCopy, create_status:"pending" });
  await refreshStatus();
  expect(fakes.copyStatus).toHaveBeenLastCalledWith(binding, nodeId);
  expect(fakes.copyProgress).toHaveBeenCalledTimes(1);
  expect(view.state.doc.toString()).toBe("base");
});

test("다른 등록의 상태 응답은 사본 저장 완료로 표시하지 않는다", async () => {
  fakes.deleted = true; fakes.inspect.mockResolvedValue("deleted");
  fakes.drafts = [{ binding, document_id:nodeId, generation:"1", current_registration:true, has_session:true }];
  fakes.copyStatus.mockResolvedValue({ ...pendingCopy, binding:{ ...binding, workspace_epoch:"other" }, create_status:"created", content_status:"saved", content_operation_id:contentOperation });
  document.body.innerHTML = '<div id="root"></div>';
  await import("./main");
  await vi.waitFor(() => expect(document.querySelector(`[data-draft-document="${nodeId}"]`)).not.toBeNull());
  document.querySelector<HTMLButtonElement>(`[data-draft-document="${nodeId}"]`)!.click();
  await vi.waitFor(() => expect(fakes.copyStatus).toHaveBeenCalled());
  await vi.waitFor(() => expect(document.body.textContent).toMatch(/사본.*확인하지 못|사본.*다시 확인/));
  expect(document.body.textContent).not.toContain("사본 저장 완료");
  expect(fakes.copyProgress).not.toHaveBeenCalled();
});

test("진행 응답을 기다리는 동안 등록이 바뀌면 새 등록의 전송을 깨우지 않는다", async () => {
  await restorePending();
  let release!: () => void;
  fakes.copyProgress.mockImplementation(() => new Promise((resolve) => { release = () => resolve({ operation_id:createOperation }); }));
  await clickProgress();
  fakes.active = { ...binding, workspace_epoch:"new-epoch" };
  fakes.listeners.get("workspace-sync-status-changed")?.();
  await vi.waitFor(() => expect(document.querySelector('[data-testid="deleted-copy-progress"]')).toBeNull());
  fakes.transmit.mockClear(); release();
  await new Promise((resolve) => setTimeout(resolve, 0));
  expect(fakes.transmit).not.toHaveBeenCalled();
  expect(document.body.textContent).not.toContain("사본 저장 완료");
});
