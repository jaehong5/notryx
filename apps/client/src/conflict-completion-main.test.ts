// @vitest-environment jsdom
import { EditorView } from "@codemirror/view";
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import type { AutoTransmit } from "./auto-transmit";
import type { WorkspaceRegistration } from "../../../packages/protocol-types";

const fakes = vi.hoisted(() => ({
  listeners: new Map<string, () => void>(),
  root: undefined as { unmount(): void } | undefined,
  scheduler: undefined as AutoTransmit | undefined,
  resumeContent: vi.fn(), kick: vi.fn(),
  syncStatus: "ready",
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
vi.mock("./auto-transmit", async (original) => {
  const actual = await original<typeof import("./auto-transmit")>();
  return { ...actual, AutoTransmit: class extends actual.AutoTransmit {
    constructor(ports: ConstructorParameters<typeof actual.AutoTransmit>[0]) { super(ports); fakes.scheduler = this; }
    resumeContentAfterAbandon(binding: string, operationId: string) {
      const result = super.resumeContentAfterAbandon(binding, operationId);
      fakes.resumeContent(binding, operationId, result);
      return result;
    }
    kick() { fakes.kick(); return super.kick(); }
  } };
});
vi.mock("./ipc", async (original) => {
  const actual = await original<typeof import("./ipc")>();
  return { ...actual,
    localDrafts: async () => [], pendingPairStatus: async () => "none",
    workspaceSyncState: async () => ({ binding:fakes.active, status: fakes.syncStatus }),
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
  fakes.deleted = false; fakes.scheduler = undefined; fakes.syncStatus = "ready"; fakes.resumeContent.mockReset(); fakes.kick.mockReset();
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
  fakes.transmit.mockReset().mockResolvedValue({ status: "idle", binding });
  fakes.pair.mockReset().mockResolvedValue(undefined);
});

afterEach(async () => {
  fakes.root?.unmount(); fakes.root = undefined; fakes.scheduler = undefined;
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

const blockedOperation = "019a1000-0000-7000-8000-000000000004";

async function pendingChoice() {
  fakes.choose.mockResolvedValue({ status: "pending", version: "0" });
  fakes.resolution.mockResolvedValue({ binding, document_id: nodeId, operation_id: operationId, state: { status: "not_found" } });
  await showComparison();
  await chooseRemote();
  await vi.waitFor(() => expect(fakes.choose).toHaveBeenCalledOnce());
  await vi.waitFor(() => expect(document.querySelector('[data-testid="conflict-continue"]')).not.toBeNull());
}

const anotherOperation = "019a1000-0000-7000-8000-000000000005";
const pendingState = { status: "pending" as const, version: "0", generation: "1", choice: "choose_remote" as const, needs_review: false, frozen: false };
const completedState = { status: "completed" as const, generation: "2", frozen: false };
const statusResponse = (state: object) => ({ binding, document_id: nodeId, operation_id: operationId, state });
const reopenResponse = (state: typeof pendingState | typeof completedState, text: string) => ({
  state, document: { binding, document_id: nodeId, edit_session_id: "reopened-session", generation: state.generation, bytes_base64: btoa(text), readonly: false, local_draft: true, has_session: true, cache_warning: null, open_notice: null },
});

async function completeChoice() {
  await vi.waitFor(() => expect(document.querySelector('[data-testid="conflict-complete"]')).not.toBeNull());
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-complete"]')!.click();
  await vi.waitFor(() => expect(fakes.completeChoice).toHaveBeenCalled());
}

test("여러 후속 작업은 명시 진행을 반복하고 사용자의 완료 선택 후 새 session을 연다", async () => {
  fakes.continueChoice.mockResolvedValueOnce({ status: "pending", version: "0", requested_operation_id: blockedOperation })
    .mockResolvedValueOnce({ status: "pending", version: "0", requested_operation_id: anotherOperation })
    .mockResolvedValueOnce({ status: "pending", version: "0", requested_operation_id: null });
  fakes.completeChoice.mockResolvedValue({ status: "completed", generation: "2" });
  fakes.reopen.mockResolvedValue(reopenResponse(completedState, "remote"));
  await pendingChoice();
  expect(fakes.continueChoice).not.toHaveBeenCalled();
  expect(fakes.completeChoice).not.toHaveBeenCalled();
  for (let count = 1; count <= 3; count++) {
    document.querySelector<HTMLButtonElement>('[data-testid="conflict-continue"]')!.click();
    await vi.waitFor(() => expect(fakes.continueChoice).toHaveBeenCalledTimes(count));
    await vi.waitFor(() => expect(document.querySelector<HTMLButtonElement>('[data-testid="conflict-continue"]')?.disabled).toBe(false));
  }
  expect(fakes.completeChoice).not.toHaveBeenCalled();
  await completeChoice();
  expect(fakes.completeChoice).toHaveBeenCalledWith(binding, nodeId, "open-session", operationId, "0");
  await vi.waitFor(() => expect(fakes.reopen).toHaveBeenCalledWith(binding, nodeId, operationId, "open-session"));
  await vi.waitFor(() => expect(EditorView.findFromDOM(document.querySelector(".cm-editor")!)?.state.doc.toString()).toBe("remote"));
  expect(fakes.queue).not.toHaveBeenCalled();
});

test("서버 ACK 전 완료 거절은 성공으로 표시하지 않고 상태 확인 뒤 초안을 유지한다", async () => {
  fakes.completeChoice.mockRejectedValue(new Error("pending_conflict_resolution"));
  fakes.reopen.mockResolvedValue(reopenResponse(pendingState, "local"));
  await pendingChoice();
  fakes.resolution.mockResolvedValue(statusResponse(pendingState));
  await completeChoice();
  await vi.waitFor(() => expect(document.querySelector('[data-testid="conflict-recover-result"]')).not.toBeNull());
  expect(document.body.textContent).not.toContain("서버 본문을 선택했습니다");
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-recover-result"]')!.click();
  await vi.waitFor(() => expect(fakes.resolution).toHaveBeenCalledWith(binding, nodeId, operationId));
  await vi.waitFor(() => expect(fakes.reopen).toHaveBeenCalledWith(binding, nodeId, operationId, "open-session"));
  await vi.waitFor(() => expect(EditorView.findFromDOM(document.querySelector(".cm-editor")!)?.state.doc.toString()).toBe("local"));
  await vi.waitFor(() => expect(document.querySelector('[data-testid="conflict-continue"]')).not.toBeNull());
  expect(fakes.queue).not.toHaveBeenCalled();
});

test("완료 응답 유실은 상태 조회로 완료를 확인한 뒤 선택된 원문을 새 session으로 연다", async () => {
  fakes.completeChoice.mockRejectedValue(new Error("response lost"));
  fakes.reopen.mockResolvedValue(reopenResponse(completedState, "remote"));
  await pendingChoice();
  fakes.resolution.mockResolvedValue(statusResponse(completedState));
  await completeChoice();
  await vi.waitFor(() => expect(document.querySelector('[data-testid="conflict-recover-result"]')).not.toBeNull());
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-recover-result"]')!.click();
  await vi.waitFor(() => expect(fakes.resolution).toHaveBeenCalledWith(binding, nodeId, operationId));
  await vi.waitFor(() => expect(EditorView.findFromDOM(document.querySelector(".cm-editor")!)?.state.doc.toString()).toBe("remote"));
  expect(fakes.reopen).toHaveBeenCalledWith(binding, nodeId, operationId, "open-session");
  expect(fakes.queue).not.toHaveBeenCalled();
});

test("재시작 후 비교에서 저장된 pending 선택과 version을 복원하고 재승인 없이 진행한다", async () => {
  const state = { ...pendingState, version: "2" };
  fakes.resolution.mockResolvedValue(statusResponse(state));
  fakes.continueChoice.mockResolvedValue({ status: "pending", version: "2", requested_operation_id: blockedOperation });
  await showComparison();
  await vi.waitFor(() => expect(fakes.resolution).toHaveBeenCalledWith(binding, nodeId, operationId));
  await vi.waitFor(() => expect(document.querySelector('[data-testid="conflict-continue"]')).not.toBeNull());
  expect(fakes.choose).not.toHaveBeenCalled();
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-continue"]')!.click();
  await vi.waitFor(() => expect(fakes.continueChoice).toHaveBeenCalledWith(binding, nodeId, "open-session", operationId, "2"));
});

test.each([
  { ...pendingState, needs_review: true },
  { ...pendingState, frozen: true },
  { ...pendingState, choice: "save_merged" as const },
])("재시작 후 재검토·동결·다른 선택 상태는 원격판 진행으로 취급하지 않는다", async (state) => {
  fakes.resolution.mockResolvedValue(statusResponse(state));
  const view = await showComparison();
  await vi.waitFor(() => expect(fakes.resolution).toHaveBeenCalled());
  await vi.waitFor(() => expect(document.body.textContent).toMatch(/다시|대조|이전/));
  expect(document.querySelector('[data-testid="conflict-continue"]')).toBeNull();
  expect(document.querySelector('[data-testid="conflict-complete"]')).toBeNull();
  expect(fakes.continueChoice).not.toHaveBeenCalled();
  expect(fakes.completeChoice).not.toHaveBeenCalled();
  expect(view.state.doc.toString()).toBe("local");
});

test("새 입력 후 재검토는 기존 intent version을 조회하고 새 snapshot을 사용자가 명시 확인한 뒤에만 전송한다", async () => {
  fakes.generation = "2";
  const newer = { ...snapshot, generation: "2" };
  fakes.prepareConflict.mockResolvedValue(newer);
  fakes.resolution.mockResolvedValue(statusResponse({ ...pendingState, version: "2", needs_review: true }));
  fakes.choose.mockResolvedValue({ status: "pending", version: "3" });
  await showComparison();
  await vi.waitFor(() => expect(fakes.resolution).toHaveBeenCalledWith(binding, nodeId, operationId));
  expect(fakes.choose).not.toHaveBeenCalled();
  await chooseRemote();
  await vi.waitFor(() => expect(fakes.choose).toHaveBeenCalledWith(newer, "open-session", "2"));
});

test("재시작 pending 조회가 늦는 동안 등록이 바뀌면 이전 선택을 새 등록에 표시하지 않는다", async () => {
  const releases: Array<(value: ReturnType<typeof statusResponse>) => void> = [];
  fakes.resolution.mockImplementation(() => new Promise((resolve) => { releases.push(resolve); }));
  await showComparison();
  await vi.waitFor(() => expect(fakes.resolution).toHaveBeenCalledWith(binding, nodeId, operationId));
  fakes.active = { ...binding, workspace_epoch: "new-epoch" }; fakes.syncStatus = "connecting";
  fakes.listeners.get("workspace-sync-status-changed")?.();
  await vi.waitFor(() => expect(document.querySelector("#workspace-sync-status")?.textContent).toContain("서버 동기화 중"));
  for (const release of releases) release(statusResponse(pendingState));
  await new Promise((resolve) => setTimeout(resolve, 0));
  expect(document.querySelector('[data-testid="conflict-continue"]')).toBeNull();
  expect(fakes.continueChoice).not.toHaveBeenCalled();
  expect(fakes.completeChoice).not.toHaveBeenCalled();
});

test("pending 복원 뒤 완료하면 이전 session의 비교 화면을 닫는다", async () => {
  fakes.resolution.mockResolvedValue(statusResponse(pendingState));
  fakes.completeChoice.mockResolvedValue({ status: "completed", generation: "2" });
  fakes.reopen.mockResolvedValue(reopenResponse(completedState, "remote"));
  await showComparison();
  await vi.waitFor(() => expect(document.querySelector('[data-testid="conflict-complete"]')).not.toBeNull());
  expect(document.querySelector('[aria-label="충돌 원문 비교"]')).not.toBeNull();
  await completeChoice();
  await vi.waitFor(() => expect(EditorView.findFromDOM(document.querySelector(".cm-editor")!)?.state.doc.toString()).toBe("remote"));
  expect(document.querySelector('[aria-label="충돌 원문 비교"]')).toBeNull();
  expect(document.querySelector('[data-testid="conflict-continue"]')).toBeNull();
  expect(fakes.queue).not.toHaveBeenCalled();
});

test("pending 선택 뒤 서버 원문이 바뀌면 새 서버판을 보고 기존 version으로 명시 재검토할 수 있다", async () => {
  const newer = { ...snapshot, remote_revision: "later-rev", remote_bytes: new TextEncoder().encode("newer remote"), remote_hash: "sha256:d35528057be109d8e037980ae6d555def581e47204eb684f8be4d23e13e50a89" };
  fakes.prepareConflict.mockResolvedValue(newer);
  fakes.resolution.mockResolvedValue(statusResponse(pendingState));
  fakes.choose.mockResolvedValue({ status: "pending", version: "1" });
  await showConflictButton();
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-review-open"]')!.click();
  await vi.waitFor(() => expect(document.querySelector<HTMLTextAreaElement>('[aria-label="조회 당시 서버 본문"]')?.value).toBe("newer remote"));
  await vi.waitFor(() => expect(fakes.resolution).toHaveBeenCalledWith(binding, nodeId, operationId));
  expect(fakes.choose).not.toHaveBeenCalled();
  await chooseRemote();
  await vi.waitFor(() => expect(fakes.choose).toHaveBeenCalledWith(newer, "open-session", "0"));
});
