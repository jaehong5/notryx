// @vitest-environment jsdom
import { EditorView } from "@codemirror/view";
import { act } from "react";
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import type { WorkspaceRegistration } from "../../../packages/protocol-types";

// These tests model a reopened app with mocked IPC; they do not restart a native process.
const fakes = vi.hoisted(() => ({
  root:undefined as { unmount():void } | undefined,
  listeners:new Map<string, () => void>(),
  active:undefined as WorkspaceRegistration | undefined,
  syncStatus:"ready", documentGeneration:"4", documentText:"approved merge\nlater draft\n",
  outbox:vi.fn(), resolution:vi.fn(), mergeStatus:vi.fn(), queueMerge:vi.fn(), continueChoice:vi.fn(),
  confirmMerge:vi.fn(), prepareConflict:vi.fn(), chooseRemote:vi.fn(), queue:vi.fn(), save:vi.fn(), transmit:vi.fn(), close:vi.fn(),
}));
vi.mock("react-dom/client", async (original) => {
  const actual = await original<typeof import("react-dom/client")>();
  return { ...actual, createRoot:(...args:Parameters<typeof actual.createRoot>) => { const root = actual.createRoot(...args); fakes.root = root; return root; } };
});
vi.mock("@tauri-apps/api/event", () => ({ listen:async (name:string, callback:() => void) => {
  fakes.listeners.set(name, callback);
  return () => { fakes.listeners.delete(name); };
} }));
vi.mock("@tauri-apps/api/core", () => ({ invoke:vi.fn() }));
vi.mock("./migration-recovery", () => ({ StartupGate:({ ready }:{ ready:() => unknown }) => ready() }));
vi.mock("./ipc", async (original) => {
  const actual = await original<typeof import("./ipc")>();
  return { ...actual,
    localDrafts:async () => [], pendingPairStatus:async () => "none",
    workspaceSyncState:async () => ({ binding:fakes.active, status:fakes.syncStatus }),
    workspaceInventory:async () => ({ nodes:[rootNode, node, otherNode], next_after_id:null }),
    openWorkspaceDocument:async (item:{ node_id:string }) => ({
      binding, document_id:item.node_id, edit_session_id:item.node_id === nodeId ? "merge-session" : "other-session",
      generation:item.node_id === nodeId ? fakes.documentGeneration : "0",
      bytes_base64:btoa(item.node_id === nodeId ? fakes.documentText : "other document\n"),
      readonly:false, local_draft:item.node_id === nodeId, has_session:true, cache_warning:null, open_notice:null,
    }),
    workspaceDocumentOutbox:fakes.outbox, workspaceContentConflictResolutionStatus:fakes.resolution,
    workspaceContentConflictMergeStatus:fakes.mergeStatus, queueWorkspaceContentConflictMerge:fakes.queueMerge,
    continueWorkspaceRemoteContentConflict:fakes.continueChoice, confirmWorkspaceContentConflictMerge:fakes.confirmMerge,
    prepareWorkspaceContentConflict:fakes.prepareConflict, chooseWorkspaceRemoteContentConflict:fakes.chooseRemote,
    prepareWorkspaceRemoteRefresh:async () => null, inspectWorkspaceRemoteChange:async () => "same",
    saveWorkspaceDocument:fakes.save, queueWorkspaceDocument:fakes.queue, sendNextWorkspaceOperation:fakes.transmit,
    closeWorkspaceDocument:fakes.close,
  };
});

