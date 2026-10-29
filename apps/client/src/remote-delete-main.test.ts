// @vitest-environment jsdom
import { EditorView } from "@codemirror/view";
import { afterEach, beforeEach, expect, test, vi } from "vitest";

const fakes = vi.hoisted(() => ({
  listeners: new Map<string, () => void>(),
  root: undefined as { unmount(): void } | undefined,
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
    localDrafts: async () => [], pendingPairStatus: async () => "none",
    workspaceSyncState: async () => ({ binding, status: "ready" }),
    workspaceInventory: async () => ({ nodes: fakes.deleted ? [rootNode] : [rootNode, node], next_after_id: null }),
    openWorkspaceDocument: async () => ({ binding, document_id: nodeId, edit_session_id: "open-session", generation: fakes.generation, bytes_base64: btoa("base"), readonly: false, local_draft: false, has_session: true, cache_warning: null, open_notice: null }),
    workspaceDocumentOutbox: fakes.outbox,
    prepareWorkspaceRemoteRefresh: fakes.prepare, commitWorkspaceRemoteRefresh: fakes.commit,
    inspectWorkspaceRemoteChange: fakes.inspect, pairWorkspace: fakes.pair,
    saveWorkspaceDocument: fakes.save, queueWorkspaceDocument: fakes.queue,
    sendNextWorkspaceOperation: fakes.transmit,
    closeWorkspaceDocument: fakes.close, recoverWorkspaceSource: fakes.recover,
  };
});

const binding = { server_origin: "https://example.invalid", workspace_id: "workspace", workspace_epoch: "epoch", device_id: "device", device_stream_id: "stream" };
const rootId = "019a1000-0000-7000-8000-000000000001", nodeId = "019a1000-0000-7000-8000-000000000002";
const rootNode = { node_id: rootId, parent_id: null, name: "", kind: "folder" as const, deleted: false, content_rev: null, content_etag: null, meta_rev: "meta", meta_etag: '"meta"', node_etag: '"node"', content_hash: null, size: null, children_generation: "0", last_change_seq: "1" };
const node = { ...rootNode, node_id: nodeId, parent_id: rootId, name: "synthetic.md", kind: "file" as const, content_rev: "content", content_etag: '"content"', content_hash: "sha256:" + "a".repeat(64), size: "4" };
const rangeRects = Object.getOwnPropertyDescriptor(Range.prototype, "getClientRects");
const rangeBounds = Object.getOwnPropertyDescriptor(Range.prototype, "getBoundingClientRect");

