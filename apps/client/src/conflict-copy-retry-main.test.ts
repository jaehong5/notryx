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
  copyRetry: vi.fn(), folders: vi.fn(), createStatus: vi.fn(), copyStatus: vi.fn(), copyProgress: vi.fn(), choose: vi.fn(), resolution: vi.fn(), reopen: vi.fn(), continueChoice: vi.fn(), completeChoice: vi.fn(),
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
    retryWorkspaceContentConflictCopy: fakes.copyRetry, workspaceCreateParents: fakes.folders, workspaceCreateStatus: fakes.createStatus,
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
  fakes.copyStatus.mockReset(); fakes.copyProgress.mockReset(); fakes.copyRetry.mockReset();
  fakes.folders.mockReset().mockResolvedValue({ nodes: [rootNode], next_after_id: null });
  fakes.createStatus.mockReset().mockResolvedValue({ entries: [{ operation_id: "019a1000-0000-7000-8000-000000000006", operation_seq: "2", node_id: "019a1000-0000-7000-8000-000000000005", parent_id: rootId, name: "copy.md", kind: "file", status: "failed", reason: "destination_exists" }], next_after_seq: null });
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
const pendingResolution = { binding, document_id: nodeId, operation_id: operationId,
  state: { status: "pending", version: "0", generation: "1", choice: "keep_local_copy", needs_review: false, frozen: false } };
const status = (create_status: string, content_status: string) => ({ binding, document_id: nodeId, operation_id: operationId,
  copy: { version: "0", parent_id: rootId, name: "copy.md", node_id: copyNode, create_operation_id: createOperation, create_status, content_status, followup_operation_id: null as string | null,
    create_failure: create_status === "needs_review" ? "destination_exists" : null as string | null } });

async function restoreCopy(createStatus = "not_queued", contentStatus = "not_queued"): Promise<EditorView> {
  fakes.resolution.mockResolvedValue(pendingResolution);
  fakes.copyStatus.mockResolvedValue(status(createStatus, contentStatus));
  const view = await showConflictButton();
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-review-open"]')!.click();
  await vi.waitFor(() => expect(fakes.resolution).toHaveBeenCalledWith(binding, nodeId, operationId));
  await vi.waitFor(() => expect(fakes.copyStatus).toHaveBeenCalledWith(binding, nodeId, operationId));
  return view;
}

async function refreshCopy() {
  await vi.waitFor(() => expect(document.querySelector('[data-testid="conflict-copy-status-refresh"]')).not.toBeNull());
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-copy-status-refresh"]')!.click();
  await vi.waitFor(() => expect(fakes.copyStatus.mock.calls.length).toBeGreaterThan(1));
}

const newCopyNode = "019a1000-0000-7000-8000-000000000007";
const newCreateOperation = "019a1000-0000-7000-8000-000000000008";

function changedCopyStatus() {
  const result = status("pending", "not_queued");
  result.copy.name = "copy-retry.md";
  result.copy.node_id = newCopyNode;
  result.copy.create_operation_id = newCreateOperation;
  return result;
}

function setValue(element: HTMLInputElement | HTMLSelectElement, value: string) {
  const prototype = element instanceof HTMLInputElement ? HTMLInputElement.prototype : HTMLSelectElement.prototype;
  Object.getOwnPropertyDescriptor(prototype, "value")!.set!.call(element, value);
  element.dispatchEvent(new Event(element instanceof HTMLInputElement ? "input" : "change", { bubbles: true }));
}

async function reviewNewDestination() {
  await vi.waitFor(() => expect(document.querySelector('[data-testid="conflict-copy-retry"]')).not.toBeNull());
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-copy-retry"]')!.click();
  await vi.waitFor(() => expect(document.querySelector<HTMLSelectElement>('[data-testid="conflict-copy-retry-parent"]')?.options.length).toBeGreaterThan(1));
  setValue(document.querySelector<HTMLSelectElement>('[data-testid="conflict-copy-retry-parent"]')!, rootId);
  setValue(document.querySelector<HTMLInputElement>('[data-testid="conflict-copy-retry-name"]')!, "copy-retry.md");
  await vi.waitFor(() => expect(document.querySelector<HTMLButtonElement>('[data-testid="conflict-copy-retry-review"]')?.disabled).toBe(false));
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-copy-retry-review"]')!.click();
  await vi.waitFor(() => expect(document.querySelector('[data-testid="conflict-copy-retry-confirm"]')).not.toBeNull());
}