const binding:WorkspaceRegistration = { server_origin:"https://example.invalid", workspace_id:"workspace", workspace_epoch:"epoch", device_id:"device", device_stream_id:"stream" };
const rootId = "019a1000-0000-7000-8000-000000000001", nodeId = "019a1000-0000-7000-8000-000000000002", otherId = "019a1000-0000-7000-8000-000000000003";
const conflictId = "019a1000-0000-7000-8000-000000000004", mergeId = "019a1000-0000-7000-8000-000000000005", laterId = "019a1000-0000-7000-8000-000000000006";
const rootNode = { node_id:rootId, parent_id:null, name:"", kind:"folder" as const, deleted:false, content_rev:null, content_etag:null, meta_rev:"meta", meta_etag:'"meta"', node_etag:'"node"', content_hash:null, size:null, children_generation:"0", last_change_seq:"1" };
const node = { ...rootNode, node_id:nodeId, parent_id:rootId, name:"synthetic.md", kind:"file" as const, content_rev:"content", content_etag:'"content"', content_hash:"sha256:" + "a".repeat(64), size:"14" };
const otherNode = { ...node, node_id:otherId, name:"other.md" };
const latest = { operation_id:laterId, operation_seq:"3", document_id:nodeId, generation:"4", status:"queued", frozen:false, abandon_state:"none", failure:null, payload_retired:false };
const outbox = { binding, document_id:nodeId, latest, first_content_conflict:{ operation_id:conflictId, operation_seq:"1" }, has_conflict:true, has_failure:false, confirmed_through:"1" };
const pending = { status:"pending" as const, version:"0", generation:"2", choice:"save_merged" as const, needs_review:false, frozen:false };
const resolution = { binding, document_id:nodeId, operation_id:conflictId, state:pending };
const mergeStatus = { binding, document_id:nodeId, operation_id:conflictId, intent_version:"0", merge_operation:{ operation_id:mergeId, operation_seq:"2" } };
const rangeRects = Object.getOwnPropertyDescriptor(Range.prototype, "getClientRects");
const rangeBounds = Object.getOwnPropertyDescriptor(Range.prototype, "getBoundingClientRect");

function deferred<T>() {
  let resolve!:(value:T) => void;
  const promise = new Promise<T>((done) => { resolve = done; });
  return { promise, resolve };
}

beforeEach(() => {
  Object.defineProperty(Range.prototype, "getClientRects", { configurable:true, value:() => [] });
  Object.defineProperty(Range.prototype, "getBoundingClientRect", { configurable:true, value:() => ({ left:0, right:0, top:0, bottom:0, width:0, height:0 }) });
  vi.resetModules();
  fakes.active = binding; fakes.syncStatus = "ready"; fakes.documentGeneration = "4"; fakes.documentText = "approved merge\nlater draft\n";
  fakes.outbox.mockReset().mockImplementation(async (current:WorkspaceRegistration, document:string) => document === nodeId ? outbox : ({ binding:current, document_id:document, latest:null, first_content_conflict:null, has_conflict:false, has_failure:false, confirmed_through:"0" }));
  fakes.resolution.mockReset().mockResolvedValue(resolution); fakes.mergeStatus.mockReset().mockResolvedValue(mergeStatus);
  fakes.queueMerge.mockReset(); fakes.continueChoice.mockReset(); fakes.confirmMerge.mockReset();
  fakes.prepareConflict.mockReset(); fakes.chooseRemote.mockReset();
  fakes.queue.mockReset().mockImplementation(async (document_id:string, _session:string, generation:string) => ({ operation_id:laterId, document_id, generation, committed:false }));
  fakes.save.mockReset().mockImplementation(async (request:{ edit_session_id:string; generation:string }) => ({ edit_session_id:request.edit_session_id, generation:request.generation }));
  fakes.transmit.mockReset().mockResolvedValue({ binding, status:"idle", operation_id:null, document_id:null, generation:null, content_block:null });
  fakes.close.mockReset().mockResolvedValue(undefined);
});

afterEach(async () => {
  fakes.root?.unmount(); fakes.root = undefined;
  await new Promise((resolve) => setTimeout(resolve, 0));
  fakes.listeners.clear(); document.body.replaceChildren();
  if (rangeRects) Object.defineProperty(Range.prototype, "getClientRects", rangeRects); else delete (Range.prototype as Partial<Range>).getClientRects;
  if (rangeBounds) Object.defineProperty(Range.prototype, "getBoundingClientRect", rangeBounds); else delete (Range.prototype as Partial<Range>).getBoundingClientRect;
});

