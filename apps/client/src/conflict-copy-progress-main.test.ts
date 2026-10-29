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
  copyStatus: vi.fn(), copyProgress: vi.fn(), choose: vi.fn(), resolution: vi.fn(), reopen: vi.fn(), continueChoice: vi.fn(), completeChoice: vi.fn(),
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
    workspaceContentConflictCopyStatus: fakes.copyStatus, progressWorkspaceContentConflictCopy: fakes.copyProgress,
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
  fakes.copyStatus.mockReset(); fakes.copyProgress.mockReset();
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
const copyNode = "019a1000-0000-7000-8000-000000000005";
const createOperation = "019a1000-0000-7000-8000-000000000006";
const blockedOperation = "019a1000-0000-7000-8000-000000000004";
const pendingResolution = { binding, document_id: nodeId, operation_id: operationId,
  state: { status: "pending", version: "0", generation: "1", choice: "keep_local_copy", needs_review: false, frozen: false } };
const status = (create_status: string, content_status: string) => ({ binding, document_id: nodeId, operation_id: operationId,
  copy: { version: "0", parent_id: rootId, name: "copy.md", node_id: copyNode, create_operation_id: createOperation, create_status, content_status, followup_operation_id: null as string | null } });

async function restoreCopy(createStatus = "not_queued", contentStatus = "not_queued"): Promise<EditorView> {
  fakes.resolution.mockResolvedValue(pendingResolution);
  fakes.copyStatus.mockResolvedValue(status(createStatus, contentStatus));
  const view = await showConflictButton();
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-review-open"]')!.click();
  await vi.waitFor(() => expect(fakes.resolution).toHaveBeenCalledWith(binding, nodeId, operationId));
  await vi.waitFor(() => expect(fakes.copyStatus).toHaveBeenCalledWith(binding, nodeId, operationId));
  return view;
}

async function progressCopy() {
  await vi.waitFor(() => expect(document.querySelector('[data-testid="conflict-copy-progress"]')).not.toBeNull());
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-copy-progress"]')!.click();
  await vi.waitFor(() => expect(fakes.copyProgress).toHaveBeenCalled());
}

async function refreshCopy() {
  await vi.waitFor(() => expect(document.querySelector('[data-testid="conflict-copy-status-refresh"]')).not.toBeNull());
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-copy-status-refresh"]')!.click();
  await vi.waitFor(() => expect(fakes.copyStatus.mock.calls.length).toBeGreaterThan(1));
}

test("재시작한 앱에서 저장된 사본 대상을 확인하고 사용자 진행 전에는 변경 명령을 호출하지 않는다", async () => {
  const view = await restoreCopy();
  await vi.waitFor(() => expect(document.body.textContent).toContain("copy.md"));
  expect(document.body.textContent).toMatch(/사본.*(대기|보류)|사본.*진행/);
  expect(document.body.textContent).toMatch(/원래.*충돌|충돌.*(유지|남)/);
  expect(document.body.textContent).not.toContain("사본 저장 완료");
  expect(fakes.copyProgress).not.toHaveBeenCalled();
  expect(fakes.choose).not.toHaveBeenCalled();
  expect(fakes.completeChoice).not.toHaveBeenCalled();
  expect(view.state.doc.toString()).toBe("local");
});

test("생성 완료와 본문 ACK 대기는 사본 저장 완료가 아니다", async () => {
  await restoreCopy("created", "pending");
  await vi.waitFor(() => expect(document.body.textContent).toContain("copy.md"));
  expect(document.body.textContent).toMatch(/사본.*(대기|보류|확인 중)/);
  expect(document.body.textContent).not.toContain("사본 저장 완료");
  expect(fakes.copyProgress).not.toHaveBeenCalled();
  expect(fakes.choose).not.toHaveBeenCalled();
});

test("본문 저장과 ACK가 확인된 상태만 사본 저장 완료로 표시하고 원래 충돌을 유지한다", async () => {
  const view = await restoreCopy("created", "saved");
  await vi.waitFor(() => expect(document.body.textContent).toContain("사본 저장 완료"));
  expect(document.body.textContent).toMatch(/원래.*충돌|충돌.*(유지|남)/);
  expect(fakes.completeChoice).not.toHaveBeenCalled();
  expect(fakes.choose).not.toHaveBeenCalled();
  expect(fakes.reopen).not.toHaveBeenCalled();
  expect(view.state.doc.toString()).toBe("local");
});

