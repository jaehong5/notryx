// @vitest-environment jsdom
import { EditorState } from "@codemirror/state";
import { EditorView } from "@codemirror/view";
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import type { WorkspaceRegistration } from "../../../packages/protocol-types";

const fakes = vi.hoisted(() => ({
  listeners: new Map<string, () => void>(),
  root: undefined as { unmount(): void } | undefined,
  choose: vi.fn(), resolution: vi.fn(), reopen: vi.fn(), continueChoice: vi.fn(), completeChoice: vi.fn(),
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
    workspaceSyncState: async () => ({ binding:fakes.active, status: "ready" }),
    workspaceInventory: async () => ({ nodes: fakes.deleted ? [rootNode] : [rootNode, node], next_after_id: null }),
    openWorkspaceDocument: async () => ({ binding, document_id: nodeId, edit_session_id: "open-session", generation: fakes.generation, bytes_base64: btoa("local"), readonly: false, local_draft: false, has_session: true, cache_warning: null, open_notice: null }),
    workspaceDocumentOutbox: fakes.outbox,
    prepareWorkspaceRemoteRefresh: fakes.prepare, commitWorkspaceRemoteRefresh: fakes.commit,
    inspectWorkspaceRemoteChange: fakes.inspect, pairWorkspace: fakes.pair,
    saveWorkspaceDocument: fakes.save, queueWorkspaceDocument: fakes.queue,
    sendNextWorkspaceOperation: fakes.transmit,
    closeWorkspaceDocument: fakes.close, recoverWorkspaceSource: fakes.recover, prepareWorkspaceContentConflict:fakes.prepareConflict,
    chooseWorkspaceRemoteContentConflict: fakes.choose, workspaceContentConflictResolutionStatus: fakes.resolution,
    reopenWorkspaceContentConflictCheckpoint: fakes.reopen, continueWorkspaceRemoteContentConflict: fakes.continueChoice, completeWorkspacePendingRemoteContentConflict: fakes.completeChoice,
  };
});

const binding = { server_origin: "https://example.invalid", workspace_id: "workspace", workspace_epoch: "epoch", device_id: "device", device_stream_id: "stream" };
const rootId = "019a1000-0000-7000-8000-000000000001", nodeId = "019a1000-0000-7000-8000-000000000002";
const rootNode = { node_id: rootId, parent_id: null, name: "", kind: "folder" as const, deleted: false, content_rev: null, content_etag: null, meta_rev: "meta", meta_etag: '"meta"', node_etag: '"node"', content_hash: null, size: null, children_generation: "0", last_change_seq: "1" };
const node = { ...rootNode, node_id: nodeId, parent_id: rootId, name: "synthetic.md", kind: "file" as const, content_rev: "content", content_etag: '"content"', content_hash: "sha256:" + "a".repeat(64), size: "4" };
const snapshot = { binding, node_id:nodeId, operation_id:"019a1000-0000-7000-8000-000000000003", operation_seq:"1", generation:"1", base_revision:"base-rev", base_bytes:new TextEncoder().encode("base"), base_hash:"sha256:cae662172fd450bb0cd710a769079c05bfc5d8e35efa6576edc7d0377afdd4a2", local_bytes:new TextEncoder().encode("local"), local_hash:"sha256:25bf8e1a2393f1108d37029b3df5593236c755742ec93465bbafa9b290bddcf6", conflict_revision:"revision-at-412", remote_revision:"current-rev", remote_bytes:new TextEncoder().encode("remote"), remote_hash:"sha256:b71199ebd070b36beab7317920c2c2f1d777df8d05e5527d8458fda57cb17a7a" };
const rangeRects = Object.getOwnPropertyDescriptor(Range.prototype, "getClientRects");
const rangeBounds = Object.getOwnPropertyDescriptor(Range.prototype, "getBoundingClientRect");

