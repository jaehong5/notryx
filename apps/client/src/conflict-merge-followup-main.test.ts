// @vitest-environment jsdom
import { EditorState } from "@codemirror/state";
import { EditorView } from "@codemirror/view";
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import type { WorkspaceRegistration } from "../../../packages/protocol-types";

const fakes = vi.hoisted(() => ({
  listeners: new Map<string, () => void>(),
  root: undefined as { unmount(): void } | undefined,
  confirmMerge: vi.fn(), queueMerge: vi.fn(), mergeStatus: vi.fn(), followupStatus: vi.fn(), approveFollowup: vi.fn(), resolution: vi.fn(), reopen: vi.fn(), continueChoice: vi.fn(), completeChoice: vi.fn(),
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
    workspaceContentConflictMergeStatus:fakes.mergeStatus, workspaceContentConflictMergeFollowupStatus:fakes.followupStatus,
    requestWorkspaceContentConflictMergeFollowupAbandon:fakes.approveFollowup, workspaceContentConflictResolutionStatus:fakes.resolution,
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
  fakes.queue.mockReset().mockImplementation(async (document_id:string, _session:string, generation:string) => ({ operation_id:"019a1000-0000-7000-8000-000000000006", document_id, generation, committed:false }));
  fakes.confirmMerge.mockReset(); fakes.queueMerge.mockReset(); fakes.mergeStatus.mockReset(); fakes.followupStatus.mockReset(); fakes.approveFollowup.mockReset(); fakes.resolution.mockReset(); fakes.reopen.mockReset(); fakes.continueChoice.mockReset(); fakes.completeChoice.mockReset();
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
const resolution = (state: typeof pendingState) => ({ binding, document_id:nodeId, operation_id:operationId, state });
const mergeOperationId = "019a1000-0000-7000-8000-000000000005";
const mergeStatus = (queued:boolean) => ({ binding, document_id:nodeId, operation_id:operationId, intent_version:"0", merge_operation:queued ? { operation_id:mergeOperationId, operation_seq:"2" } : null });
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

const followupId = "019a1000-0000-7000-8000-000000000006";
const earlier = { operation_id:followupId, operation_seq:"2", generation:"2", payload_hash:"sha256:47ed148696318fc77976aac4a12c29c57ebf84edf2f25dce6ff241b52ec1c330" };
const followup = { ...earlier, generation:"2", payload_hash:snapshot.local_hash };
const followupResponse = (value:typeof followup | null) => ({ binding, document_id:nodeId, operation_id:operationId, intent_version:"0", followup:value });
const waitingFollowup = (generation = "2") => ({ operation_id:followupId, operation_seq:"2", document_id:nodeId, generation, status:"queued", frozen:false, abandon_state:"none", failure:null, payload_retired:false });

async function prepareFollowup(value:typeof followup = followup):Promise<EditorView> {
  fakes.confirmMerge.mockResolvedValue({ status:"pending", version:"0" });
  fakes.mergeStatus.mockResolvedValue(mergeStatus(false));
  fakes.followupStatus.mockResolvedValue(followupResponse(value));
  fakes.approveFollowup.mockResolvedValue({ status:"pending", version:"0", requested_operation_id:followupId });
  const view = await showMerge();
  await confirmMerge();
  await vi.waitFor(() => expect(document.querySelector<HTMLButtonElement>('[data-testid="conflict-merge-queue"]')?.disabled).toBe(false));
  fakes.resolution.mockResolvedValue(resolution(pendingState));
  fakes.outbox.mockResolvedValue(confirmedOutbox(waitingFollowup(value.generation)));
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-merge-queue"]')!.click();
  await vi.waitFor(() => expect(fakes.followupStatus).toHaveBeenCalledWith(binding, nodeId, operationId, "0"));
  await vi.waitFor(() => expect(document.querySelector('[data-testid="conflict-merge-followup-confirm"]')).not.toBeNull());
  return view;
}

test("가짜 IPC: 표시한 후속 작업을 별도 승인하기 전에는 취소하거나 합본을 queue하지 않는다", async () => {
  const view = await prepareFollowup();
  const text = document.body.textContent ?? "";
  expect(text).toContain(followup.operation_id);
  expect(text).toMatch(/순번\s*[:：]?\s*2/);
  expect(text).toMatch(/(?:generation|세대)\s*[:：]?\s*2/);
  expect(text).toContain(followup.payload_hash);
  expect(fakes.approveFollowup).not.toHaveBeenCalled();
  expect(fakes.queueMerge).not.toHaveBeenCalled();
  expect(fakes.continueChoice).not.toHaveBeenCalled();
  expect(view.state.doc.toString()).toBe(mergedText);
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-merge-followup-cancel"]')!.click();
  await vi.waitFor(() => expect(document.querySelector('[data-testid="conflict-merge-followup-confirm"]')).toBeNull());
  expect(fakes.approveFollowup).not.toHaveBeenCalled();
  expect(view.state.doc.toString()).toBe(mergedText);
});

test("가짜 IPC: 후속 승인 대기 중 편집기를 잠그고 확인한 작업만 전송 차단 해제한다", async () => {
  let finish!: (value:{ status:"pending"; version:string; requested_operation_id:string }) => void;
  const view = await prepareFollowup();
  fakes.approveFollowup.mockReturnValue(new Promise((resolve) => { finish = resolve; }));
  const { AutoTransmit } = await import("./auto-transmit");
  const resume = vi.spyOn(AutoTransmit.prototype, "resumeContentAfterAbandon").mockReturnValue(true);
  const generalResume = vi.spyOn(AutoTransmit.prototype, "resume");
  fakes.queueMerge.mockClear();
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-merge-followup-confirm"]')!.click();
  await vi.waitFor(() => expect(fakes.approveFollowup).toHaveBeenCalledExactlyOnceWith(binding, nodeId, "open-session", operationId, "0", followup));
  expect(view.state.facet(EditorState.readOnly)).toBe(true);
  expect(resume).not.toHaveBeenCalled();
  expect(fakes.queueMerge).not.toHaveBeenCalled();
  finish({ status:"pending", version:"0", requested_operation_id:followupId });
  await vi.waitFor(() => expect(resume).toHaveBeenCalledExactlyOnceWith(JSON.stringify(binding), followupId));
  expect(generalResume).not.toHaveBeenCalled();
  expect(fakes.queueMerge).not.toHaveBeenCalled();
  expect(document.body.textContent).not.toMatch(/합본.*저장 완료/);
  expect(view.state.doc.toString()).toBe(mergedText);
});

test("가짜 IPC: 후속 작업의 서버 ACK 전에는 명시 승인 뒤에도 합본을 queue하지 않는다", async () => {
  await prepareFollowup();
  const ackPending = { ...waitingFollowup(), status:"abandoned", abandon_state:"none" };
  fakes.outbox.mockResolvedValue(confirmedOutbox(ackPending));
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-merge-followup-confirm"]')!.click();
  await vi.waitFor(() => expect(fakes.approveFollowup).toHaveBeenCalledOnce());
  fakes.followupStatus.mockResolvedValue(followupResponse(null));
  await vi.waitFor(() => expect(document.body.textContent).toMatch(/ACK|영수증/));
  expect(fakes.queueMerge).not.toHaveBeenCalled();
  await vi.waitFor(() => expect(document.querySelector<HTMLButtonElement>('[data-testid="conflict-merge-queue"]')?.disabled).toBe(false));
  const previousReads = fakes.outbox.mock.calls.length;
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-merge-queue"]')!.click();
  await vi.waitFor(() => expect(fakes.outbox.mock.calls.length).toBeGreaterThan(previousReads));
  expect(fakes.queueMerge).not.toHaveBeenCalled();
  expect(document.body.textContent).not.toMatch(/합본.*저장 완료/);
});

test.each(["composition", "later-edit"] as const)("가짜 IPC: %s 상태에서는 이전 후속 승인을 실행하지 않는다", async (mode) => {
    const view = await prepareFollowup();
    const confirm = document.querySelector<HTMLButtonElement>('[data-testid="conflict-merge-followup-confirm"]')!;
    if (mode === "composition") view.contentDOM.dispatchEvent(new CompositionEvent("compositionstart", { bubbles:true }));
    else view.dispatch({ changes:{ from:view.state.doc.length, insert:"later" }, userEvent:"input.type" });
    confirm.click();
    await new Promise((resolve) => setTimeout(resolve, 0));
    expect(fakes.approveFollowup).not.toHaveBeenCalled();
    expect(fakes.queueMerge).not.toHaveBeenCalled();
    expect(view.state.doc.toString()).toBe(mode === "composition" ? mergedText : mergedText + "later");
    if (mode === "composition") view.contentDOM.dispatchEvent(new CompositionEvent("compositionend", { bubbles:true }));
});

test("가짜 IPC: 표시 뒤 입력한 checkpoint가 저장 중이면 이전 후속 작업을 승인하지 않는다", async () => {
  const view = await prepareFollowup();
  const confirm = document.querySelector<HTMLButtonElement>('[data-testid="conflict-merge-followup-confirm"]')!;
  let finish!: () => void;
  fakes.save.mockImplementation((request:{ edit_session_id:string; generation:string }) => new Promise((resolve) => {
    finish = () => resolve({ edit_session_id:request.edit_session_id, generation:request.generation });
  }));
  fakes.save.mockClear();
  view.dispatch({ changes:{ from:view.state.doc.length, insert:"later" }, userEvent:"input.type" });
  confirm.click();
  document.querySelector<HTMLButtonElement>("#save")!.click();
  await vi.waitFor(() => expect(fakes.save).toHaveBeenCalledWith(expect.objectContaining({ edit_session_id:"open-session", generation:"3", bytes_base64:btoa(mergedText + "later") })));
  expect(fakes.approveFollowup).not.toHaveBeenCalled();
  expect(fakes.queueMerge).not.toHaveBeenCalled();
  const pendingSave = vi.mocked(fakes.save).mock.results.at(-1)!.value as Promise<{ edit_session_id:string; generation:string }>;
  finish();
  await expect(pendingSave).resolves.toEqual({ edit_session_id:"open-session", generation:"3" });
  await vi.waitFor(() => expect(document.body.textContent).toContain("이 기기에 저장됨"));
  expect(fakes.approveFollowup).not.toHaveBeenCalled();
  expect(view.state.doc.toString()).toBe(mergedText + "later");
});

test("가짜 IPC: 같은 후속 ID의 generation 또는 hash가 바뀌면 이전 확인을 무효화한다", async () => {
  const laterText = "local first merged\n";
  const laterSnapshot = { ...snapshot, generation:"3", local_bytes:new TextEncoder().encode(laterText), local_hash:"sha256:05782f4809a7d71e3a3f260569ae42d335f891a6bd52ac02761593a1e9e71718" };
  const later = { ...earlier, generation:"3", payload_hash:laterSnapshot.local_hash };
  fakes.confirmMerge.mockResolvedValue({ status:"pending", version:"0" });
  fakes.mergeStatus.mockResolvedValue(mergeStatus(false));
  fakes.followupStatus.mockResolvedValue(followupResponse(earlier));
  fakes.prepareConflict.mockImplementation(async (_binding:WorkspaceRegistration, _document:string, _session:string, generation:string) => generation === "1" ? originalSnapshot : laterSnapshot);
  const view = await openEditor();
  await compareOriginal();
  view.dispatch({ changes:{ from:view.state.doc.length, insert:" first" }, userEvent:"input.type" });
  document.querySelector<HTMLButtonElement>("#save")!.click();
  await vi.waitFor(() => expect(fakes.save).toHaveBeenCalledWith(expect.objectContaining({ edit_session_id:"open-session", generation:"2", bytes_base64:btoa("local first") })));
  view.dispatch({ changes:{ from:view.state.doc.length, insert:" merged\n" }, userEvent:"input.type" });
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-review-open"]')!.click();
  await vi.waitFor(() => expect(fakes.prepareConflict).toHaveBeenCalledWith(binding, nodeId, "open-session", "3"));
  await vi.waitFor(() => expect(document.querySelector<HTMLTextAreaElement>('[aria-label="이 기기에 저장된 내 초안"]')?.value).toBe(laterText));
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-merge-review"]')!.click();
  await vi.waitFor(() => expect(document.querySelector('[data-testid="conflict-merge-confirm"]')).not.toBeNull());
  await confirmMerge();
  await vi.waitFor(() => expect(document.querySelector<HTMLButtonElement>('[data-testid="conflict-merge-queue"]')?.disabled).toBe(false));
  fakes.resolution.mockResolvedValue({ binding, document_id:nodeId, operation_id:operationId, state:{ ...pendingState, generation:"3" } });
  fakes.outbox.mockResolvedValue(confirmedOutbox(waitingFollowup("2")));
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-merge-queue"]')!.click();
  await vi.waitFor(() => expect(document.querySelector('[data-testid="conflict-merge-followup-confirm"]')).not.toBeNull());
  const oldConfirm = document.querySelector<HTMLButtonElement>('[data-testid="conflict-merge-followup-confirm"]')!;
  fakes.followupStatus.mockResolvedValue(followupResponse(later));
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-merge-followup-refresh"]')!.click();
  await vi.waitFor(() => expect(document.body.textContent).toContain(later.payload_hash));
  oldConfirm.click();
  await new Promise((resolve) => setTimeout(resolve, 0));
  expect(fakes.approveFollowup).not.toHaveBeenCalled();
  expect(fakes.queueMerge).not.toHaveBeenCalled();
  expect(view.state.doc.toString()).toBe(laterText);
});

test.each(["binding", "session"] as const)("가짜 IPC: %s 변경 뒤 늦은 후속 조회는 승인 화면을 열지 않는다", async (change) => {
    let finish!: (value:ReturnType<typeof followupResponse>) => void;
    fakes.followupStatus.mockReturnValue(new Promise((resolve) => { finish = resolve; }));
    fakes.confirmMerge.mockResolvedValue({ status:"pending", version:"0" });
    fakes.mergeStatus.mockResolvedValue(mergeStatus(false));
    const view = await showMerge();
    await confirmMerge();
    await vi.waitFor(() => expect(document.querySelector<HTMLButtonElement>('[data-testid="conflict-merge-queue"]')?.disabled).toBe(false));
    fakes.resolution.mockResolvedValue(resolution(pendingState));
    fakes.outbox.mockResolvedValue(confirmedOutbox(waitingFollowup()));
    document.querySelector<HTMLButtonElement>('[data-testid="conflict-merge-queue"]')!.click();
    await vi.waitFor(() => expect(fakes.followupStatus).toHaveBeenCalledWith(binding, nodeId, operationId, "0"));
    if (change === "binding") {
      fakes.active = { ...binding, workspace_epoch:"new-epoch" };
      fakes.listeners.get("workspace-sync-status-changed")?.();
      await vi.waitFor(() => expect(document.querySelector('[aria-label="합본 저장 상태"]')).toBeNull());
    } else {
      document.querySelector<HTMLButtonElement>('[data-testid="close-document"]')!.click();
      await vi.waitFor(() => expect(document.querySelector(".cm-editor")).toBeNull());
    }
    finish(followupResponse(followup));
    await new Promise((resolve) => setTimeout(resolve, 0));
    expect(document.querySelector('[data-testid="conflict-merge-followup-confirm"]')).toBeNull();
    expect(fakes.approveFollowup).not.toHaveBeenCalled();
    expect(fakes.queueMerge).not.toHaveBeenCalled();
    expect(view.state.doc.toString()).toBe(mergedText);
});