test("후속 작업의 ID를 표시하고 별도 승인한 경우에만 해당 ID의 처리를 요청한다", async () => {
  const blocked = status("not_queued", "not_queued");
  blocked.copy.followup_operation_id = blockedOperation;
  fakes.resolution.mockResolvedValue(pendingResolution);
  fakes.copyStatus.mockResolvedValue(blocked);
  await showConflictButton();
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-review-open"]')!.click();
  await vi.waitFor(() => expect(document.querySelector('[data-testid="conflict-copy-progress"]')).not.toBeNull());
  fakes.copyProgress.mockResolvedValue({ status: "pending", requested_operation_id: blockedOperation });
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-copy-progress"]')!.click();
  await vi.waitFor(() => expect(document.body.textContent).toContain(blockedOperation));
  expect(fakes.copyProgress).not.toHaveBeenCalled();
  expect(fakes.resumeContent).not.toHaveBeenCalled();
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-copy-approve"]')!.click();
  await vi.waitFor(() => expect(fakes.copyProgress).toHaveBeenCalledExactlyOnceWith(binding, nodeId, "open-session", operationId, "0", blockedOperation));
  await vi.waitFor(() => expect(fakes.resumeContent).toHaveBeenCalledWith(JSON.stringify(binding), blockedOperation, expect.any(Boolean)));
  expect(fakes.completeChoice).not.toHaveBeenCalled();
  expect(document.body.textContent).not.toContain("사본 저장 완료");
});

test("후속 작업 승인 전 취소하면 abandon을 요청하거나 전송 차단을 해제하지 않는다", async () => {
  const blocked = status("not_queued", "not_queued");
  blocked.copy.followup_operation_id = blockedOperation;
  fakes.resolution.mockResolvedValue(pendingResolution);
  fakes.copyStatus.mockResolvedValue(blocked);
  await showConflictButton();
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-review-open"]')!.click();
  await vi.waitFor(() => expect(document.querySelector('[data-testid="conflict-copy-progress"]')).not.toBeNull());
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-copy-progress"]')!.click();
  await vi.waitFor(() => expect(document.querySelector('[data-testid="conflict-copy-approval-cancel"]')).not.toBeNull());
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-copy-approval-cancel"]')!.click();
  expect(fakes.copyProgress).not.toHaveBeenCalled();
  expect(fakes.resumeContent).not.toHaveBeenCalled();
});

test("승인 화면을 표시한 후 대상 작업 ID가 바뀌면 이전 동의로 진행하지 않는다", async () => {
  const blocked = status("not_queued", "not_queued");
  blocked.copy.followup_operation_id = blockedOperation;
  fakes.resolution.mockResolvedValue(pendingResolution);
  fakes.copyStatus.mockResolvedValue(blocked);
  await showConflictButton();
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-review-open"]')!.click();
  await vi.waitFor(() => expect(document.querySelector('[data-testid="conflict-copy-progress"]')).not.toBeNull());
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-copy-progress"]')!.click();
  await vi.waitFor(() => expect(document.querySelector('[data-testid="conflict-copy-approve"]')).not.toBeNull());
  const oldApproval = document.querySelector<HTMLButtonElement>('[data-testid="conflict-copy-approve"]')!;
  const changed = status("not_queued", "not_queued");
  changed.copy.followup_operation_id = "019a1000-0000-7000-8000-000000000007";
  fakes.copyStatus.mockResolvedValue(changed);
  await refreshCopy();
  await vi.waitFor(() => expect(document.body.textContent).toContain(changed.copy.followup_operation_id));
  oldApproval.click();
  await new Promise((resolve) => setTimeout(resolve, 0));
  expect(fakes.copyProgress).not.toHaveBeenCalled();
  expect(fakes.resumeContent).not.toHaveBeenCalled();
});

test("진행 중 새 후속 작업이 발견되면 ID를 표시하고 별도 승인을 기다린다", async () => {
  await restoreCopy();
  fakes.copyProgress.mockResolvedValue({ status: "approval_required", followup_operation_id: blockedOperation });
  await progressCopy();
  expect(fakes.copyProgress).toHaveBeenCalledExactlyOnceWith(binding, nodeId, "open-session", operationId, "0", null);
  await vi.waitFor(() => expect(document.body.textContent).toContain(blockedOperation));
  expect(fakes.resumeContent).not.toHaveBeenCalled();
  expect(fakes.copyProgress).toHaveBeenCalledTimes(1);
  expect(document.body.textContent).not.toContain("사본 저장 완료");
});

test("승인한 ID와 다른 요청 결과로 다른 작업의 차단을 해제하지 않는다", async () => {
  const blocked = status("not_queued", "not_queued");
  blocked.copy.followup_operation_id = blockedOperation;
  fakes.resolution.mockResolvedValue(pendingResolution);
  fakes.copyStatus.mockResolvedValue(blocked);
  await showConflictButton();
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-review-open"]')!.click();
  await vi.waitFor(() => expect(document.querySelector('[data-testid="conflict-copy-progress"]')).not.toBeNull());
  fakes.copyProgress.mockResolvedValue({ status: "pending", requested_operation_id: "019a1000-0000-7000-8000-000000000007" });
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-copy-progress"]')!.click();
  await vi.waitFor(() => expect(document.querySelector('[data-testid="conflict-copy-approve"]')).not.toBeNull());
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-copy-approve"]')!.click();
  await vi.waitFor(() => expect(fakes.copyProgress).toHaveBeenCalled());
  await new Promise((resolve) => setTimeout(resolve, 0));
  expect(fakes.resumeContent).not.toHaveBeenCalled();
  expect(document.body.textContent).not.toContain("사본 저장 완료");
});

