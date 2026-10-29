// @vitest-environment jsdom
import { EditorView } from "@codemirror/view";
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import type { WorkspaceRegistration } from "../../../packages/protocol-types";

const fakes = vi.hoisted(() => ({
  listeners: new Map<string, () => void>(),
  root: undefined as { unmount(): void } | undefined,
  copy: vi.fn(), folders: vi.fn(), choose: vi.fn(), resolution: vi.fn(), reopen: vi.fn(), continueChoice: vi.fn(), completeChoice: vi.fn(),
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
    confirmWorkspaceContentConflictCopy: fakes.copy, workspaceCreateParents: fakes.folders,
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
  fakes.copy.mockReset().mockResolvedValue({ status: "pending", version: "0", node_id: "019a1000-0000-7000-8000-000000000005", create_operation_id: "019a1000-0000-7000-8000-000000000006" });
  fakes.folders.mockReset().mockResolvedValue({ nodes: [rootNode], next_after_id: null });
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

async function showComparison(): Promise<EditorView> {
  const view = await showConflictButton();
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-review-open"]')!.click();
  await vi.waitFor(() => expect(document.querySelector<HTMLTextAreaElement>('[aria-label="이 기기에 저장된 내 초안"]')?.value).toBe("local"));
  return view;
}

async function beginCopy() {
  await vi.waitFor(() => expect(document.querySelector('[data-testid="conflict-copy-open"]')).not.toBeNull());
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-copy-open"]')!.click();
  await vi.waitFor(() => expect(document.querySelector<HTMLSelectElement>('[data-testid="conflict-copy-parent"]')?.options.length).toBeGreaterThan(1));
}

function setValue(element: HTMLInputElement | HTMLSelectElement, value: string) {
  const prototype = element instanceof HTMLInputElement ? HTMLInputElement.prototype : HTMLSelectElement.prototype;
  Object.getOwnPropertyDescriptor(prototype, "value")!.set!.call(element, value);
  element.dispatchEvent(new Event(element instanceof HTMLInputElement ? "input" : "change", { bubbles: true }));
}

async function selectDestination(name = "copy.md") {
  setValue(document.querySelector<HTMLSelectElement>('[data-testid="conflict-copy-parent"]')!, rootId);
  setValue(document.querySelector<HTMLInputElement>('[data-testid="conflict-copy-name"]')!, name);
  await vi.waitFor(() => expect(document.querySelector<HTMLButtonElement>('[data-testid="conflict-copy-review"]')?.disabled).toBe(false));
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-copy-review"]')!.click();
  await vi.waitFor(() => expect(document.querySelector('[data-testid="conflict-copy-confirm"]')).not.toBeNull());
}

test("비교한 초안과 대상 폴더·이름을 확인한 뒤에만 사본 보존을 요청한다", async () => {
  const view = await showComparison();
  expect(fakes.copy).not.toHaveBeenCalled();
  await beginCopy();
  expect(fakes.folders).toHaveBeenCalledWith(binding, null);
  expect(fakes.copy).not.toHaveBeenCalled();
  await selectDestination();
  expect(fakes.copy).not.toHaveBeenCalled();
  expect(document.body.textContent).toContain("copy.md");
  expect(document.querySelector<HTMLTextAreaElement>('[aria-label="이 기기에 저장된 내 초안"]')?.value).toBe("local");
  expect(document.body.textContent).toMatch(/충돌.*(남|유지|별도)|원래.*충돌/);
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-copy-confirm"]')!.click();
  await vi.waitFor(() => expect(fakes.copy).toHaveBeenCalledExactlyOnceWith(snapshot, "open-session", null, rootId, "copy.md"));
  await vi.waitFor(() => expect(document.body.textContent).toMatch(/대기|보류|확인 중/));
  expect(document.body.textContent).not.toMatch(/사본 저장 완료/);
  expect(fakes.choose).not.toHaveBeenCalled();
  expect(fakes.continueChoice).not.toHaveBeenCalled();
  expect(fakes.completeChoice).not.toHaveBeenCalled();
  expect(view.state.doc.toString()).toBe("local");
});

test("대상 선택을 취소하면 확인과 서버 작업을 요청하지 않는다", async () => {
  const view = await showComparison();
  await beginCopy();
  await selectDestination();
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-copy-cancel"]')!.click();
  await vi.waitFor(() => expect(document.querySelector('[data-testid="conflict-copy-confirm"]')).toBeNull());
  expect(fakes.copy).not.toHaveBeenCalled();
  expect(fakes.choose).not.toHaveBeenCalled();
  expect(view.state.doc.toString()).toBe("local");
});

test("폴더가 없거나 Markdown 이름이 아니면 확인할 수 없다", async () => {
  await showComparison();
  await beginCopy();
  const review = document.querySelector<HTMLButtonElement>('[data-testid="conflict-copy-review"]')!;
  expect(review.disabled).toBe(true);
  setValue(document.querySelector<HTMLSelectElement>('[data-testid="conflict-copy-parent"]')!, rootId);
  setValue(document.querySelector<HTMLInputElement>('[data-testid="conflict-copy-name"]')!, "copy.txt");
  review.click();
  await vi.waitFor(() => expect(document.querySelector('[data-testid="conflict-copy-confirm"]')).toBeNull());
  expect(fakes.copy).not.toHaveBeenCalled();
});

test("확인 중 새 입력이 생기면 이전에 표시한 초안의 사본을 요청하지 않는다", async () => {
  const view = await showComparison();
  await beginCopy();
  await selectDestination();
  const confirm = document.querySelector<HTMLButtonElement>('[data-testid="conflict-copy-confirm"]')!;
  view.dispatch({ changes: { from: view.state.doc.length, insert: " newer" }, userEvent: "input.type" });
  confirm.click();
  await new Promise((resolve) => setTimeout(resolve, 0));
  expect(fakes.copy).not.toHaveBeenCalled();
  expect(view.state.doc.toString()).toBe("local newer");
});

test("확인 직전 한글 조합이 시작되면 조합을 유지하고 사본을 요청하지 않는다", async () => {
  const view = await showComparison();
  await beginCopy();
  await selectDestination();
  view.contentDOM.dispatchEvent(new CompositionEvent("compositionstart", { bubbles: true }));
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-copy-confirm"]')!.click();
  await vi.waitFor(() => expect(document.body.textContent).toMatch(/조합/));
  expect(fakes.copy).not.toHaveBeenCalled();
  expect(view.state.doc.toString()).toBe("local");
  view.contentDOM.dispatchEvent(new CompositionEvent("compositionend", { bubbles: true }));
});

test("등록 binding이 교체되면 이전 사본 확인을 폐기한다", async () => {
  await showComparison();
  await beginCopy();
  await selectDestination();
  const confirm = document.querySelector<HTMLButtonElement>('[data-testid="conflict-copy-confirm"]')!;
  fakes.active = { ...binding, workspace_epoch: "new-epoch" };
  fakes.listeners.get("workspace-sync-status-changed")?.();
  await vi.waitFor(() => expect(document.querySelector('[aria-label="충돌 원문 비교"]')).toBeNull());
  confirm.click();
  expect(fakes.copy).not.toHaveBeenCalled();
  expect(fakes.choose).not.toHaveBeenCalled();
});

test("확인 요청이 진행 중이면 중복 클릭으로 새 선택을 만들지 않는다", async () => {
  let release!: () => void;
  fakes.copy.mockImplementation(() => new Promise((resolve) => {
    release = () => resolve({ status: "pending", version: "0", node_id: "019a1000-0000-7000-8000-000000000005", create_operation_id: "019a1000-0000-7000-8000-000000000006" });
  }));
  await showComparison();
  await beginCopy();
  await selectDestination();
  const confirm = document.querySelector<HTMLButtonElement>('[data-testid="conflict-copy-confirm"]')!;
  confirm.click();
  confirm.click();
  await vi.waitFor(() => expect(fakes.copy).toHaveBeenCalledTimes(1));
  expect(fakes.choose).not.toHaveBeenCalled();
  release();
});