beforeEach(() => {
  Object.defineProperty(Range.prototype, "getClientRects", { configurable: true, value: () => [] });
  Object.defineProperty(Range.prototype, "getBoundingClientRect", { configurable: true, value: () => ({ left: 0, right: 0, top: 0, bottom: 0, width: 0, height: 0 }) });
  vi.resetModules();
  fakes.deleted = false;
  fakes.generation = "0";
  fakes.outbox.mockReset().mockResolvedValue({ binding, document_id:nodeId, first_content_conflict:null, has_conflict:false, has_failure:false, latest:null, confirmed_through:"0" });
  fakes.close.mockReset().mockResolvedValue(undefined);
  fakes.recover.mockReset().mockResolvedValue(true);
  fakes.prepare.mockReset().mockResolvedValue(null);
  fakes.commit.mockReset().mockResolvedValue(true);
  fakes.inspect.mockReset().mockResolvedValue("same");
  fakes.save.mockReset().mockImplementation(async (request: { edit_session_id: string; generation: string }) => ({ edit_session_id: request.edit_session_id, generation: request.generation }));
  fakes.queue.mockReset();
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


test("원격 삭제된 수정 없는 열린 문서도 삭제를 표시하고 기존 세션과 원문을 유지한다", async () => {
  const view = await openEditor();
  fakes.deleted = true; fakes.inspect.mockResolvedValue("deleted");
  fakes.listeners.get("workspace-metadata-changed")?.();
  await vi.waitFor(() => expect(document.body.textContent).toMatch(/다른 기기.*삭제/));
  expect(view.state.doc.toString()).toBe("base");
  expect(document.querySelector('.cm-content[contenteditable=true]')).not.toBeNull();
  expect(fakes.close).not.toHaveBeenCalled();
  expect(fakes.save).not.toHaveBeenCalled();
  expect(document.querySelector('#copy-source')).not.toBeNull();
  expect(document.querySelector('#export-source')).not.toBeNull();
  expect(fakes.queue).not.toHaveBeenCalled();
  document.querySelector<HTMLButtonElement>('#copy-source')!.click();
  await vi.waitFor(() => expect(fakes.recover).toHaveBeenCalledOnce());
  const call = fakes.recover.mock.calls[0];
  expect(call[0]).toBe(nodeId); expect(call[1]).toBe("open-session");
  expect(new TextDecoder().decode(call[2])).toBe("base"); expect(call[3]).toBe("copy");
});

test("원격 삭제와 로컬 편집이 겹치면 초안과 세션을 유지하고 현재 원문을 내보낼 수 있다", async () => {
  const view = await openEditor();
  view.dispatch({ changes:{ from:view.state.doc.length, insert:" local draft" }, userEvent:"input.type" });
  fakes.deleted = true; fakes.inspect.mockResolvedValue("deleted");
  fakes.listeners.get("workspace-metadata-changed")?.();
  await vi.waitFor(() => expect(document.body.textContent).toMatch(/다른 기기.*삭제/));
  expect(view.state.doc.toString()).toBe("base local draft");
  expect(document.querySelector('.cm-content[contenteditable=true]')).not.toBeNull();
  expect(fakes.close).not.toHaveBeenCalled();
  expect(fakes.queue).not.toHaveBeenCalled();
  document.querySelector<HTMLButtonElement>('#export-source')!.click();
  await vi.waitFor(() => expect(fakes.recover).toHaveBeenCalledOnce());
  const call = fakes.recover.mock.calls[0];
  expect(call[0]).toBe(nodeId); expect(call[1]).toBe("open-session");
  expect(new TextDecoder().decode(call[2])).toBe("base local draft"); expect(call[3]).toBe("export");
});

test("원격 삭제 확인 뒤 일시 오류나 pending이 기존 삭제 안내를 지우지 않는다", async () => {
  await openEditor();
  fakes.deleted = true; fakes.inspect.mockResolvedValue("deleted");
  fakes.listeners.get("workspace-metadata-changed")?.();
  await vi.waitFor(() => expect(document.body.textContent).toMatch(/다른 기기.*삭제/));
  const before = fakes.inspect.mock.calls.length;
  fakes.inspect.mockResolvedValue("pending");
  fakes.listeners.get("workspace-sync-status-changed")?.();
  await vi.waitFor(() => expect(fakes.inspect.mock.calls.length).toBeGreaterThan(before));
  expect(document.body.textContent).toMatch(/다른 기기.*삭제/);
  const afterPending = fakes.inspect.mock.calls.length;
  fakes.inspect.mockRejectedValue(new Error("offline"));
  fakes.listeners.get("workspace-sync-status-changed")?.();
  await vi.waitFor(() => expect(fakes.inspect.mock.calls.length).toBeGreaterThan(afterPending));
  expect(document.body.textContent).toMatch(/다른 기기.*삭제/);
  expect(fakes.close).not.toHaveBeenCalled();
});

test("이전 본문 ACK 뒤 원격 삭제되면 현재 서버 반영 상태로 표시하지 않는다", async () => {
  fakes.generation = "1";
  fakes.outbox.mockResolvedValue({ binding, document_id:nodeId, first_content_conflict:null, has_conflict:false, has_failure:false, confirmed_through:"1", latest: { operation_id:"019a1000-0000-7000-8000-000000000003", operation_seq:"1", document_id:nodeId, generation:"1", status:"committed", frozen:false, abandon_state:"none", failure:null, payload_retired:false } });
  await openEditor(false);
  await vi.waitFor(() => expect(document.querySelector('#sync-status')?.textContent).toContain("서버 반영됨"));
  fakes.deleted = true; fakes.inspect.mockResolvedValue("deleted");
  fakes.listeners.get("workspace-metadata-changed")?.();
  await vi.waitFor(() => expect(document.body.textContent).toMatch(/다른 기기.*삭제/));
  expect(document.querySelector('#sync-status')?.textContent ?? "").not.toContain("서버 반영됨");
  expect(fakes.close).not.toHaveBeenCalled();
});