test("진행 결과가 불명이면 완료를 추정하지 않고 같은 사본 상태를 다시 확인한다", async () => {
  const view = await restoreCopy();
  fakes.copyProgress.mockRejectedValue(new Error("response lost"));
  await progressCopy();
  await vi.waitFor(() => expect(document.body.textContent).toContain("사본 처리 결과를 확인하지 못했습니다"));
  await vi.waitFor(() => expect(document.querySelector<HTMLButtonElement>('[data-testid="conflict-copy-status-refresh"]')?.disabled).toBe(false));
  expect(document.body.textContent).not.toContain("사본 저장 완료");
  expect(fakes.choose).not.toHaveBeenCalled();
  fakes.copyStatus.mockResolvedValue(status("pending", "not_queued"));
  await refreshCopy();
  expect(fakes.copyStatus).toHaveBeenLastCalledWith(binding, nodeId, operationId);
  expect(fakes.copyProgress).toHaveBeenCalledTimes(1);
  expect(view.state.doc.toString()).toBe("local");
});

test("사본 상태에 재검토가 필요하면 진행과 완료를 표시하지 않고 원문을 유지한다", async () => {
  const view = await restoreCopy("needs_review", "not_queued");
  await vi.waitFor(() => expect(document.body.textContent).toMatch(/다시 확인|재검토/));
  expect(document.body.textContent).not.toContain("사본 저장 완료");
  expect(fakes.copyProgress).not.toHaveBeenCalled();
  expect(fakes.choose).not.toHaveBeenCalled();
  expect(view.state.doc.toString()).toBe("local");
});

test("동결된 이전 선택은 새로운 등록에서 자동으로 진행하지 않는다", async () => {
  fakes.resolution.mockResolvedValue({ ...pendingResolution, state: { ...pendingResolution.state, frozen: true } });
  const view = await showConflictButton();
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-review-open"]')!.click();
  await vi.waitFor(() => expect(fakes.resolution).toHaveBeenCalled());
  await vi.waitFor(() => expect(document.body.textContent).toMatch(/동결|이전 선택|대조/));
  expect(document.querySelector('[data-testid="conflict-copy-progress"]')).toBeNull();
  expect(fakes.copyProgress).not.toHaveBeenCalled();
  expect(fakes.choose).not.toHaveBeenCalled();
  expect(view.state.doc.toString()).toBe("local");
});

test("다른 문서나 등록의 사본 상태 응답은 현재 사본에 표시하거나 진행하지 않는다", async () => {
  fakes.resolution.mockResolvedValue(pendingResolution);
  fakes.copyStatus.mockResolvedValue({ ...status("created", "saved"), document_id: copyNode });
  const view = await showConflictButton();
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-review-open"]')!.click();
  await vi.waitFor(() => expect(fakes.copyStatus).toHaveBeenCalled());
  await vi.waitFor(() => expect(document.body.textContent).toMatch(/확인|오류/));
  expect(document.body.textContent).not.toContain("사본 저장 완료");
  expect(document.querySelector('[data-testid="conflict-copy-progress"]')).toBeNull();
  expect(fakes.copyProgress).not.toHaveBeenCalled();
  expect(view.state.doc.toString()).toBe("local");
});

test("진행 응답을 기다리는 동안 binding이 바뀌면 다른 등록의 전송을 재개하지 않는다", async () => {
  await restoreCopy();
  let release!: () => void;
  fakes.copyProgress.mockImplementation(() => new Promise((resolve) => { release = () => resolve({ status: "pending", requested_operation_id: null }); }));
  await progressCopy();
  fakes.active = { ...binding, workspace_epoch: "new-epoch" };
  fakes.listeners.get("workspace-sync-status-changed")?.();
  await vi.waitFor(() => expect(document.querySelector('[data-testid="conflict-copy-progress"]')).toBeNull());
  fakes.resumeContent.mockClear(); fakes.kick.mockClear();
  release();
  await new Promise((resolve) => setTimeout(resolve, 0));
  expect(fakes.resumeContent).not.toHaveBeenCalled();
  expect(fakes.kick).not.toHaveBeenCalled();
  expect(document.body.textContent).not.toContain("사본 저장 완료");
});
