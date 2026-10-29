// @vitest-environment jsdom
import { EditorView } from "@codemirror/view";
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

async function discoverMerge() {
  const view = await openEditor();
  await vi.waitFor(() => expect(fakes.resolution).toHaveBeenCalledWith(binding, nodeId, conflictId));
  await vi.waitFor(() => expect(fakes.mergeStatus).toHaveBeenCalledWith(binding, nodeId, conflictId, "0"));
  await vi.waitFor(() => expect(document.body.textContent).toContain(mergeId));
  await vi.waitFor(() => expect(fakes.listeners.has("workspace-metadata-changed")).toBe(true));
  return view;
}

function signalMetadata() {
  const listener = fakes.listeners.get("workspace-metadata-changed");
  expect(listener).toBeDefined();
  listener!();
}

test("충돌 목록이 사라져도 pending은 대기하고 확인된 합본 완료만 자동 표시한다", async () => {
  const view = await discoverMerge();
  const waitForPending = deferred<typeof resolution>();
  fakes.outbox.mockResolvedValue({ ...outbox, first_content_conflict:null, has_conflict:false, confirmed_through:"1" });
  fakes.resolution.mockReset().mockReturnValueOnce(waitForPending.promise);
  signalMetadata();
  await vi.waitFor(() => expect(fakes.resolution).toHaveBeenCalledWith(binding, nodeId, conflictId));
  expect(document.body.textContent).not.toMatch(/합본.*저장 완료/);
  expect(document.querySelector('[aria-label="합본 저장 상태"]')).not.toBeNull();
  waitForPending.resolve(resolution);
  await new Promise((resolve) => setTimeout(resolve, 0));
  await vi.waitFor(() => expect(document.body.textContent).not.toMatch(/합본.*저장 완료/));
  expect(document.querySelector('[aria-label="합본 저장 상태"]')).not.toBeNull();
  expect(view.state.doc.toString()).toBe("approved merge\nlater draft\n");

  fakes.outbox.mockResolvedValue({ ...outbox, first_content_conflict:null, has_conflict:false, confirmed_through:"2" });
  fakes.resolution.mockReset().mockResolvedValue({ ...resolution, state:{ status:"completed", generation:"3", frozen:false } });
  signalMetadata();
  await vi.waitFor(() => expect(fakes.resolution).toHaveBeenCalledWith(binding, nodeId, conflictId));
  await vi.waitFor(() => expect(document.body.textContent).toMatch(/합본.*저장 완료/));
  expect(fakes.queueMerge).not.toHaveBeenCalled();
  expect(fakes.continueChoice).not.toHaveBeenCalled();
  expect(view.state.doc.toString()).toBe("approved merge\nlater draft\n");
  view.dispatch({ changes:{ from:view.state.doc.length, insert:"another edit" }, userEvent:"input.type" });
  document.querySelector<HTMLButtonElement>("#save")!.click();
  await vi.waitFor(() => expect(fakes.save).toHaveBeenCalledWith(expect.objectContaining({ edit_session_id:"merge-session", generation:"5", bytes_base64:btoa("approved merge\nlater draft\nanother edit") })));
});

test("완료 응답의 generation이나 동결 상태가 다르면 자동 완료하지 않는다", async () => {
  await discoverMerge();
  fakes.outbox.mockResolvedValue({ ...outbox, first_content_conflict:null, has_conflict:false, confirmed_through:"2" });
  for (const state of [
    { status:"completed", generation:"2", frozen:false },
    { status:"completed", generation:"4", frozen:false },
    { status:"completed", generation:"3", frozen:true },
  ]) {
    fakes.resolution.mockReset().mockResolvedValue({ ...resolution, state });
    signalMetadata();
    await vi.waitFor(() => expect(fakes.resolution).toHaveBeenCalledWith(binding, nodeId, conflictId));
    await new Promise((resolve) => setTimeout(resolve, 0));
    expect(document.body.textContent).not.toMatch(/합본.*저장 완료/);
    expect(document.querySelector('[aria-label="합본 저장 상태"]')).not.toBeNull();
  }
  expect(fakes.queueMerge).not.toHaveBeenCalled();
  expect(fakes.continueChoice).not.toHaveBeenCalled();
});

test("등록 교체 후 늦은 합본 완료 응답은 새 등록 화면에 표시하지 않는다", async () => {
  const view = await discoverMerge();
  const delayed = deferred<{ binding:WorkspaceRegistration; document_id:string; operation_id:string; state:{ status:"completed"; generation:string; frozen:boolean } }>();
  fakes.outbox.mockResolvedValue({ ...outbox, first_content_conflict:null, has_conflict:false, confirmed_through:"2" });
  fakes.resolution.mockReset().mockReturnValueOnce(delayed.promise);
  signalMetadata();
  await vi.waitFor(() => expect(fakes.resolution).toHaveBeenCalledWith(binding, nodeId, conflictId));
  fakes.active = { ...binding, workspace_epoch:"new-epoch" }; fakes.syncStatus = "connecting";
  fakes.listeners.get("workspace-sync-status-changed")?.();
  await vi.waitFor(() => expect(document.querySelector("#workspace-sync-status")?.textContent).toContain("서버 동기화 중"));
  delayed.resolve({ ...resolution, state:{ status:"completed", generation:"3", frozen:false } });
  await new Promise((resolve) => setTimeout(resolve, 0));
  expect(document.body.textContent).not.toMatch(/합본.*저장 완료/);
  expect(view.state.doc.toString()).toBe("approved merge\nlater draft\n");
  expect(fakes.queueMerge).not.toHaveBeenCalled();
});

test("다른 문서 session으로 전환한 뒤 늦은 합본 완료 응답을 적용하지 않는다", async () => {
  await discoverMerge();
  const delayed = deferred<{ binding:WorkspaceRegistration; document_id:string; operation_id:string; state:{ status:"completed"; generation:string; frozen:boolean } }>();
  fakes.outbox.mockImplementation(async (current:WorkspaceRegistration, document:string) => document === nodeId ? { ...outbox, first_content_conflict:null, has_conflict:false, confirmed_through:"2" } : ({ binding:current, document_id:document, latest:null, first_content_conflict:null, has_conflict:false, has_failure:false, confirmed_through:"0" }));
  fakes.resolution.mockReset().mockReturnValueOnce(delayed.promise);
  signalMetadata();
  await vi.waitFor(() => expect(fakes.resolution).toHaveBeenCalledWith(binding, nodeId, conflictId));
  await switchDocument(otherId, "other document\n");
  delayed.resolve({ ...resolution, state:{ status:"completed", generation:"3", frozen:false } });
  await new Promise((resolve) => setTimeout(resolve, 0));
  expect(document.body.textContent).not.toMatch(/합본.*저장 완료/);
  expect(EditorView.findFromDOM(document.querySelector(".cm-editor")!)?.state.doc.toString()).toBe("other document\n");
  expect(fakes.queueMerge).not.toHaveBeenCalled();
});