async function openEditor(id = nodeId) {
  document.body.innerHTML = '<div id="root"></div>';
  await import("./main");
  await vi.waitFor(() => expect(document.querySelector(`[data-document="${id}"]`)).not.toBeNull());
  document.querySelector<HTMLButtonElement>(`[data-document="${id}"]`)!.click();
  await vi.waitFor(() => expect(document.querySelector(".cm-editor")).not.toBeNull());
  return EditorView.findFromDOM(document.querySelector(".cm-editor")!)!;
}

async function switchDocument(id:string, expectedText:string) {
  await vi.waitFor(() => expect(document.querySelector(`[data-document="${id}"]`)).not.toBeNull());
  document.querySelector<HTMLButtonElement>(`[data-document="${id}"]`)!.click();
  await vi.waitFor(() => expect(EditorView.findFromDOM(document.querySelector(".cm-editor")!)?.state.doc.toString()).toBe(expectedText));
}

test("재열린 문서는 최초 충돌과 고정된 합본 작업을 찾아 보여주며 이후 초안을 보존한다", async () => {
  const view = await openEditor();
  await vi.waitFor(() => expect(fakes.resolution).toHaveBeenCalledWith(binding, nodeId, conflictId));
  await vi.waitFor(() => expect(fakes.mergeStatus).toHaveBeenCalledWith(binding, nodeId, conflictId, "0"));
  await vi.waitFor(() => expect(document.body.textContent).toContain(mergeId));
  expect(document.body.textContent).toMatch(/순번\s*2/);
  expect(document.body.textContent).not.toMatch(/합본.*저장 완료/);
  expect(fakes.queueMerge).not.toHaveBeenCalled();
  expect(fakes.continueChoice).not.toHaveBeenCalled();
  expect(fakes.confirmMerge).not.toHaveBeenCalled();
  expect(view.state.doc.toString()).toBe("approved merge\nlater draft\n");
  view.dispatch({ changes:{ from:view.state.doc.length, insert:"another edit" }, userEvent:"input.type" });
  document.querySelector<HTMLButtonElement>("#save")!.click();
  await vi.waitFor(() => expect(fakes.save).toHaveBeenCalledWith(expect.objectContaining({ edit_session_id:"merge-session", generation:"5", bytes_base64:btoa("approved merge\nlater draft\nanother edit") })));
  expect(fakes.queueMerge).not.toHaveBeenCalled();
  expect(fakes.continueChoice).not.toHaveBeenCalled();
});

test("처음 발견한 충돌이 없으면 합본 상태 조회나 선택을 시작하지 않는다", async () => {
  fakes.outbox.mockResolvedValue({ ...outbox, latest:null, first_content_conflict:null, has_conflict:false });
  const view = await openEditor();
  await vi.waitFor(() => expect(fakes.outbox).toHaveBeenCalledWith(binding, nodeId));
  await new Promise((resolve) => setTimeout(resolve, 0));
  expect(fakes.resolution).not.toHaveBeenCalled();
  expect(fakes.mergeStatus).not.toHaveBeenCalled();
  expect(fakes.queueMerge).not.toHaveBeenCalled();
  expect(fakes.continueChoice).not.toHaveBeenCalled();
  expect(document.querySelector('[data-testid="conflict-merge-queue"]')).toBeNull();
  expect(view.state.doc.toString()).toBe(fakes.documentText);
});

