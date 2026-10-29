// @vitest-environment jsdom
import { EditorView } from "@codemirror/view";
import type { AutoTransmit } from "./auto-transmit";
import type { WorkspaceDeletedDraftCopyStatus, WorkspaceDraft, WorkspaceRegistration } from "../../../packages/protocol-types";
import { afterEach, beforeEach, expect, test, vi } from "vitest";

const fakes = vi.hoisted(() => ({
  listeners: new Map<string, () => void>(),
  root: undefined as { unmount(): void } | undefined,
  approve: vi.fn(), releaseHeld: vi.fn(), failRelease:false, resumeContent: vi.fn(), resumeAll: vi.fn(), resumeSync: vi.fn(), scheduler:undefined as AutoTransmit | undefined,
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
vi.mock("./auto-transmit", async (original) => {
  const actual = await original<typeof import("./auto-transmit")>();
  return { ...actual, AutoTransmit: class extends actual.AutoTransmit {
    constructor(ports: ConstructorParameters<typeof actual.AutoTransmit>[0]) { super(ports); fakes.scheduler=this; }
    releaseConflictSession(scope: Parameters<AutoTransmit["releaseConflictSession"]>[0]) {
      fakes.releaseHeld(scope);
      if (fakes.failRelease) throw new Error("synthetic release failure");
      return super.releaseConflictSession(scope);
    }
    resumeContentAfterAbandon(binding:string, operationId:string) { const result=super.resumeContentAfterAbandon(binding,operationId); fakes.resumeContent(binding,operationId,result); return result; }
    resume() { fakes.resumeAll(); return super.resume(); }
    resumeSync() { fakes.resumeSync(); return super.resumeSync(); }
  } };
});
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
    requestWorkspaceDeletedDraftCopyFollowupAbandon: fakes.approve,
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
  fakes.approve.mockReset(); fakes.releaseHeld.mockReset(); fakes.failRelease=false; fakes.resumeContent.mockReset(); fakes.resumeAll.mockReset(); fakes.resumeSync.mockReset(); fakes.scheduler=undefined;
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
  fakes.root?.unmount(); fakes.root = undefined; fakes.scheduler=undefined;
  await new Promise((resolve) => setTimeout(resolve, 0));
  fakes.listeners.clear(); document.body.replaceChildren();
  if (rangeRects) Object.defineProperty(Range.prototype, "getClientRects", rangeRects);
  else delete (Range.prototype as Partial<Range>).getClientRects;
  if (rangeBounds) Object.defineProperty(Range.prototype, "getBoundingClientRect", rangeBounds);
  else delete (Range.prototype as Partial<Range>).getBoundingClientRect;
});

async function restorePending(copy: WorkspaceDeletedDraftCopyStatus = pendingCopy): Promise<EditorView> {
  fakes.deleted = true; fakes.inspect.mockResolvedValue("deleted");
  const generation = copy.blocking_followup?.followup_generation ?? "1";
  fakes.generation = generation;
  fakes.openDraft.mockResolvedValue({ binding, document_id:nodeId, edit_session_id:"open-session", generation, bytes_base64:btoa("base"), readonly:false, local_draft:true, has_session:true, cache_warning:null, open_notice:null });
  fakes.drafts = [{ binding, document_id:nodeId, generation, current_registration:true, has_session:true }];
  fakes.copyStatus.mockResolvedValue(copy);
  document.body.innerHTML = '<div id="root"></div>';
  await import("./main");
  await vi.waitFor(() => expect(document.querySelector(`[data-draft-document="${nodeId}"]`)).not.toBeNull());
  document.querySelector<HTMLButtonElement>(`[data-draft-document="${nodeId}"]`)!.click();
  await vi.waitFor(() => expect(fakes.openDraft).toHaveBeenCalled());
  await vi.waitFor(() => expect(fakes.copyStatus).toHaveBeenCalledWith(binding, nodeId));
  await vi.waitFor(() => expect(document.body.textContent).toContain("recovered.md"));
  fakes.resumeContent.mockClear(); fakes.resumeAll.mockClear(); fakes.resumeSync.mockClear();
  return EditorView.findFromDOM(document.querySelector(".cm-editor")!)!;
}

async function refreshStatus() {
  await vi.waitFor(() => expect(document.querySelector<HTMLButtonElement>('[data-testid="deleted-copy-status-refresh"]')?.disabled).toBe(false));
  const calls = fakes.copyStatus.mock.calls.length;
  document.querySelector<HTMLButtonElement>('[data-testid="deleted-copy-status-refresh"]')!.click();
  await vi.waitFor(() => expect(fakes.copyStatus.mock.calls.length).toBeGreaterThan(calls));
}

// The confirmed copy and the later queued edit can have different generations and bytes.
const blockerOperation = "019a1000-0000-7000-8000-000000000008";
const followupOperation = "019a1000-0000-7000-8000-000000000009";
const otherFollowup = "019a1000-0000-7000-8000-00000000000a";
const followupHash = "sha256:" + "b".repeat(64);
const blockedCopy = { ...pendingCopy, blocking_followup:{ blocker_operation_id:blockerOperation, followup_operation_id:followupOperation, followup_operation_seq:"2", followup_generation:"3", followup_hash:followupHash } };

async function showApproval() {
  await vi.waitFor(() => expect(document.querySelector('[data-testid="deleted-copy-followup-open"]')).not.toBeNull());
  document.querySelector<HTMLButtonElement>('[data-testid="deleted-copy-followup-open"]')!.click();
  await vi.waitFor(() => expect(document.querySelector('[data-testid="deleted-copy-followup-approve"]')).not.toBeNull());
}

test("삭제 실패 작업과 후속 작업의 ID를 표시하고 별도 승인하기 전에는 abandon하지 않는다", async () => {
  const view = await restorePending(blockedCopy);
  await showApproval();
  expect(document.body.textContent).toContain(blockerOperation);
  expect(document.body.textContent).toContain(followupOperation);
  expect(document.body.textContent).toContain(followupHash);
  expect(document.body.textContent).toMatch(/generation\s*3/);
  expect(document.body.textContent).toMatch(/사본.*초안|초안.*사본/);
  expect(fakes.approve).not.toHaveBeenCalled(); expect(fakes.resumeContent).not.toHaveBeenCalled();
  expect(fakes.copyProgress).not.toHaveBeenCalled();
  fakes.approve.mockResolvedValue({ operation_id:followupOperation });
  document.querySelector<HTMLButtonElement>('[data-testid="deleted-copy-followup-approve"]')!.click();
  await vi.waitFor(() => expect(fakes.approve).toHaveBeenCalledExactlyOnceWith(binding, nodeId, copyNode, createOperation, blockerOperation, followupOperation, "2", "3", followupHash));
  await vi.waitFor(() => expect(fakes.resumeContent).toHaveBeenCalledWith(JSON.stringify(binding), followupOperation, expect.any(Boolean)));
  expect(fakes.resumeAll).not.toHaveBeenCalled(); expect(fakes.resumeSync).not.toHaveBeenCalled();
  expect(document.body.textContent).not.toContain("사본 저장 완료");
  expect(view.state.doc.toString()).toBe("base");
});

test("후속 작업 처리 승인을 취소하면 대기 원문과 차단을 유지한다", async () => {
  const view = await restorePending(blockedCopy); await showApproval();
  document.querySelector<HTMLButtonElement>('[data-testid="deleted-copy-followup-cancel"]')!.click();
  expect(fakes.approve).not.toHaveBeenCalled(); expect(fakes.resumeContent).not.toHaveBeenCalled();
  expect(fakes.copyProgress).not.toHaveBeenCalled(); expect(view.state.doc.toString()).toBe("base");
});

test("후속 작업 ID가 바뀌면 이전 승인 화면으로 새 작업을 처리하지 않는다", async () => {
  await restorePending(blockedCopy); await showApproval();
  const oldButton = document.querySelector<HTMLButtonElement>('[data-testid="deleted-copy-followup-approve"]')!;
  fakes.copyStatus.mockResolvedValue({ ...blockedCopy, blocking_followup:{ ...blockedCopy.blocking_followup, followup_operation_id:otherFollowup } });
  await refreshStatus();
  await vi.waitFor(() => expect(document.body.textContent).toContain(otherFollowup));
  oldButton.click(); await new Promise((resolve) => setTimeout(resolve,0));
  expect(fakes.approve).not.toHaveBeenCalled(); expect(fakes.resumeContent).not.toHaveBeenCalled();
});

test("승인한 ID와 다른 응답은 차단된 다른 작업을 재개하지 않는다", async () => {
  await restorePending(blockedCopy); await showApproval();
  fakes.approve.mockResolvedValue({ operation_id:otherFollowup });
  document.querySelector<HTMLButtonElement>('[data-testid="deleted-copy-followup-approve"]')!.click();
  await vi.waitFor(() => expect(fakes.approve).toHaveBeenCalled());
  await new Promise((resolve) => setTimeout(resolve,0));
  expect(fakes.resumeContent).not.toHaveBeenCalled();
  expect(fakes.resumeAll).not.toHaveBeenCalled(); expect(fakes.resumeSync).not.toHaveBeenCalled();
});

test("승인 응답을 기다리는 동안 등록이 바뀌면 이전 차단을 해제하지 않는다", async () => {
  await restorePending(blockedCopy); await showApproval();
  let release!: () => void;
  fakes.approve.mockImplementation(() => new Promise((resolve) => { release=() => resolve({ operation_id:followupOperation }); }));
  document.querySelector<HTMLButtonElement>('[data-testid="deleted-copy-followup-approve"]')!.click();
  await vi.waitFor(() => expect(fakes.approve).toHaveBeenCalled());
  fakes.active = { ...binding, workspace_epoch:"new-epoch" };
  fakes.listeners.get("workspace-sync-status-changed")?.();
  await vi.waitFor(() => expect(document.querySelector('[data-testid="deleted-copy-followup-approve"]')).toBeNull());
  fakes.resumeContent.mockClear(); fakes.resumeAll.mockClear(); fakes.resumeSync.mockClear(); release();
  await new Promise((resolve) => setTimeout(resolve,0));
  expect(fakes.resumeContent).not.toHaveBeenCalled(); expect(fakes.resumeAll).not.toHaveBeenCalled(); expect(fakes.resumeSync).not.toHaveBeenCalled();
});

test("후속 처리가 필요한 동안에는 사본 생성 진행으로 승인 없는 취소를 우회하지 않는다", async () => {
  await restorePending(blockedCopy);
  expect(fakes.approve).not.toHaveBeenCalled(); expect(fakes.copyProgress).not.toHaveBeenCalled();
  const progress = document.querySelector<HTMLButtonElement>('[data-testid="deleted-copy-progress"]');
  if (progress) progress.click();
  await vi.waitFor(() => expect(document.body.textContent).toContain(followupOperation));
  expect(fakes.approve).not.toHaveBeenCalled(); expect(fakes.copyProgress).not.toHaveBeenCalled();
  expect(fakes.resumeAll).not.toHaveBeenCalled(); expect(fakes.resumeSync).not.toHaveBeenCalled();
});


test("같은 후속 ID의 generation이나 hash가 바뀌면 이전 확인으로 승인하지 않는다", async () => {
  const view = await restorePending(blockedCopy); await showApproval();
  const oldButton = document.querySelector<HTMLButtonElement>('[data-testid="deleted-copy-followup-approve"]')!;
  // A newer queued generation requires a durable checkpoint; automatic saves wait one second of input silence.
  view.dispatch({ changes:{ from:4, insert:" later" }, userEvent:"input.type" });
  await vi.waitFor(() => expect(fakes.save).toHaveBeenCalledWith(expect.objectContaining({ edit_session_id:"open-session", generation:"4" })), { timeout:3000 });
  const newerHash = "sha256:" + "c".repeat(64);
  fakes.copyStatus.mockResolvedValue({ ...blockedCopy, blocking_followup:{ ...blockedCopy.blocking_followup, followup_generation:"4", followup_hash:newerHash } });
  await refreshStatus();
  await vi.waitFor(() => expect(document.body.textContent).toContain(newerHash));
  oldButton.click(); await new Promise((resolve) => setTimeout(resolve,0));
  expect(fakes.approve).not.toHaveBeenCalled();
  expect(fakes.resumeContent).not.toHaveBeenCalled();
  await showApproval();
  fakes.approve.mockResolvedValue({ operation_id:followupOperation });
  document.querySelector<HTMLButtonElement>('[data-testid="deleted-copy-followup-approve"]')!.click();
  await vi.waitFor(() => expect(fakes.approve).toHaveBeenCalledExactlyOnceWith(binding, nodeId, copyNode, createOperation, blockerOperation, followupOperation, "2", "4", newerHash));
});


test("같은 후속 ID의 순번이 바뀌면 다시 확인해야 한다", async () => {
  await restorePending(blockedCopy); await showApproval();
  const oldButton = document.querySelector<HTMLButtonElement>('[data-testid="deleted-copy-followup-approve"]')!;
  fakes.copyStatus.mockResolvedValue({ ...blockedCopy, blocking_followup:{ ...blockedCopy.blocking_followup, followup_operation_seq:"3" } });
  await refreshStatus();
  await vi.waitFor(() => expect(document.body.textContent).toContain("순번 3"));
  oldButton.click(); await new Promise((resolve) => setTimeout(resolve,0));
  expect(fakes.approve).not.toHaveBeenCalled();
  expect(fakes.resumeContent).not.toHaveBeenCalled();
});


test("새 입력의 checkpoint가 끝나기 전에는 표시된 후속 작업을 승인하지 않는다", async () => {
  const view = await restorePending(blockedCopy); await showApproval();
  let saveStarted!: () => void;
  const started = new Promise<void>((resolve) => { saveStarted = resolve; });
  let finishSave!: () => void;
  fakes.save.mockImplementation((request: { edit_session_id:string; generation:string }) => new Promise((resolve) => {
    finishSave = () => resolve({ edit_session_id:request.edit_session_id, generation:request.generation });
    saveStarted();
  }));
  view.dispatch({ changes:{ from:4, insert:" later" }, userEvent:"input.type" });
  document.querySelector<HTMLButtonElement>('[data-testid="deleted-copy-followup-approve"]')!.click();
  await started;
  expect(fakes.approve).not.toHaveBeenCalled();
  expect(fakes.resumeContent).not.toHaveBeenCalled();
  expect(view.state.doc.toString()).toBe("base later");
  const newerHash = "sha256:" + "c".repeat(64);
  fakes.copyStatus.mockResolvedValue({ ...blockedCopy, blocking_followup:{ ...blockedCopy.blocking_followup, followup_generation:"4", followup_hash:newerHash } });
  finishSave();
  await vi.waitFor(() => expect(document.querySelector<HTMLButtonElement>('[data-testid="deleted-copy-status-refresh"]')?.disabled).toBe(false));
  await refreshStatus();
  await vi.waitFor(() => expect(document.body.textContent).toContain(newerHash));
  expect(fakes.approve).not.toHaveBeenCalled();
  expect(fakes.resumeContent).not.toHaveBeenCalled();
  expect(view.state.doc.toString()).toBe("base later");
});

test("IME 조합 중에는 후속 취소 승인을 호출하지 않고 조합 종료를 안내한다", async () => {
  const view = await restorePending(blockedCopy); await showApproval();
  const element = document.querySelector<HTMLElement>('.cm-content')!;
  element.dispatchEvent(new CompositionEvent("compositionstart", { bubbles:true }));
  view.dispatch({ changes:{ from:4, insert:"한" }, userEvent:"input.type" });
  document.querySelector<HTMLButtonElement>('[data-testid="deleted-copy-followup-approve"]')!.click();
  await vi.waitFor(() => expect(document.body.textContent).toMatch(/조합.*마친|조합.*끝/));
  expect(fakes.approve).not.toHaveBeenCalled();
  expect(fakes.resumeContent).not.toHaveBeenCalled();
  expect(view.state.doc.toString()).toBe("base한");
  element.dispatchEvent(new CompositionEvent("compositionend", { bubbles:true }));
  await new Promise((resolve) => setTimeout(resolve,300));
});

test("승인 결과를 받았어도 편집 잠금 해제 실패 시 전송을 재개하거나 성공으로 표시하지 않는다", async () => {
  const view = await restorePending(blockedCopy); await showApproval();
  fakes.failRelease = true;
  fakes.approve.mockResolvedValue({ operation_id:followupOperation });
  document.querySelector<HTMLButtonElement>('[data-testid="deleted-copy-followup-approve"]')!.click();
  await vi.waitFor(() => expect(fakes.approve).toHaveBeenCalled());
  await vi.waitFor(() => expect(fakes.releaseHeld).toHaveBeenCalled());
  await vi.waitFor(() => expect(document.body.textContent).toMatch(/결과.*확인|다시 확인|다시 열/));
  expect(fakes.resumeContent).not.toHaveBeenCalled();
  expect(document.body.textContent).not.toContain("사본 저장 완료");
  expect(view.state.doc.toString()).toBe("base");
  expect(document.querySelector('.cm-content[contenteditable=true]')).toBeNull();
});
