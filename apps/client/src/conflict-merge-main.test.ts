// @vitest-environment jsdom
import { EditorState } from "@codemirror/state";
import { EditorView } from "@codemirror/view";
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import type { WorkspaceRegistration } from "../../../packages/protocol-types";

const fakes = vi.hoisted(() => ({
  listeners: new Map<string, () => void>(),
  root: undefined as { unmount(): void } | undefined,
  confirmMerge: vi.fn(), queueMerge: vi.fn(), mergeStatus: vi.fn(), mergeFollowupStatus: vi.fn(), abandonMergeFollowup: vi.fn(), resolution: vi.fn(), reopen: vi.fn(), continueChoice: vi.fn(), completeChoice: vi.fn(),
  prepareConflict: vi.fn(), prepare: vi.fn(), commit: vi.fn(), inspect: vi.fn(), save: vi.fn(), queue: vi.fn(), transmit: vi.fn(), pair: vi.fn(), recover: vi.fn(), outbox: vi.fn(), generation:"0", deleted: false, close:vi.fn(), active:undefined as WorkspaceRegistration | undefined,
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
    workspaceSyncState: async () => ({ binding:fakes.active, status:"ready" }),
    workspaceInventory: async () => ({ nodes:fakes.deleted ? [rootNode] : [rootNode, node], next_after_id:null }),
    openWorkspaceDocument: async () => ({ binding, document_id:nodeId, edit_session_id:"open-session", generation:fakes.generation, bytes_base64:btoa("local"), readonly:false, local_draft:false, has_session:true, cache_warning:null, open_notice:null }),
    workspaceDocumentOutbox:fakes.outbox,
    prepareWorkspaceRemoteRefresh:fakes.prepare, commitWorkspaceRemoteRefresh:fakes.commit,
    inspectWorkspaceRemoteChange:fakes.inspect, pairWorkspace:fakes.pair,
    saveWorkspaceDocument:fakes.save, queueWorkspaceDocument:fakes.queue,
    sendNextWorkspaceOperation:fakes.transmit,
    closeWorkspaceDocument:fakes.close, recoverWorkspaceSource:fakes.recover, prepareWorkspaceContentConflict:fakes.prepareConflict,
    confirmWorkspaceContentConflictMerge:fakes.confirmMerge, queueWorkspaceContentConflictMerge:fakes.queueMerge,
    workspaceContentConflictMergeStatus:fakes.mergeStatus, workspaceContentConflictMergeFollowupStatus:fakes.mergeFollowupStatus,
    requestWorkspaceContentConflictMergeFollowupAbandon:fakes.abandonMergeFollowup, workspaceContentConflictResolutionStatus:fakes.resolution,
    reopenWorkspaceContentConflictCheckpoint:fakes.reopen, continueWorkspaceRemoteContentConflict:fakes.continueChoice, completeWorkspacePendingRemoteContentConflict:fakes.completeChoice,
  };
});

const binding: WorkspaceRegistration = { server_origin:"https://example.invalid", workspace_id:"workspace", workspace_epoch:"epoch", device_id:"device", device_stream_id:"stream" };
const rootId = "019a1000-0000-7000-8000-000000000001", nodeId = "019a1000-0000-7000-8000-000000000002";
const rootNode = { node_id:rootId, parent_id:null, name:"", kind:"folder" as const, deleted:false, content_rev:null, content_etag:null, meta_rev:"meta", meta_etag:'"meta"', node_etag:'"node"', content_hash:null, size:null, children_generation:"0", last_change_seq:"1" };
const node = { ...rootNode, node_id:nodeId, parent_id:rootId, name:"synthetic.md", kind:"file" as const, content_rev:"content", content_etag:'"content"', content_hash:"sha256:" + "a".repeat(64), size:"5" };
const mergedText = "local merged\n";
const snapshot = { binding, node_id:nodeId, operation_id:"019a1000-0000-7000-8000-000000000003", operation_seq:"1", generation:"2", base_revision:"base-rev", base_bytes:new TextEncoder().encode("base"), base_hash:"sha256:cae662172fd450bb0cd710a769079c05bfc5d8e35efa6576edc7d0377afdd4a2", local_bytes:new TextEncoder().encode(mergedText), local_hash:"sha256:08611daf89ad74ea29f191b9244bb42d55b09211d700f7b451651948b9161f53", conflict_revision:"revision-at-412", remote_revision:"current-rev", remote_bytes:new TextEncoder().encode("remote"), remote_hash:"sha256:b71199ebd070b36beab7317920c2c2f1d777df8d05e5527d8458fda57cb17a7a" };
const originalSnapshot = { ...snapshot, generation:"1", local_bytes:new TextEncoder().encode("local"), local_hash:"sha256:25bf8e1a2393f1108d37029b3df5593236c755742ec93465bbafa9b290bddcf6" };
const rangeRects = Object.getOwnPropertyDescriptor(Range.prototype, "getClientRects");
const rangeBounds = Object.getOwnPropertyDescriptor(Range.prototype, "getBoundingClientRect");