test("생성 실패 ID와 새 대상을 확인한 뒤에만 다른 사본 재승인을 기록한다", async () => {
  const view = await restoreCopy("needs_review", "not_queued");
  await vi.waitFor(() => expect(document.body.textContent).toContain(createOperation));
  await reviewNewDestination();
  expect(document.body.textContent).toContain(createOperation);
  expect(document.body.textContent).toContain("copy-retry.md");
  expect(fakes.copyRetry).not.toHaveBeenCalled();
  expect(fakes.copyProgress).not.toHaveBeenCalled();
  fakes.copyRetry.mockResolvedValue({ status: "pending", version: "0", node_id: newCopyNode, create_operation_id: newCreateOperation });
  fakes.copyStatus.mockResolvedValue(changedCopyStatus());
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-copy-retry-confirm"]')!.click();
  await vi.waitFor(() => expect(fakes.copyRetry).toHaveBeenCalledExactlyOnceWith(binding, nodeId, "open-session", operationId, "0", createOperation, rootId, "copy-retry.md"));
  await vi.waitFor(() => expect(document.body.textContent).toContain("copy-retry.md"));
  expect(document.body.textContent).not.toContain("사본 저장 완료");
  expect(fakes.copyProgress).not.toHaveBeenCalled();
  expect(fakes.choose).not.toHaveBeenCalled();
  expect(view.state.doc.toString()).toBe("local");
  const openCreate = [...document.querySelectorAll<HTMLButtonElement>("button")].find((button) => button.textContent === "새 폴더 또는 문서");
  expect(openCreate).toBeDefined();
  openCreate!.click();
  await vi.waitFor(() => expect(document.body.textContent).toContain("목적지에 같은 이름이 있습니다."));
  expect(fakes.createStatus).toHaveBeenCalledWith(binding, "0");
});

test("새 대상 확인 취소는 기존 실패와 원문을 유지하고 재승인하지 않는다", async () => {
  const view = await restoreCopy("needs_review", "not_queued");
  await reviewNewDestination();
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-copy-retry-cancel"]')!.click();
  expect(fakes.copyRetry).not.toHaveBeenCalled();
  expect(fakes.copyProgress).not.toHaveBeenCalled();
  expect(document.body.textContent).toContain(createOperation);
  expect(view.state.doc.toString()).toBe("local");
});

test("재승인 응답이 유실되면 새 ID를 추정하지 않고 기존 충돌의 상태를 재조회한다", async () => {
  const view = await restoreCopy("needs_review", "not_queued");
  await reviewNewDestination();
  fakes.copyRetry.mockRejectedValue(new Error("response lost"));
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-copy-retry-confirm"]')!.click();
  await vi.waitFor(() => expect(fakes.copyRetry).toHaveBeenCalledOnce());
  await vi.waitFor(() => expect(document.body.textContent).toContain("사본 재승인 결과를 확인하지 못했습니다"));
  await vi.waitFor(() => expect(document.querySelector<HTMLButtonElement>('[data-testid="conflict-copy-status-refresh"]')?.disabled).toBe(false));
  fakes.copyStatus.mockResolvedValue(changedCopyStatus());
  await refreshCopy();
  await vi.waitFor(() => expect(document.body.textContent).toContain("copy-retry.md"));
  expect(fakes.copyRetry).toHaveBeenCalledTimes(1);
  expect(fakes.copyProgress).not.toHaveBeenCalled();
  expect(fakes.choose).not.toHaveBeenCalled();
  expect(view.state.doc.toString()).toBe("local");
});

test("재시작 후 새 사본 작업을 복원할 때 기존 실패를 새 요청으로 재발행하지 않는다", async () => {
  fakes.resolution.mockResolvedValue(pendingResolution);
  fakes.copyStatus.mockResolvedValue(changedCopyStatus());
  const view = await showConflictButton();
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-review-open"]')!.click();
  await vi.waitFor(() => expect(document.body.textContent).toContain("copy-retry.md"));
  expect(fakes.copyRetry).not.toHaveBeenCalled();
  expect(fakes.copyProgress).not.toHaveBeenCalled();
  expect(document.body.textContent).not.toContain("사본 저장 완료");
  expect(view.state.doc.toString()).toBe("local");
});