test("동결된 합본 선택은 자동 진행하지 않고 보존된 초안을 유지한다", async () => {
  fakes.resolution.mockResolvedValue({ ...resolution, state:{ ...pending, frozen:true } });
  const view = await openEditor();
  await vi.waitFor(() => expect(fakes.resolution).toHaveBeenCalledWith(binding, nodeId, conflictId));
  await new Promise((resolve) => setTimeout(resolve, 0));
  expect(fakes.queueMerge).not.toHaveBeenCalled();
  expect(fakes.continueChoice).not.toHaveBeenCalled();
  expect(document.querySelector('[data-testid="conflict-merge-queue"]')).toBeNull();
  expect(view.state.doc.toString()).toBe(fakes.documentText);
});

test("합본 작업이 아직 없으면 자동 queue 없이 보류 상태로 남는다", async () => {
  fakes.documentGeneration = "2"; fakes.documentText = "approved merge\n";
  fakes.outbox.mockResolvedValue({ ...outbox, latest:null });
  fakes.mergeStatus.mockResolvedValue({ ...mergeStatus, merge_operation:null });
  const view = await openEditor();
  await vi.waitFor(() => expect(fakes.mergeStatus).toHaveBeenCalledWith(binding, nodeId, conflictId, "0"));
  await vi.waitFor(() => expect(document.querySelector('[data-testid="conflict-merge-queue"]')).not.toBeNull());
  expect(fakes.queueMerge).not.toHaveBeenCalled();
  expect(fakes.continueChoice).not.toHaveBeenCalled();
  expect(view.state.doc.toString()).toBe("approved merge\n");
});

test("등록 교체 후 늦은 충돌 상태는 이전 합본을 새 등록 화면에 표시하지 않는다", async () => {
  const delayed = deferred<typeof resolution>();
  fakes.resolution.mockReturnValueOnce(delayed.promise);
  const view = await openEditor();
  await vi.waitFor(() => expect(fakes.resolution).toHaveBeenCalledWith(binding, nodeId, conflictId));
  fakes.active = { ...binding, workspace_epoch:"new-epoch" }; fakes.syncStatus = "connecting";
  fakes.listeners.get("workspace-sync-status-changed")?.();
  await vi.waitFor(() => expect(document.querySelector("#workspace-sync-status")?.textContent).toContain("서버 동기화 중"));
  delayed.resolve(resolution);
  await new Promise((resolve) => setTimeout(resolve, 0));
  expect(fakes.mergeStatus).not.toHaveBeenCalledWith(binding, nodeId, conflictId, "0");
  expect(document.body.textContent).not.toContain(mergeId);
  expect(fakes.queueMerge).not.toHaveBeenCalled();
  expect(fakes.continueChoice).not.toHaveBeenCalled();
  expect(view.state.doc.toString()).toBe(fakes.documentText);
});

test("다른 문서 session으로 전환한 뒤 늦은 합본 상태는 현재 화면에 적용하지 않는다", async () => {
  const delayed = deferred<typeof mergeStatus>();
  fakes.mergeStatus.mockReturnValueOnce(delayed.promise);
  await openEditor();
  await vi.waitFor(() => expect(fakes.mergeStatus).toHaveBeenCalledWith(binding, nodeId, conflictId, "0"));
  await switchDocument(otherId, "other document\n");
  delayed.resolve(mergeStatus);
  await new Promise((resolve) => setTimeout(resolve, 0));
  expect(document.body.textContent).not.toContain(mergeId);
  expect(document.querySelector('[data-testid="conflict-merge-queue"]')).toBeNull();
  expect(fakes.queueMerge).not.toHaveBeenCalled();
  expect(fakes.continueChoice).not.toHaveBeenCalled();
  expect(EditorView.findFromDOM(document.querySelector(".cm-editor")!)?.state.doc.toString()).toBe("other document\n");
});