beforeEach(() => {
  Object.defineProperty(Range.prototype, "getClientRects", { configurable:true, value:() => [] });
  Object.defineProperty(Range.prototype, "getBoundingClientRect", { configurable:true, value:() => ({ left:0, right:0, top:0, bottom:0, width:0, height:0 }) });
  vi.resetModules();
  fakes.deleted = false; fakes.active = binding; fakes.generation = "1";
  fakes.outbox.mockReset().mockResolvedValue({ binding, document_id:nodeId, first_content_conflict:{ operation_id:snapshot.operation_id, operation_seq:snapshot.operation_seq }, has_conflict:true, has_failure:false, latest:null, confirmed_through:"0" });
  fakes.close.mockReset().mockResolvedValue(undefined); fakes.recover.mockReset().mockResolvedValue(true);
  fakes.prepareConflict.mockReset().mockImplementation(async (_binding:WorkspaceRegistration, _document:string, _session:string, generation:string) => generation === "1" ? originalSnapshot : snapshot); fakes.prepare.mockReset().mockResolvedValue(null);
  fakes.commit.mockReset().mockResolvedValue(true); fakes.inspect.mockReset().mockResolvedValue("same");
  fakes.save.mockReset().mockImplementation(async (request: { edit_session_id:string; generation:string }) => ({ edit_session_id:request.edit_session_id, generation:request.generation }));
  fakes.queue.mockReset().mockImplementation(async (document_id:string, _session:string, generation:string) => ({ operation_id:"019a1000-0000-7000-8000-000000000004", document_id, generation, committed:false }));
  fakes.confirmMerge.mockReset(); fakes.queueMerge.mockReset(); fakes.mergeStatus.mockReset(); fakes.mergeFollowupStatus.mockReset().mockResolvedValue(noMergeFollowup); fakes.abandonMergeFollowup.mockReset(); fakes.resolution.mockReset(); fakes.reopen.mockReset(); fakes.continueChoice.mockReset(); fakes.completeChoice.mockReset();
  fakes.resolution.mockResolvedValue({ binding, document_id:nodeId, operation_id:snapshot.operation_id, state:{ status:"not_found" } });
  fakes.transmit.mockReset().mockResolvedValue({ status:"idle", binding }); fakes.pair.mockReset().mockResolvedValue(undefined);
});
afterEach(async () => {
  fakes.root?.unmount(); fakes.root = undefined;
  await new Promise((resolve) => setTimeout(resolve, 0));
  fakes.listeners.clear(); document.body.replaceChildren(); vi.restoreAllMocks();
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
  await vi.waitFor(() => expect(document.querySelector('[data-testid="conflict-review-open"]')).not.toBeNull());
  return EditorView.findFromDOM(document.querySelector(".cm-editor")!)!;
}

const operationId = snapshot.operation_id;
const pendingState = { status:"pending" as const, version:"0", generation:"2", choice:"save_merged" as const, needs_review:false, frozen:false };
const completedState = { status:"completed" as const, generation:"3", frozen:false };
const resolution = (state: typeof pendingState | typeof completedState) => ({ binding, document_id:nodeId, operation_id:operationId, state });
const reopened = (state: typeof pendingState | typeof completedState, text:string) => ({
  state, document:{ binding, document_id:nodeId, edit_session_id:"reopened-session", generation:state.generation, bytes_base64:btoa(text), readonly:false, local_draft:true, has_session:true, cache_warning:null, open_notice:null },
});
const mergeOperationId = "019a1000-0000-7000-8000-000000000005";
const mergeStatus = (queued:boolean) => ({ binding, document_id:nodeId, operation_id:operationId, intent_version:"0", merge_operation:queued ? { operation_id:mergeOperationId, operation_seq:"2" } : null });
const noMergeFollowup = { binding, document_id:nodeId, operation_id:operationId, intent_version:"0", followup:null };
const reopenedQueued = () => ({ ...reopened(pendingState, mergedText), document:{ ...reopened(pendingState, mergedText).document, generation:"3" } });
const confirmedOutbox = (latest:unknown = null) => ({ binding, document_id:nodeId, first_content_conflict:{ operation_id:snapshot.operation_id, operation_seq:snapshot.operation_seq }, has_conflict:true, has_failure:false, latest, confirmed_through:"1" });

async function compareOriginal() {
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-review-open"]')!.click();
  await vi.waitFor(() => expect(document.querySelector<HTMLTextAreaElement>('[aria-label="이 기기에 저장된 내 초안"]')?.value).toBe("local"));
  const section = document.querySelector<HTMLElement>('[aria-label="충돌 원문 비교"]')!;
  const close = [...section.querySelectorAll("button")].find((button) => button.textContent?.trim() === "닫기");
  expect(close).toBeDefined();
  close!.click();
  await vi.waitFor(() => expect(document.querySelector('[aria-label="충돌 원문 비교"]')).toBeNull());
  fakes.prepareConflict.mockClear();
}

async function showMerge(): Promise<EditorView> {
  const view = await openEditor();
  await compareOriginal();
  view.dispatch({ changes:{ from:view.state.doc.length, insert:" merged\n" }, userEvent:"input.type" });
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-review-open"]')!.click();
  await vi.waitFor(() => expect(fakes.save).toHaveBeenCalledWith(expect.objectContaining({ document_id:nodeId, edit_session_id:"open-session", generation:"2", bytes_base64:btoa(mergedText) })));
  await vi.waitFor(() => expect(fakes.prepareConflict).toHaveBeenCalledWith(binding, nodeId, "open-session", "2"));
  await vi.waitFor(() => expect(document.querySelector<HTMLTextAreaElement>('[aria-label="이 기기에 저장된 내 초안"]')?.value).toBe(mergedText));
  await vi.waitFor(() => expect(document.querySelector('[data-testid="conflict-merge-review"]')).not.toBeNull());
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-merge-review"]')!.click();
  await vi.waitFor(() => expect(document.querySelector('[data-testid="conflict-merge-confirm"]')).not.toBeNull());
  return view;
}

async function confirmMerge() {
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-merge-confirm"]')!.click();
}

test("본문 편집기의 checkpoint를 다시 비교하고 별도 확인해야 합본 선택을 저장한다", async () => {
  fakes.confirmMerge.mockResolvedValue({ status:"pending", version:"0" });
  const view = await showMerge();
  expect(view.state.doc.toString()).toBe(mergedText);
  expect(snapshot.local_bytes).toEqual(new TextEncoder().encode(mergedText));
  expect(fakes.confirmMerge).not.toHaveBeenCalled();
  await confirmMerge();
  await vi.waitFor(() => expect(fakes.confirmMerge).toHaveBeenCalledExactlyOnceWith(snapshot, "open-session", null));
  await vi.waitFor(() => expect(document.body.textContent).toMatch(/보류|대기/));
  expect(document.body.textContent).not.toMatch(/합본.*저장 완료/);
  expect(fakes.reopen).not.toHaveBeenCalled();
  expect(view.state.doc.toString()).toBe(mergedText);
});

test("확인 중 편집기를 잠그고 이후 새 입력은 원래 초안에 별도로 보존한다", async () => {
  let finish!: (value: { status:"pending"; version:string }) => void;
  fakes.confirmMerge.mockReturnValue(new Promise((resolve) => { finish = resolve; }));
  const view = await showMerge();
  fakes.queue.mockClear();
  await confirmMerge();
  await vi.waitFor(() => expect(fakes.confirmMerge).toHaveBeenCalledOnce());
  expect(view.state.facet(EditorState.readOnly)).toBe(true);
  expect(view.state.doc.toString()).toBe(mergedText);
  expect(fakes.queue).not.toHaveBeenCalled();
  finish({ status:"pending", version:"0" });
  await vi.waitFor(() => expect(view.state.facet(EditorState.readOnly)).toBe(false));
  view.dispatch({ changes:{ from:view.state.doc.length, insert:"later" }, userEvent:"input.type" });
  document.querySelector<HTMLButtonElement>("#save")!.click();
  await vi.waitFor(() => expect(fakes.save).toHaveBeenCalledWith(expect.objectContaining({ document_id:nodeId, edit_session_id:"open-session", generation:"3", bytes_base64:btoa(mergedText + "later") })));
  expect(view.state.doc.toString()).toBe(mergedText + "later");
  expect(fakes.confirmMerge).toHaveBeenCalledExactlyOnceWith(snapshot, "open-session", null);
});

test("비교 전 최신 checkpoint가 끝나야 그 generation의 기기 초안을 확인한다", async () => {
  let finish!: () => void;
  fakes.save.mockImplementation((request: { edit_session_id:string; generation:string }) => new Promise((resolve) => {
    finish = () => resolve({ edit_session_id:request.edit_session_id, generation:request.generation });
  }));
  fakes.confirmMerge.mockResolvedValue({ status:"pending", version:"0" });
  const view = await openEditor();
  await compareOriginal();
  view.dispatch({ changes:{ from:view.state.doc.length, insert:" merged\n" }, userEvent:"input.type" });
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-review-open"]')!.click();
  await vi.waitFor(() => expect(fakes.save).toHaveBeenCalledWith(expect.objectContaining({ edit_session_id:"open-session", generation:"2", bytes_base64:btoa(mergedText) })));
  expect(fakes.prepareConflict).not.toHaveBeenCalled();
  expect(fakes.confirmMerge).not.toHaveBeenCalled();
  finish();
  await vi.waitFor(() => expect(fakes.prepareConflict).toHaveBeenCalledWith(binding, nodeId, "open-session", "2"));
  await vi.waitFor(() => expect(document.querySelector<HTMLTextAreaElement>('[aria-label="이 기기에 저장된 내 초안"]')?.value).toBe(mergedText));
  await vi.waitFor(() => expect(document.querySelector('[data-testid="conflict-merge-review"]')).not.toBeNull());
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-merge-review"]')!.click();
  await vi.waitFor(() => expect(document.querySelector('[data-testid="conflict-merge-confirm"]')).not.toBeNull());
  expect(fakes.confirmMerge).not.toHaveBeenCalled();
  await confirmMerge();
  await vi.waitFor(() => expect(fakes.confirmMerge).toHaveBeenCalledExactlyOnceWith(snapshot, "open-session", null));
  expect(view.state.doc.toString()).toBe(mergedText);
});

test("편집기에서 한글 조합 중에는 합본 확인을 호출하지 않는다", async () => {
  const view = await showMerge();
  view.contentDOM.dispatchEvent(new CompositionEvent("compositionstart", { bubbles:true }));
  await confirmMerge();
  await vi.waitFor(() => expect(document.body.textContent).toMatch(/조합/));
  expect(fakes.confirmMerge).not.toHaveBeenCalled();
  expect(view.state.doc.toString()).toBe(mergedText);
  view.contentDOM.dispatchEvent(new CompositionEvent("compositionend", { bubbles:true }));
});

test("초안 확인 뒤 편집기에 새 입력이 있으면 이전 비교로 승인하지 않는다", async () => {
  const view = await showMerge();
  const button = document.querySelector<HTMLButtonElement>('[data-testid="conflict-merge-confirm"]')!;
  view.dispatch({ changes:{ from:view.state.doc.length, insert:"newer" }, userEvent:"input.type" });
  button.click();
  await new Promise((resolve) => setTimeout(resolve, 0));
  expect(fakes.confirmMerge).not.toHaveBeenCalled();
  expect(view.state.doc.toString()).toBe(mergedText + "newer");
});

test("선택 응답 유실은 상태 확인과 재열기로 복구하고 이후 초안을 보존한다", async () => {
  fakes.confirmMerge.mockRejectedValue(new Error("response lost"));
  fakes.reopen.mockResolvedValue(reopened(pendingState, mergedText));
  const oldView = await showMerge();
  fakes.queue.mockClear();
  fakes.resolution.mockClear().mockResolvedValue(resolution(pendingState));
  await confirmMerge();
  await vi.waitFor(() => expect(document.querySelector('[data-testid="conflict-recover-result"]')).not.toBeNull());
  expect(oldView.state.facet(EditorState.readOnly)).toBe(true);
  expect(fakes.resolution).not.toHaveBeenCalled();
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-recover-result"]')!.click();
  await vi.waitFor(() => expect(fakes.reopen).toHaveBeenCalledExactlyOnceWith(binding, nodeId, operationId, "open-session"));
  await vi.waitFor(() => expect(EditorView.findFromDOM(document.querySelector(".cm-editor")!)?.state.doc.toString()).toBe(mergedText));
  expect(fakes.queue).not.toHaveBeenCalled();
  const current = EditorView.findFromDOM(document.querySelector(".cm-editor")!)!;
  current.dispatch({ changes:{ from:current.state.doc.length, insert:"later" }, userEvent:"input.type" });
  document.querySelector<HTMLButtonElement>("#save")!.click();
  await vi.waitFor(() => expect(fakes.save).toHaveBeenCalledWith(expect.objectContaining({ document_id:nodeId, edit_session_id:"reopened-session", generation:"3", bytes_base64:btoa(mergedText + "later") })));
  expect(current.state.doc.toString()).toBe(mergedText + "later");
});

test("합본 저장은 서버 ACK 완료 상태 이후에만 완료로 표시한다", async () => {
  fakes.confirmMerge.mockResolvedValue({ status:"pending", version:"0" });
  await showMerge();
  await confirmMerge();
  await vi.waitFor(() => expect(fakes.confirmMerge).toHaveBeenCalledOnce());
  fakes.resolution.mockClear().mockResolvedValue(resolution(pendingState));
  await vi.waitFor(() => expect(document.querySelector('[data-testid="conflict-merge-status-refresh"]')).not.toBeNull());
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-merge-status-refresh"]')!.click();
  await vi.waitFor(() => expect(fakes.resolution).toHaveBeenCalledWith(binding, nodeId, operationId));
  expect(document.body.textContent).not.toMatch(/합본.*저장 완료/);
  await vi.waitFor(() => expect(document.querySelector<HTMLButtonElement>('[data-testid="conflict-merge-status-refresh"]')?.disabled).toBe(false));
  fakes.resolution.mockClear().mockResolvedValue(resolution(completedState));
  fakes.mergeStatus.mockResolvedValue(mergeStatus(true));
  fakes.reopen.mockResolvedValue(reopened(completedState, mergedText));
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-merge-status-refresh"]')!.click();
  await vi.waitFor(() => expect(fakes.resolution).toHaveBeenCalledWith(binding, nodeId, operationId));
  await vi.waitFor(() => expect(fakes.mergeStatus).toHaveBeenCalledWith(binding, nodeId, operationId, "0"));
  await vi.waitFor(() => expect(document.body.textContent).toMatch(/합본.*저장 완료/));
});

test("합본 상태가 비어 있을 때만 queue하고 성공하면 세션을 퇴역·재열기해 ACK를 기다린다", async () => {
  let finish!: (value:{ status:"pending"; version:string; requested_operation_id:string }) => void;
  fakes.confirmMerge.mockResolvedValue({ status:"pending", version:"0" });
  fakes.queueMerge.mockReturnValue(new Promise((resolve) => { finish = resolve; }));
  fakes.mergeStatus.mockResolvedValueOnce(mergeStatus(false)).mockResolvedValue(mergeStatus(true));
  fakes.reopen.mockResolvedValue(reopenedQueued());
  const oldView = await showMerge();
  await confirmMerge();
  await vi.waitFor(() => expect(document.querySelector('[data-testid="conflict-merge-queue"]')).not.toBeNull());
  expect(fakes.queueMerge).not.toHaveBeenCalled();
  fakes.resolution.mockResolvedValue(resolution(pendingState));
  fakes.outbox.mockResolvedValue(confirmedOutbox());
  await vi.waitFor(() => expect(document.querySelector<HTMLButtonElement>('[data-testid="conflict-merge-queue"]')?.disabled).toBe(false));
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-merge-queue"]')!.click();
  await vi.waitFor(() => expect(fakes.queueMerge).toHaveBeenCalledExactlyOnceWith(binding, nodeId, "open-session", operationId, "0"));
  expect(fakes.mergeStatus).toHaveBeenCalledWith(binding, nodeId, operationId, "0");
  expect(fakes.mergeFollowupStatus).toHaveBeenCalledWith(binding, nodeId, operationId, "0");
  expect(fakes.mergeStatus.mock.invocationCallOrder[0]).toBeLessThan(fakes.queueMerge.mock.invocationCallOrder[0]);
  expect(fakes.mergeFollowupStatus.mock.invocationCallOrder[0]).toBeLessThan(fakes.queueMerge.mock.invocationCallOrder[0]);
  expect(oldView.state.facet(EditorState.readOnly)).toBe(true);
  expect(document.body.textContent).not.toMatch(/합본.*저장 완료/);
  finish({ status:"pending", version:"0", requested_operation_id:mergeOperationId });
  await vi.waitFor(() => expect(fakes.reopen).toHaveBeenCalledExactlyOnceWith(binding, nodeId, operationId, "open-session"));
  await vi.waitFor(() => expect(fakes.mergeStatus.mock.calls.length).toBeGreaterThanOrEqual(2));
  await vi.waitFor(() => expect(EditorView.findFromDOM(document.querySelector(".cm-editor")!)?.state.doc.toString()).toBe(mergedText));
  expect(document.body.textContent).toContain(mergeOperationId);
  expect(document.body.textContent).not.toMatch(/합본.*저장 완료/);
  expect(fakes.queueMerge).toHaveBeenCalledOnce();
  const current = EditorView.findFromDOM(document.querySelector(".cm-editor")!)!;
  expect(current).not.toBe(oldView);
  current.dispatch({ changes:{ from:current.state.doc.length, insert:"later" }, userEvent:"input.type" });
  document.querySelector<HTMLButtonElement>("#save")!.click();
  await vi.waitFor(() => expect(fakes.save).toHaveBeenCalledWith(expect.objectContaining({ edit_session_id:"reopened-session", generation:"4", bytes_base64:btoa(mergedText + "later") })));
  expect(current.state.doc.toString()).toBe(mergedText + "later");
});

test("queue 응답 유실 후 재열고 상태를 조회해 기존 합본 ID를 재사용한다", async () => {
  fakes.confirmMerge.mockResolvedValue({ status:"pending", version:"0" });
  fakes.queueMerge.mockRejectedValue(new Error("response lost after queue"));
  fakes.mergeStatus.mockResolvedValueOnce(mergeStatus(false)).mockResolvedValue(mergeStatus(true));
  fakes.reopen.mockResolvedValue(reopenedQueued());
  const oldView = await showMerge();
  await confirmMerge();
  await vi.waitFor(() => expect(document.querySelector('[data-testid="conflict-merge-queue"]')).not.toBeNull());
  fakes.resolution.mockResolvedValue(resolution(pendingState));
  fakes.outbox.mockResolvedValue(confirmedOutbox());
  await vi.waitFor(() => expect(document.querySelector<HTMLButtonElement>('[data-testid="conflict-merge-queue"]')?.disabled).toBe(false));
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-merge-queue"]')!.click();
  await vi.waitFor(() => expect(fakes.queueMerge).toHaveBeenCalledOnce());
  await vi.waitFor(() => expect(document.querySelector('[data-testid="conflict-recover-result"]')).not.toBeNull());
  expect(oldView.state.facet(EditorState.readOnly)).toBe(true);
  fakes.resolution.mockResolvedValue(resolution(pendingState));
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-recover-result"]')!.click();
  await vi.waitFor(() => expect(fakes.reopen).toHaveBeenCalledExactlyOnceWith(binding, nodeId, operationId, "open-session"));
  await vi.waitFor(() => expect(EditorView.findFromDOM(document.querySelector(".cm-editor")!)?.state.doc.toString()).toBe(mergedText));
  await vi.waitFor(() => expect(document.querySelector('[data-testid="conflict-merge-queue"]')).not.toBeNull());
  await vi.waitFor(() => expect(document.querySelector<HTMLButtonElement>('[data-testid="conflict-merge-queue"]')?.disabled).toBe(false));
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-merge-queue"]')!.click();
  await vi.waitFor(() => expect(fakes.mergeStatus.mock.calls.length).toBeGreaterThanOrEqual(2));
  await vi.waitFor(() => expect(document.body.textContent).toContain(mergeOperationId));
  expect(fakes.queueMerge).toHaveBeenCalledOnce();
  expect(document.body.textContent).not.toMatch(/합본.*저장 완료/);
  const current = EditorView.findFromDOM(document.querySelector(".cm-editor")!)!;
  current.dispatch({ changes:{ from:current.state.doc.length, insert:"later" }, userEvent:"input.type" });
  document.querySelector<HTMLButtonElement>("#save")!.click();
  await vi.waitFor(() => expect(fakes.save).toHaveBeenCalledWith(expect.objectContaining({ edit_session_id:"reopened-session", generation:"4", bytes_base64:btoa(mergedText + "later") })));
});

test("승인하지 않은 후속 작업이 있으면 합본 queue나 abandon을 호출하지 않고 초안을 보존한다", async () => {
  fakes.confirmMerge.mockResolvedValue({ status:"pending", version:"0" });
  fakes.mergeStatus.mockResolvedValue(mergeStatus(false));
  const view = await showMerge();
  await confirmMerge();
  await vi.waitFor(() => expect(document.querySelector('[data-testid="conflict-merge-queue"]')).not.toBeNull());
  fakes.resolution.mockResolvedValue(resolution(pendingState));
  const followup = { operation_id:"019a1000-0000-7000-8000-000000000006", operation_seq:"2", document_id:nodeId, generation:"2", status:"queued", frozen:false, abandon_state:"none", failure:null, payload_retired:false };
  fakes.outbox.mockResolvedValue(confirmedOutbox(followup));
  fakes.mergeFollowupStatus.mockResolvedValue({ ...noMergeFollowup, followup:{ operation_id:followup.operation_id, operation_seq:followup.operation_seq, generation:followup.generation, payload_hash:snapshot.local_hash } });
  await vi.waitFor(() => expect(document.querySelector<HTMLButtonElement>('[data-testid="conflict-merge-queue"]')?.disabled).toBe(false));
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-merge-queue"]')!.click();
  await vi.waitFor(() => expect(fakes.mergeStatus).toHaveBeenCalledWith(binding, nodeId, operationId, "0"));
  await vi.waitFor(() => expect(document.body.textContent).toMatch(/후속 작업|선행 작업/));
  expect(fakes.queueMerge).not.toHaveBeenCalled();
  expect(fakes.abandonMergeFollowup).not.toHaveBeenCalled();
  expect(fakes.continueChoice).not.toHaveBeenCalled();
  expect(fakes.completeChoice).not.toHaveBeenCalled();
  expect(fakes.reopen).not.toHaveBeenCalled();
  expect(view.state.doc.toString()).toBe(mergedText);
  view.dispatch({ changes:{ from:view.state.doc.length, insert:"later" }, userEvent:"input.type" });
  document.querySelector<HTMLButtonElement>("#save")!.click();
  await vi.waitFor(() => expect(fakes.save).toHaveBeenCalledWith(expect.objectContaining({ edit_session_id:"open-session", generation:"3", bytes_base64:btoa(mergedText + "later") })));
});

test("queue 성공 뒤 편집 세션 퇴역 재시도가 실패하면 복구 버튼으로 같은 합본을 재열 수 있다", async () => {
  fakes.confirmMerge.mockResolvedValue({ status:"pending", version:"0" });
  fakes.queueMerge.mockResolvedValue({ status:"pending", version:"0", requested_operation_id:mergeOperationId });
  fakes.mergeStatus.mockResolvedValueOnce(mergeStatus(false)).mockResolvedValue(mergeStatus(true));
  fakes.reopen.mockResolvedValue(reopenedQueued());
  const oldView = await showMerge();
  await confirmMerge();
  await vi.waitFor(() => expect(document.querySelector('[data-testid="conflict-merge-queue"]')).not.toBeNull());
  fakes.resolution.mockResolvedValue(resolution(pendingState));
  fakes.outbox.mockResolvedValue(confirmedOutbox());
  const { EditorController } = await import("./editor-controller");
  // Model retirement failure at the controller boundary before native reopen starts.
  const reopen = vi.spyOn(EditorController.prototype, "reopenConflictCheckpoint")
    .mockRejectedValueOnce(new Error("retire retry failed before native reopen"));
  await vi.waitFor(() => expect(document.querySelector<HTMLButtonElement>('[data-testid="conflict-merge-queue"]')?.disabled).toBe(false));
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-merge-queue"]')!.click();
  await vi.waitFor(() => expect(fakes.queueMerge).toHaveBeenCalledExactlyOnceWith(binding, nodeId, "open-session", operationId, "0"));
  await vi.waitFor(() => expect(reopen).toHaveBeenCalledOnce());
  expect(fakes.reopen).not.toHaveBeenCalled();
  expect(oldView.state.facet(EditorState.readOnly)).toBe(true);
  await vi.waitFor(() => expect(document.querySelector('[data-testid="conflict-recover-result"]')).not.toBeNull());
  await vi.waitFor(() => expect(document.querySelector<HTMLButtonElement>('[data-testid="conflict-recover-result"]')?.disabled).toBe(false));
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-recover-result"]')!.click();
  await vi.waitFor(() => expect(fakes.resolution).toHaveBeenCalledWith(binding, nodeId, operationId));
  await vi.waitFor(() => expect(fakes.reopen).toHaveBeenCalledExactlyOnceWith(binding, nodeId, operationId, "open-session"));
  await vi.waitFor(() => expect(EditorView.findFromDOM(document.querySelector(".cm-editor")!)?.state.doc.toString()).toBe(mergedText));
  await vi.waitFor(() => expect(document.querySelector<HTMLButtonElement>('[data-testid="conflict-merge-queue"]')?.disabled).toBe(false));
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-merge-queue"]')!.click();
  await vi.waitFor(() => expect(fakes.mergeStatus.mock.calls.length).toBeGreaterThanOrEqual(2));
  await vi.waitFor(() => expect(document.body.textContent).toContain(mergeOperationId));
  expect(document.body.textContent).toMatch(/순번\s*2/);
  expect(fakes.queueMerge).toHaveBeenCalledOnce();
  expect(document.body.textContent).not.toMatch(/합본.*저장 완료/);
  const current = EditorView.findFromDOM(document.querySelector(".cm-editor")!)!;
  current.dispatch({ changes:{ from:current.state.doc.length, insert:"later" }, userEvent:"input.type" });
  document.querySelector<HTMLButtonElement>("#save")!.click();
  await vi.waitFor(() => expect(fakes.save).toHaveBeenCalledWith(expect.objectContaining({ edit_session_id:"reopened-session", generation:"4", bytes_base64:btoa(mergedText + "later") })));
  expect(current.state.doc.toString()).toBe(mergedText + "later");
});