test("이름 충돌이 아닌 생성 실패는 새 대상 재승인을 제공하지 않고 보존을 안내한다", async () => {
  fakes.resolution.mockResolvedValue(pendingResolution);
  const failed = status("needs_review", "not_queued");
  failed.copy.create_failure = "other";
  fakes.copyStatus.mockResolvedValue(failed);
  const view = await showConflictButton();
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-review-open"]')!.click();
  await vi.waitFor(() => expect(fakes.copyStatus).toHaveBeenCalledWith(binding, nodeId, operationId));
  await vi.waitFor(() => expect(document.querySelector('[aria-label="사본 저장 상태"]')?.textContent).toMatch(/보존|확인/));
  expect(document.querySelector('[data-testid="conflict-copy-retry"]')).toBeNull();
  expect(fakes.copyRetry).not.toHaveBeenCalled();
  expect(fakes.copyProgress).not.toHaveBeenCalled();
  expect(view.state.doc.toString()).toBe("local");
});

test("재승인 확인 전 문서가 바뀌면 오래된 대상 확인을 전송하지 않는다", async () => {
  const view = await restoreCopy("needs_review", "not_queued");
  await reviewNewDestination();
  const confirm = document.querySelector<HTMLButtonElement>('[data-testid="conflict-copy-retry-confirm"]')!;
  view.dispatch({ changes: { from: view.state.doc.length, insert: " newer" }, userEvent: "input.type" });
  confirm.click();
  await new Promise((resolve) => setTimeout(resolve, 0));
  expect(fakes.copyRetry).not.toHaveBeenCalled();
  expect(view.state.doc.toString()).toBe("local newer");
});

test("새 작업 응답이 이전 사본 node ID를 재사용하면 다른 사본으로 표시하지 않는다", async () => {
  const view = await restoreCopy("needs_review", "not_queued");
  await reviewNewDestination();
  fakes.copyRetry.mockResolvedValue({ status: "pending", version: "0", node_id: copyNode, create_operation_id: newCreateOperation });
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-copy-retry-confirm"]')!.click();
  await vi.waitFor(() => expect(fakes.copyRetry).toHaveBeenCalledOnce());
  await vi.waitFor(() => expect(document.body.textContent).toMatch(/확인하지 못|다시 확인/));
  expect(fakes.copyProgress).not.toHaveBeenCalled();
  expect(document.body.textContent).not.toContain("사본 저장 완료");
  expect(view.state.doc.toString()).toBe("local");
});

test("재승인 확인 전 등록이 바뀌면 이전 세션의 요청을 전송하지 않는다", async () => {
  await restoreCopy("needs_review", "not_queued");
  await reviewNewDestination();
  const confirm = document.querySelector<HTMLButtonElement>('[data-testid="conflict-copy-retry-confirm"]')!;
  fakes.active = { ...binding, workspace_epoch: "new-epoch" };
  fakes.listeners.get("workspace-sync-status-changed")?.();
  await vi.waitFor(() => expect(document.querySelector('[data-testid="conflict-copy-retry-confirm"]')).toBeNull());
  confirm.click();
  expect(fakes.copyRetry).not.toHaveBeenCalled();
});

test("재승인 응답 대기 중 등록이 바뀌면 새 등록 화면에 이전 결과를 표시하지 않는다", async () => {
  await restoreCopy("needs_review", "not_queued");
  await reviewNewDestination();
  let release!: () => void;
  fakes.copyRetry.mockImplementation(() => new Promise((resolve) => {
    release = () => resolve({ status: "pending", version: "0", node_id: newCopyNode, create_operation_id: newCreateOperation });
  }));
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-copy-retry-confirm"]')!.click();
  await vi.waitFor(() => expect(fakes.copyRetry).toHaveBeenCalledOnce());
  fakes.active = { ...binding, workspace_epoch: "new-epoch" };
  fakes.listeners.get("workspace-sync-status-changed")?.();
  await vi.waitFor(() => expect(document.querySelector('[data-testid="conflict-copy-retry-confirm"]')).toBeNull());
  const requestsBefore = fakes.copyStatus.mock.calls.length;
  release();
  await new Promise((resolve) => setTimeout(resolve, 0));
  expect(fakes.copyStatus.mock.calls.length).toBe(requestsBefore);
  expect(document.body.textContent).not.toContain("copy-retry.md");
  expect(fakes.copyProgress).not.toHaveBeenCalled();
});