async function chooseDifferentResolution() {
  const encoder = new TextEncoder();
  const shown = { binding, node_id:nodeId, operation_id:conflictId, operation_seq:"1", generation:"4",
    base_revision:"base-rev", base_bytes:encoder.encode("base\n"), base_hash:"sha256:f34848ca92665c342abd5816c9e3eda0e82180671195362bcd0080544a3bc2ac",
    local_bytes:encoder.encode("approved merge\nlater draft\n"), local_hash:"sha256:acf4f8f4bf8b03e57d09b0ab2b3565c5e4f141819947535791725b6010c23265",
    conflict_revision:"revision-at-412", remote_revision:"current-rev", remote_bytes:encoder.encode("remote\n"), remote_hash:"sha256:c81662749eb30a2c7f6b6b9b1a561a7b0be5cc115759b72442ddb476e7712751" };
  fakes.prepareConflict.mockResolvedValue(shown);
  fakes.resolution.mockResolvedValue({ ...resolution, state:{ ...pending, needs_review:true } });
  fakes.chooseRemote.mockImplementation(async () => {
    fakes.resolution.mockResolvedValue({ ...resolution, state:{ ...pending, version:"1", generation:"4", choice:"choose_remote", needs_review:false } });
    return { status:"pending", version:"1" };
  });
  await vi.waitFor(() => expect(document.querySelector('[data-testid="conflict-review-open"]')).not.toBeNull());
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-review-open"]')!.click();
  await vi.waitFor(() => expect(document.querySelector('[data-testid="conflict-choose-remote"]')).not.toBeNull());
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-choose-remote"]')!.click();
  await vi.waitFor(() => expect(document.querySelector('[data-testid="conflict-choose-remote-confirm"]')).not.toBeNull());
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-choose-remote-confirm"]')!.click();
  await vi.waitFor(() => expect(fakes.chooseRemote).toHaveBeenCalledExactlyOnceWith(shown, "merge-session", "0"));
  await vi.waitFor(() => expect(document.querySelector('[data-testid="conflict-continue"]')).not.toBeNull());
}

test("서버 본문을 새로 선택하면 이전 합본 패널은 사라진다", async () => {
  fakes.resolution.mockResolvedValue({ ...resolution, state:{ ...pending, needs_review:true } });
  fakes.mergeStatus.mockResolvedValue({ ...mergeStatus, merge_operation:null });
  const view = await openEditor();
  await vi.waitFor(() => expect(document.querySelector('[aria-label="합본 저장 상태"]')).not.toBeNull());
  await chooseDifferentResolution();
  await vi.waitFor(() => expect(document.querySelector('[aria-label="합본 저장 상태"]')).toBeNull());
  expect(fakes.queueMerge).not.toHaveBeenCalled();
  expect(view.state.doc.toString()).toBe(fakes.documentText);
});

test("다른 선택이 끝난 뒤 늦게 온 합본 조회는 이전 패널을 되살리지 않는다", async () => {
  const olderStatus = { ...mergeStatus, merge_operation:null };
  const delayed = deferred<typeof olderStatus>();
  fakes.resolution.mockResolvedValue({ ...resolution, state:{ ...pending, needs_review:true } });
  fakes.mergeStatus.mockReturnValue(delayed.promise);
  fakes.transmit.mockResolvedValue({ binding, status:"pending", operation_id:null, document_id:null, generation:null, content_block:null });
  const view = await openEditor();
  await vi.waitFor(() => expect(fakes.mergeStatus).toHaveBeenCalledWith(binding, nodeId, conflictId, "0"));
  await new Promise((resolve) => setTimeout(resolve, 50));
  const outboxCalls = fakes.outbox.mock.calls.length;
  await chooseDifferentResolution();
  await vi.waitFor(() => expect(document.querySelector('[aria-label="합본 저장 상태"]')).toBeNull());
  expect(fakes.outbox.mock.calls).toHaveLength(outboxCalls);
  await act(async () => { delayed.resolve(olderStatus); await delayed.promise; });
  expect(fakes.outbox.mock.calls).toHaveLength(outboxCalls);
  expect(document.querySelector('[aria-label="합본 저장 상태"]')).toBeNull();
  expect(fakes.queueMerge).not.toHaveBeenCalled();
  expect(view.state.doc.toString()).toBe(fakes.documentText);
});
