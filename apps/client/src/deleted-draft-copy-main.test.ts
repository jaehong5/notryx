// @vitest-environment jsdom
import { EditorView } from "@codemirror/view";
import { EditorState } from "@codemirror/state";
import type { WorkspaceDraft, WorkspaceRegistration } from "../../../packages/protocol-types";
import { afterEach, beforeEach, expect, test, vi } from "vitest";

const fakes = vi.hoisted(() => ({
  listeners: new Map<string, () => void>(),
  root: undefined as { unmount(): void } | undefined,
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
const pendingCopy = { binding, document_id:nodeId, generation:"1", local_hash:snapshot.local_hash, tombstone_seq:"3", terminal_operation_id:null, parent_id:rootId, name:"recovered.md", node_id:copyNode, create_operation_id:createOperation, create_status:"not_queued", create_failure:null, content_status:"not_queued", content_operation_id:null, blocking_followup:null };
const rangeRects = Object.getOwnPropertyDescriptor(Range.prototype, "getClientRects");
const rangeBounds = Object.getOwnPropertyDescriptor(Range.prototype, "getBoundingClientRect");

beforeEach(() => {
  Object.defineProperty(Range.prototype, "getClientRects", { configurable: true, value: () => [] });
  Object.defineProperty(Range.prototype, "getBoundingClientRect", { configurable: true, value: () => ({ left: 0, right: 0, top: 0, bottom: 0, width: 0, height: 0 }) });
  vi.resetModules();
  fakes.deleted = false; fakes.active = binding; fakes.drafts = [];
  fakes.generation = "1";
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


async function deletedEditor(): Promise<EditorView> {
  const view = await openEditor(false);
  fakes.deleted = true; fakes.inspect.mockResolvedValue("deleted");
  fakes.listeners.get("workspace-metadata-changed")?.();
  await vi.waitFor(() => expect(document.body.textContent).toMatch(/다른 기기.*삭제/));
  return view;
}

function setValue(element: HTMLInputElement | HTMLSelectElement, value: string) {
  const prototype = element instanceof HTMLInputElement ? HTMLInputElement.prototype : HTMLSelectElement.prototype;
  Object.getOwnPropertyDescriptor(prototype, "value")!.set!.call(element, value);
  element.dispatchEvent(new Event(element instanceof HTMLInputElement ? "input" : "change", { bubbles: true }));
}

async function openCopyReview() {
  await vi.waitFor(() => expect(document.querySelector('[data-testid="deleted-copy-open"]')).not.toBeNull());
  document.querySelector<HTMLButtonElement>('[data-testid="deleted-copy-open"]')!.click();
  await vi.waitFor(() => expect(document.querySelector<HTMLTextAreaElement>('[aria-label="보존할 기기 초안"]')?.value).toBe("base"));
  await vi.waitFor(() => expect(document.querySelector<HTMLSelectElement>('[data-testid="deleted-copy-parent"]')?.options.length).toBeGreaterThan(1));
}

async function selectDestination() {
  setValue(document.querySelector<HTMLSelectElement>('[data-testid="deleted-copy-parent"]')!, rootId);
  setValue(document.querySelector<HTMLInputElement>('[data-testid="deleted-copy-name"]')!, "recovered.md");
  await vi.waitFor(() => expect(document.querySelector<HTMLButtonElement>('[data-testid="deleted-copy-review"]')?.disabled).toBe(false));
  document.querySelector<HTMLButtonElement>('[data-testid="deleted-copy-review"]')!.click();
  await vi.waitFor(() => expect(document.querySelector('[data-testid="deleted-copy-confirm"]')).not.toBeNull());
}

test("현재 삭제와 초안·목적지를 확인한 뒤에만 고정된 snapshot의 사본을 요청한다", async () => {
  const view = await deletedEditor();
  expect(fakes.confirm).not.toHaveBeenCalled();
  await openCopyReview();
  expect(fakes.snapshot).toHaveBeenCalledWith(binding, nodeId, "open-session", "1");
  expect(fakes.confirm).not.toHaveBeenCalled();
  await selectDestination();
  expect(document.body.textContent).toContain("recovered.md");
  expect(fakes.confirm).not.toHaveBeenCalled();
  document.querySelector<HTMLButtonElement>('[data-testid="deleted-copy-confirm"]')!.click();
  await vi.waitFor(() => expect(fakes.confirm).toHaveBeenCalledExactlyOnceWith(snapshot, rootId, "recovered.md"));
  await vi.waitFor(() => expect(document.body.textContent).toMatch(/사본.*(대기|보류)/));
  expect(document.body.textContent).not.toContain("사본 저장 완료");
  expect(fakes.copyProgress).not.toHaveBeenCalled();
  expect(view.state.doc.toString()).toBe("base");
});

test("대상 확인을 취소하면 새 사본이나 서버 변경을 요청하지 않는다", async () => {
  const view = await deletedEditor();
  await openCopyReview(); await selectDestination();
  document.querySelector<HTMLButtonElement>('[data-testid="deleted-copy-cancel"]')!.click();
  await vi.waitFor(() => expect(document.querySelector('[data-testid="deleted-copy-confirm"]')).toBeNull());
  expect(fakes.confirm).not.toHaveBeenCalled(); expect(fakes.copyProgress).not.toHaveBeenCalled();
  expect(view.state.doc.toString()).toBe("base");
});

test("과거 삭제 안내가 있어도 최신 삭제 확인에 실패하면 새 선택을 만들지 않는다", async () => {
  const view = await deletedEditor();
  fakes.snapshot.mockRejectedValue(new Error("offline"));
  await vi.waitFor(() => expect(document.querySelector('[data-testid="deleted-copy-open"]')).not.toBeNull());
  document.querySelector<HTMLButtonElement>('[data-testid="deleted-copy-open"]')!.click();
  await vi.waitFor(() => expect(fakes.snapshot).toHaveBeenCalled());
  await vi.waitFor(() => expect(document.body.textContent).toMatch(/삭제.*확인|연결.*확인/));
  expect(document.querySelector('[data-testid="deleted-copy-confirm"]')).toBeNull();
  expect(fakes.confirm).not.toHaveBeenCalled(); expect(view.state.doc.toString()).toBe("base");
});

test("표시할 편집 bytes와 native snapshot hash가 다르면 확인을 허용하지 않는다", async () => {
  const view = await deletedEditor();
  fakes.snapshot.mockResolvedValue({ ...snapshot, local_hash:"sha256:" + "f".repeat(64) });
  await vi.waitFor(() => expect(document.querySelector('[data-testid="deleted-copy-open"]')).not.toBeNull());
  document.querySelector<HTMLButtonElement>('[data-testid="deleted-copy-open"]')!.click();
  await vi.waitFor(() => expect(fakes.snapshot).toHaveBeenCalled());
  await vi.waitFor(() => expect(document.body.textContent).toMatch(/초안.*확인|원문.*변경/));
  expect(document.querySelector('[data-testid="deleted-copy-confirm"]')).toBeNull();
  expect(fakes.confirm).not.toHaveBeenCalled();
  expect(view.state.doc.toString()).toBe("base");
});

test("다른 등록의 삭제 snapshot은 표시하거나 확인하지 않는다", async () => {
  const view = await deletedEditor();
  fakes.snapshot.mockResolvedValue({ ...snapshot, binding:{ ...binding, workspace_epoch:"other" } });
  await vi.waitFor(() => expect(document.querySelector('[data-testid="deleted-copy-open"]')).not.toBeNull());
  document.querySelector<HTMLButtonElement>('[data-testid="deleted-copy-open"]')!.click();
  await vi.waitFor(() => expect(fakes.snapshot).toHaveBeenCalled());
  await vi.waitFor(() => expect(document.body.textContent).toMatch(/초안.*확인|원문.*변경/));
  expect(document.querySelector('[data-testid="deleted-copy-confirm"]')).toBeNull();
  expect(fakes.confirm).not.toHaveBeenCalled();
  expect(view.state.doc.toString()).toBe("base");
});

test("미저장 입력은 checkpoint 완료 전 삭제 snapshot을 요청하지 않는다", async () => {
  const view = await deletedEditor();
  view.dispatch({ changes:{ from:view.state.doc.length, insert:" newer" }, userEvent:"input.type" });
  let release!: () => void;
  fakes.save.mockImplementation((request:{ edit_session_id:string; generation:string }) => new Promise((resolve) => {
    release = () => resolve({ edit_session_id:request.edit_session_id, generation:request.generation });
  }));
  await vi.waitFor(() => expect(document.querySelector('[data-testid="deleted-copy-open"]')).not.toBeNull());
  document.querySelector<HTMLButtonElement>('[data-testid="deleted-copy-open"]')!.click();
  await vi.waitFor(() => expect(fakes.save).toHaveBeenCalled());
  expect(fakes.snapshot).not.toHaveBeenCalled();
  release();
  await vi.waitFor(() => expect(fakes.snapshot).toHaveBeenCalledWith(binding, nodeId, "open-session", "2"));
  expect(view.state.doc.toString()).toBe("base newer");
});

test("확인 직전 새 입력이 생기면 이전 초안 snapshot을 보내지 않는다", async () => {
  const view = await deletedEditor(); await openCopyReview(); await selectDestination();
  const confirm = document.querySelector<HTMLButtonElement>('[data-testid="deleted-copy-confirm"]')!;
  view.dispatch({ changes:{ from:view.state.doc.length, insert:" newer" }, userEvent:"input.type" });
  confirm.click();
  await new Promise((resolve) => setTimeout(resolve, 0));
  expect(fakes.confirm).not.toHaveBeenCalled(); expect(view.state.doc.toString()).toBe("base newer");
});

test("확인 직전 IME 조합 중에는 요청하지 않고 조합과 초안을 유지한다", async () => {
  const view = await deletedEditor(); await openCopyReview(); await selectDestination();
  view.contentDOM.dispatchEvent(new CompositionEvent("compositionstart", { bubbles:true }));
  document.querySelector<HTMLButtonElement>('[data-testid="deleted-copy-confirm"]')!.click();
  await vi.waitFor(() => expect(document.body.textContent).toMatch(/조합/));
  expect(fakes.confirm).not.toHaveBeenCalled(); expect(view.state.doc.toString()).toBe("base");
  view.contentDOM.dispatchEvent(new CompositionEvent("compositionend", { bubbles:true }));
});

test("확인 요청이 진행 중인 동안 현재 편집을 잠그고 중복 요청하지 않는다", async () => {
  const view = await deletedEditor(); await openCopyReview(); await selectDestination();
  let release!: () => void;
  fakes.confirm.mockImplementation(() => new Promise((resolve) => { release = () => resolve({ status:"pending", node_id:copyNode, create_operation_id:createOperation }); }));
  const confirm = document.querySelector<HTMLButtonElement>('[data-testid="deleted-copy-confirm"]')!;
  confirm.click(); confirm.click();
  await vi.waitFor(() => expect(fakes.confirm).toHaveBeenCalledOnce());
  expect(view.state.facet(EditorState.readOnly)).toBe(true);
  release();
});

test("등록이 바뀌면 이전 기기의 확인 화면과 요청을 폐기한다", async () => {
  await deletedEditor(); await openCopyReview(); await selectDestination();
  const confirm = document.querySelector<HTMLButtonElement>('[data-testid="deleted-copy-confirm"]')!;
  fakes.active = { ...binding, workspace_epoch:"new-epoch" };
  fakes.listeners.get("workspace-sync-status-changed")?.();
  await vi.waitFor(() => expect(document.querySelector('[data-testid="deleted-copy-confirm"]')).toBeNull());
  confirm.click(); expect(fakes.confirm).not.toHaveBeenCalled();
});

test("확인 결과가 불명이면 상태 조회만으로 편집을 재개하지 않고 명시적으로 재열기한다", async () => {
  const oldView = await deletedEditor(); await openCopyReview(); await selectDestination();
  fakes.confirm.mockRejectedValue(new Error("response lost"));
  document.querySelector<HTMLButtonElement>('[data-testid="deleted-copy-confirm"]')!.click();
  await vi.waitFor(() => expect(document.querySelector('[data-testid="deleted-copy-recover"]')).not.toBeNull());
  expect(oldView.state.facet(EditorState.readOnly)).toBe(true);
  expect(fakes.copyProgress).not.toHaveBeenCalled();
  expect(document.querySelector<HTMLButtonElement>('[data-testid="deleted-copy-recover"]')?.textContent).toMatch(/다시 열|재열기/);
  fakes.copyStatus.mockResolvedValue(pendingCopy);
  fakes.reopenCopy.mockResolvedValue({ state:pendingCopy, document:{ binding, document_id:nodeId, edit_session_id:"reopened-session", generation:"1", bytes_base64:btoa("base"), readonly:false, local_draft:true, has_session:true, cache_warning:null, open_notice:null } });
  document.querySelector<HTMLButtonElement>('[data-testid="deleted-copy-recover"]')!.click();
  await vi.waitFor(() => expect(fakes.copyStatus).toHaveBeenCalledWith(binding, nodeId));
  await vi.waitFor(() => expect(fakes.reopenCopy).toHaveBeenCalledWith(binding, nodeId, "open-session"));
  await vi.waitFor(() => expect(EditorView.findFromDOM(document.querySelector(".cm-editor")!)?.state.doc.toString()).toBe("base"));
  expect(fakes.copyProgress).not.toHaveBeenCalled();
});

test("재시작 후 삭제된 기기 초안을 열면 기존 사본 선택을 복원하고 자동 재발급하지 않는다", async () => {
  fakes.deleted = true; fakes.inspect.mockResolvedValue("deleted");
  fakes.drafts = [{ binding, document_id:nodeId, generation:"1", current_registration:true, has_session:true }];
  fakes.copyStatus.mockResolvedValue(pendingCopy);
  document.body.innerHTML = '<div id="root"></div>';
  await import("./main");
  await vi.waitFor(() => expect(document.querySelector(`[data-draft-document="${nodeId}"]`)).not.toBeNull());
  document.querySelector<HTMLButtonElement>(`[data-draft-document="${nodeId}"]`)!.click();
  await vi.waitFor(() => expect(fakes.openDraft).toHaveBeenCalled());
  await vi.waitFor(() => expect(fakes.copyStatus).toHaveBeenCalledWith(binding, nodeId));
  await vi.waitFor(() => expect(document.body.textContent).toContain("recovered.md"));
  expect(document.body.textContent).not.toContain("사본 저장 완료");
  expect(fakes.confirm).not.toHaveBeenCalled(); expect(fakes.copyProgress).not.toHaveBeenCalled();
});