beforeEach(() => {
  Object.defineProperty(Range.prototype, "getClientRects", { configurable: true, value: () => [] });
  Object.defineProperty(Range.prototype, "getBoundingClientRect", { configurable: true, value: () => ({ left: 0, right: 0, top: 0, bottom: 0, width: 0, height: 0 }) });
  vi.resetModules();
  fakes.deleted = false;
  fakes.active = binding;
  fakes.generation = "1";
  fakes.outbox.mockReset().mockResolvedValue({ binding, document_id:nodeId, first_content_conflict:{ operation_id:snapshot.operation_id, operation_seq:snapshot.operation_seq }, has_conflict:true, has_failure:false, latest:null, confirmed_through:"0" });
  fakes.close.mockReset().mockResolvedValue(undefined);
  fakes.recover.mockReset().mockResolvedValue(true);
  fakes.prepareConflict.mockReset().mockResolvedValue(snapshot);
  fakes.prepare.mockReset().mockResolvedValue(null);
  fakes.commit.mockReset().mockResolvedValue(true);
  fakes.inspect.mockReset().mockResolvedValue("same");
  fakes.save.mockReset().mockImplementation(async (request: { edit_session_id: string; generation: string }) => ({ edit_session_id: request.edit_session_id, generation: request.generation }));
  fakes.queue.mockReset().mockImplementation(async (document_id: string, _session: string, generation: string) => ({ operation_id: "019a1000-0000-7000-8000-000000000004", document_id, generation, committed: false }));
  fakes.choose.mockReset(); fakes.resolution.mockReset(); fakes.reopen.mockReset(); fakes.continueChoice.mockReset(); fakes.completeChoice.mockReset();
  fakes.resolution.mockResolvedValue({ binding, document_id: nodeId, operation_id: snapshot.operation_id, state: { status: "not_found" } });
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

async function openEditor(expectPristineRefresh = true): Promise<EditorView> {
  document.body.innerHTML = '<div id="root"></div>';
  await import("./main");
  await vi.waitFor(() => expect(document.querySelector(`[data-document="${nodeId}"]`)).not.toBeNull());
  document.querySelector<HTMLButtonElement>(`[data-document="${nodeId}"]`)!.click();
  await vi.waitFor(() => expect(document.querySelector(".cm-editor")).not.toBeNull());
  if (expectPristineRefresh) await vi.waitFor(() => expect(fakes.prepare).toHaveBeenCalled());
  fakes.prepare.mockClear();
  return EditorView.findFromDOM(document.querySelector(".cm-editor")!)!;
}



async function showConflictButton(): Promise<EditorView> {
  const view = await openEditor(false);
  await vi.waitFor(() => expect(document.querySelector('[data-testid="conflict-review-open"]')).not.toBeNull());
  return view;
}

const operationId = snapshot.operation_id;
const completedState = { status: "completed" as const, generation: "2", frozen: false };
const pendingState = { status: "pending" as const, version: "0", generation: "1", choice: "choose_remote" as const, needs_review: false, frozen: false };
const reopened = (state: typeof completedState | typeof pendingState, text: string) => ({
  state,
  document: { binding, document_id: nodeId, edit_session_id: "reopened-session", generation: state.generation, bytes_base64: btoa(text), readonly: false, local_draft: true, has_session: true, cache_warning: null, open_notice: null },
});

async function showComparison() {
  const view = await showConflictButton();
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-review-open"]')!.click();
  await vi.waitFor(() => expect(document.querySelector<HTMLTextAreaElement>('[aria-label="조회 당시 서버 본문"]')?.value).toBe("remote"));
  return view;
}

async function chooseRemote() {
  await vi.waitFor(() => expect(document.querySelector('[data-testid="conflict-choose-remote"]')).not.toBeNull());
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-choose-remote"]')!.click();
  await vi.waitFor(() => expect(document.querySelector('[data-testid="conflict-choose-remote-confirm"]')).not.toBeNull());
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-choose-remote-confirm"]')!.click();
}

test("비교한 원문을 본 뒤 별도 확인한 경우에만 해당 snapshot의 원격판을 선택한다", async () => {
  fakes.choose.mockResolvedValue({ status: "pending", version: "0" });
  const view = await showComparison();
  await vi.waitFor(() => expect(document.querySelector('[data-testid="conflict-choose-remote"]')).not.toBeNull());
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-choose-remote"]')!.click();
  await vi.waitFor(() => expect(document.querySelector('[data-testid="conflict-choose-remote-confirm"]')).not.toBeNull());
  expect(fakes.choose).not.toHaveBeenCalled();
  expect(document.body.textContent).toMatch(/초안|보존/);
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-choose-remote-confirm"]')!.click();
  await vi.waitFor(() => expect(fakes.choose).toHaveBeenCalledExactlyOnceWith(snapshot, "open-session", null));
  await vi.waitFor(() => expect(document.body.textContent).toMatch(/보류|대기/));
  expect(document.body.textContent).toContain("후속 처리를 직접 진행");
  expect(fakes.continueChoice).not.toHaveBeenCalled();
  expect(fakes.completeChoice).not.toHaveBeenCalled();
  expect(fakes.reopen).not.toHaveBeenCalled();
  expect(view.state.doc.toString()).toBe("local");
});

test("완료된 선택은 새 session과 durable 원문을 열고 복구 직후 자동 queue하지 않는다", async () => {
  fakes.choose.mockResolvedValue({ status: "completed", generation: "2" });
  fakes.reopen.mockResolvedValue(reopened(completedState, "remote"));
  await showComparison();
  await chooseRemote();
  await vi.waitFor(() => expect(fakes.reopen).toHaveBeenCalledExactlyOnceWith(binding, nodeId, operationId, "open-session"));
  await vi.waitFor(() => expect(EditorView.findFromDOM(document.querySelector(".cm-editor")!)?.state.doc.toString()).toBe("remote"));
  expect(fakes.queue).not.toHaveBeenCalled();
  expect(fakes.close).not.toHaveBeenCalledWith(expect.objectContaining({ edit_session_id: "open-session" }));
  fakes.outbox.mockResolvedValue({ binding, document_id: nodeId, first_content_conflict:null, has_conflict: false, has_failure: false, latest: null, confirmed_through: "0" });
  fakes.save.mockClear();
  const view = EditorView.findFromDOM(document.querySelector(".cm-editor")!)!;
  view.dispatch({ changes: { from: view.state.doc.length, insert: " new" }, userEvent: "input.type" });
  document.querySelector<HTMLButtonElement>("#save")!.click();
  await vi.waitFor(() => expect(fakes.save).toHaveBeenCalledWith(expect.objectContaining({ document_id: nodeId, edit_session_id: "reopened-session", generation: "3" })));
  expect(view.state.doc.toString()).toBe("remote new");
});

test("선택 응답이 유실되면 사용자 확인으로 durable 상태를 조회하고 pending 초안을 새 session으로 연다", async () => {
  fakes.choose.mockRejectedValue(new Error("response lost"));
  fakes.reopen.mockResolvedValue(reopened(pendingState, "local"));
  const oldView = await showComparison();
  await vi.waitFor(() => expect(document.querySelector('[data-testid="conflict-choose-remote"]')).not.toBeNull());
  expect(fakes.resolution).toHaveBeenCalledWith(binding, nodeId, operationId);
  fakes.resolution.mockClear().mockResolvedValue({ binding, document_id: nodeId, operation_id: operationId, state: pendingState });
  await chooseRemote();
  await vi.waitFor(() => expect(document.querySelector('[data-testid="conflict-recover-result"]')).not.toBeNull());
  expect(oldView.state.facet(EditorState.readOnly)).toBe(true);
  expect(fakes.resolution.mock.calls.every(([selectedBinding, documentId, selectedOperation]) => JSON.stringify(selectedBinding) === JSON.stringify(binding) && documentId === nodeId && selectedOperation === operationId)).toBe(true);
  expect(fakes.reopen).not.toHaveBeenCalled();
  expect(fakes.continueChoice).not.toHaveBeenCalled();
  expect(fakes.queue).not.toHaveBeenCalled();
  fakes.resolution.mockClear();
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-recover-result"]')!.click();
  await vi.waitFor(() => expect(fakes.resolution).toHaveBeenCalledWith(binding, nodeId, operationId));
  await vi.waitFor(() => expect(fakes.reopen).toHaveBeenCalledWith(binding, nodeId, operationId, "open-session"));
  await vi.waitFor(() => expect(EditorView.findFromDOM(document.querySelector(".cm-editor")!)?.state.doc.toString()).toBe("local"));
  expect(fakes.continueChoice).not.toHaveBeenCalled();
  expect(fakes.queue).not.toHaveBeenCalled();
  const view = EditorView.findFromDOM(document.querySelector(".cm-editor")!)!;
  view.dispatch({ changes: { from: view.state.doc.length, insert: " later" }, userEvent: "input.type" });
  document.querySelector<HTMLButtonElement>("#save")!.click();
  await vi.waitFor(() => expect(fakes.save).toHaveBeenCalledWith(expect.objectContaining({ document_id: nodeId, edit_session_id: "reopened-session", generation: "2" })));
  expect(view.state.doc.toString()).toBe("local later");
});

test("재열기 응답도 유실되면 자동 재로드하지 않고 초안 보존과 앱 재시작을 안내한다", async () => {
  fakes.choose.mockRejectedValue(new Error("response lost"));
  fakes.reopen.mockRejectedValue(new Error("reopen response lost"));
  const oldView = await showComparison();
  await vi.waitFor(() => expect(document.querySelector('[data-testid="conflict-choose-remote"]')).not.toBeNull());
  expect(fakes.resolution).toHaveBeenCalledWith(binding, nodeId, operationId);
  fakes.resolution.mockClear().mockResolvedValue({ binding, document_id: nodeId, operation_id: operationId, state: pendingState });
  await chooseRemote();
  await vi.waitFor(() => expect(document.querySelector('[data-testid="conflict-recover-result"]')).not.toBeNull());
  expect(fakes.resolution.mock.calls.every(([selectedBinding, documentId, selectedOperation]) => JSON.stringify(selectedBinding) === JSON.stringify(binding) && documentId === nodeId && selectedOperation === operationId)).toBe(true);
  expect(fakes.reopen).not.toHaveBeenCalled();
  expect(fakes.continueChoice).not.toHaveBeenCalled();
  expect(fakes.queue).not.toHaveBeenCalled();
  fakes.resolution.mockClear();
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-recover-result"]')!.click();
  await vi.waitFor(() => expect(fakes.resolution).toHaveBeenCalledWith(binding, nodeId, operationId));
  await vi.waitFor(() => expect(fakes.reopen).toHaveBeenCalledWith(binding, nodeId, operationId, "open-session"));
  await vi.waitFor(() => expect(document.body.textContent).toMatch(/재시작|다시 시작/));
  expect(document.body.textContent).toMatch(/초안|보존/);
  expect(oldView.state.facet(EditorState.readOnly)).toBe(true);
  expect(oldView.state.doc.toString()).toBe("local");
  expect(fakes.continueChoice).not.toHaveBeenCalled();
  expect(fakes.queue).not.toHaveBeenCalled();
});

test("원격판 확인 중 새 입력이 발생하면 오래된 비교 결과를 선택하지 않는다", async () => {
  const view = await showComparison();
  await vi.waitFor(() => expect(document.querySelector('[data-testid="conflict-choose-remote"]')).not.toBeNull());
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-choose-remote"]')!.click();
  await vi.waitFor(() => expect(document.querySelector('[data-testid="conflict-choose-remote-confirm"]')).not.toBeNull());
  const oldConfirm = document.querySelector<HTMLButtonElement>('[data-testid="conflict-choose-remote-confirm"]')!;
  view.dispatch({ changes: { from: view.state.doc.length, insert: " newer" }, userEvent: "input.type" });
  oldConfirm.click();
  await new Promise((resolve) => setTimeout(resolve, 0));
  expect(fakes.choose).not.toHaveBeenCalled();
  expect(view.state.doc.toString()).toBe("local newer");
});

test("확인 직전 한글 조합이 시작되면 선택하지 않고 조합을 유지한다", async () => {
  const view = await showComparison();
  await vi.waitFor(() => expect(document.querySelector('[data-testid="conflict-choose-remote"]')).not.toBeNull());
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-choose-remote"]')!.click();
  await vi.waitFor(() => expect(document.querySelector('[data-testid="conflict-choose-remote-confirm"]')).not.toBeNull());
  view.contentDOM.dispatchEvent(new CompositionEvent("compositionstart", { bubbles: true }));
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-choose-remote-confirm"]')!.click();
  await vi.waitFor(() => expect(document.body.textContent).toMatch(/조합/));
  expect(fakes.choose).not.toHaveBeenCalled();
  expect(view.state.doc.toString()).toBe("local");
  view.contentDOM.dispatchEvent(new CompositionEvent("compositionend", { bubbles: true }));
});
