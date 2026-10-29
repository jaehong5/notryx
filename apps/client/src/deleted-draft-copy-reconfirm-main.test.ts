// @vitest-environment jsdom
import { EditorView } from "@codemirror/view";
import type { WorkspaceDeletedDraftCopyStatus, WorkspaceDraft, WorkspaceRegistration } from "../../../packages/protocol-types";
import { afterEach, beforeEach, expect, test, vi } from "vitest";

const fakes = vi.hoisted(() => ({
  listeners: new Map<string, () => void>(),
  root: undefined as { unmount(): void } | undefined,
  snapshot: vi.fn(), confirm: vi.fn(), copyStatus: vi.fn(), reopenCopy: vi.fn(), copyProgress: vi.fn(), reconfirm: vi.fn(), folders: vi.fn(), openDraft: vi.fn(), drafts: [] as WorkspaceDraft[], active: undefined as WorkspaceRegistration | undefined,
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
    progressWorkspaceDeletedDraftCopy: fakes.copyProgress, reconfirmWorkspaceDeletedDraftCopy: fakes.reconfirm, workspaceCreateParents: fakes.folders,
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
  fakes.reopenCopy.mockReset(); fakes.copyProgress.mockReset(); fakes.reconfirm.mockReset();
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
  await vi.waitFor(() => expect(document.body.textContent).toContain(copy.name));
  return EditorView.findFromDOM(document.querySelector(".cm-editor")!)!;
}

async function refreshStatus() {
  await vi.waitFor(() => expect(document.querySelector<HTMLButtonElement>('[data-testid="deleted-copy-status-refresh"]')?.disabled).toBe(false));
  const calls = fakes.copyStatus.mock.calls.length;
  document.querySelector<HTMLButtonElement>('[data-testid="deleted-copy-status-refresh"]')!.click();
  await vi.waitFor(() => expect(fakes.copyStatus.mock.calls.length).toBeGreaterThan(calls));
}


// The stored status describes the already confirmed checkpoint, not later editor input.
const failedCopy: WorkspaceDeletedDraftCopyStatus = { ...pendingCopy, create_status:"needs_review", create_failure:"destination_exists" };
const nextNode = "019a1000-0000-7000-8000-000000000008";
const nextCreate = "019a1000-0000-7000-8000-000000000009";
const nextCopy: WorkspaceDeletedDraftCopyStatus = { ...pendingCopy, name:"recovered-again.md", node_id:nextNode, create_operation_id:nextCreate };

async function openRetry() {
  await vi.waitFor(() => expect(document.querySelector('[data-testid="deleted-copy-retry"]')).not.toBeNull());
  document.querySelector<HTMLButtonElement>('[data-testid="deleted-copy-retry"]')!.click();
  await vi.waitFor(() => expect(document.body.textContent).toContain(createOperation));
  await vi.waitFor(() => expect(document.querySelector<HTMLSelectElement>('[data-testid="conflict-copy-retry-parent"]')?.options.length).toBeGreaterThan(1));
}

function setValue(selector: string, value: string) {
  const input = document.querySelector<HTMLInputElement | HTMLSelectElement>(selector)!;
  const prototype = input instanceof HTMLInputElement ? HTMLInputElement.prototype : HTMLSelectElement.prototype;
  Object.getOwnPropertyDescriptor(prototype, "value")!.set!.call(input, value);
  input.dispatchEvent(new Event(input instanceof HTMLInputElement ? "input" : "change", { bubbles:true }));
}

async function reviewDestination(name = "recovered-again.md") {
  setValue('[data-testid="conflict-copy-retry-parent"]', rootId);
  setValue('[data-testid="conflict-copy-retry-name"]', name);
  await vi.waitFor(() => expect(document.querySelector<HTMLButtonElement>('[data-testid="conflict-copy-retry-review"]')?.disabled).toBe(false));
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-copy-retry-review"]')!.click();
}

async function confirmDestination() {
  await vi.waitFor(() => expect(document.querySelector('[data-testid="conflict-copy-retry-confirm"]')).not.toBeNull());
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-copy-retry-confirm"]')!.click();
}

test("이름 충돌 사본은 실패한 생성 ID와 새 대상을 확인한 뒤에만 재승인한다", async () => {
  const view = await restorePending(failedCopy);
  fakes.reconfirm.mockResolvedValue({ status:"pending", node_id:nextNode, create_operation_id:nextCreate });
  await openRetry();
  expect(document.body.textContent).toContain(createOperation);
  await reviewDestination();
  expect(document.body.textContent).toContain("recovered-again.md");
  expect(fakes.reconfirm).not.toHaveBeenCalled();
  fakes.copyStatus.mockResolvedValue(nextCopy);
  await confirmDestination();
  await vi.waitFor(() => expect(fakes.reconfirm).toHaveBeenCalledExactlyOnceWith(binding, nodeId, createOperation, rootId, "recovered-again.md"));
  await vi.waitFor(() => expect(document.querySelector('[data-testid="conflict-copy-retry-confirm"]')).toBeNull());
  expect(document.body.textContent).not.toContain("사본 저장 완료");
  expect(fakes.copyProgress).not.toHaveBeenCalled();
  expect(view.state.doc.toString()).toBe("base");
});

test("새 대상 선택을 취소하면 기존 실패 상태와 초안을 보존한다", async () => {
  const view = await restorePending(failedCopy);
  await openRetry();
  await reviewDestination();
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-copy-retry-cancel"]')!.click();
  expect(fakes.reconfirm).not.toHaveBeenCalled();
  expect(fakes.copyProgress).not.toHaveBeenCalled();
  expect(view.state.doc.toString()).toBe("base");
  expect(document.body.textContent).not.toContain("사본 저장 완료");
});

test("기존과 같은 폴더와 이름은 재승인 전에 거절한다", async () => {
  await restorePending(failedCopy);
  await openRetry();
  await reviewDestination("recovered.md");
  await vi.waitFor(() => expect(document.body.textContent).toMatch(/이전과 다른|다른 폴더나 이름/));
  expect(document.querySelector('[data-testid="conflict-copy-retry-confirm"]')).toBeNull();
  expect(fakes.reconfirm).not.toHaveBeenCalled();
});

test("이름 충돌이 아닌 생성 실패는 재승인하지 않고 원문 보존을 안내한다", async () => {
  const view = await restorePending({ ...failedCopy, create_failure:"other" });
  await vi.waitFor(() => expect(document.body.textContent).toMatch(/실패|확인/));
  expect(document.querySelector('[data-testid="deleted-copy-retry"]')).toBeNull();
  expect(fakes.reconfirm).not.toHaveBeenCalled();
  expect(fakes.copyProgress).not.toHaveBeenCalled();
  expect(view.state.doc.toString()).toBe("base");
});

test("재승인 응답이 유실되면 상태를 다시 확인하되 자동 재요청하거나 완료로 표시하지 않는다", async () => {
  const view = await restorePending(failedCopy);
  fakes.reconfirm.mockRejectedValue(new Error("response lost"));
  await openRetry(); await reviewDestination(); await confirmDestination();
  await vi.waitFor(() => expect(document.body.textContent).toMatch(/사본.*결과.*확인하지 못/));
  fakes.copyStatus.mockResolvedValue(nextCopy);
  await refreshStatus();
  await vi.waitFor(() => expect(document.body.textContent).toContain("recovered-again.md"));
  expect(fakes.reconfirm).toHaveBeenCalledTimes(1);
  expect(fakes.copyProgress).not.toHaveBeenCalled();
  expect(document.body.textContent).not.toContain("사본 저장 완료");
  expect(view.state.doc.toString()).toBe("base");
});

test("재시작 후 재승인된 새 사본은 상태로 복원하고 자동으로 전송하지 않는다", async () => {
  const view = await restorePending(nextCopy);
  expect(document.body.textContent).toContain("recovered-again.md");
  expect(fakes.reconfirm).not.toHaveBeenCalled();
  expect(fakes.copyProgress).not.toHaveBeenCalled();
  expect(document.body.textContent).not.toContain("사본 저장 완료");
  expect(view.state.doc.toString()).toBe("base");
});

test("재승인 응답이 이전 사본 node를 재사용하면 성공으로 처리하지 않는다", async () => {
  const view = await restorePending(failedCopy);
  fakes.reconfirm.mockResolvedValue({ status:"pending", node_id:copyNode, create_operation_id:nextCreate });
  await openRetry(); await reviewDestination(); await confirmDestination();
  await vi.waitFor(() => expect(document.body.textContent).toMatch(/사본.*확인하지 못|사본.*다시 확인/));
  expect(fakes.copyProgress).not.toHaveBeenCalled();
  expect(document.body.textContent).not.toContain("사본 저장 완료");
  expect(view.state.doc.toString()).toBe("base");
});

test("재승인 응답을 기다리는 동안 등록이 교체되면 새 등록을 진행하거나 깨우지 않는다", async () => {
  await restorePending(failedCopy);
  let release!: () => void;
  fakes.reconfirm.mockImplementation(() => new Promise((resolve) => { release = () => resolve({ status:"pending", node_id:nextNode, create_operation_id:nextCreate }); }));
  await openRetry(); await reviewDestination(); await confirmDestination();
  await vi.waitFor(() => expect(fakes.reconfirm).toHaveBeenCalledTimes(1));
  fakes.active = { ...binding, workspace_epoch:"new-epoch" };
  fakes.listeners.get("workspace-sync-status-changed")?.();
  await vi.waitFor(() => expect(document.querySelector('[data-testid="conflict-copy-retry-confirm"]')).toBeNull());
  fakes.transmit.mockClear();
  release();
  await new Promise((resolve) => setTimeout(resolve, 0));
  expect(fakes.transmit).not.toHaveBeenCalled();
  expect(fakes.copyProgress).not.toHaveBeenCalled();
  expect(document.body.textContent).not.toContain("사본 저장 완료");
});

test("이미 확인한 사본을 새 대상으로 지정할 때 나중 편집을 기존 사본 원문으로 바꾸지 않는다", async () => {
  const view = await restorePending(failedCopy);
  await openRetry();
  view.dispatch({ changes:{ from:4, insert:" later" }, userEvent:"input.type" });
  expect(view.state.doc.toString()).toBe("base later");
  await reviewDestination();
  fakes.reconfirm.mockResolvedValue({ status:"pending", node_id:nextNode, create_operation_id:nextCreate });
  fakes.copyStatus.mockResolvedValue(nextCopy);
  await confirmDestination();
  await vi.waitFor(() => expect(fakes.reconfirm).toHaveBeenCalledExactlyOnceWith(binding, nodeId, createOperation, rootId, "recovered-again.md"));
  expect(view.state.doc.toString()).toBe("base later");
  expect(fakes.snapshot).not.toHaveBeenCalled();
  expect(fakes.confirm).not.toHaveBeenCalled();
});
