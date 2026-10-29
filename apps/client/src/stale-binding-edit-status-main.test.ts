// @vitest-environment jsdom
import { EditorView } from "@codemirror/view";
import { afterEach, beforeEach, expect, test, vi } from "vitest";

const fakes = vi.hoisted(() => ({
  listeners: new Map<string, () => void>(),
  root: undefined as { unmount(): void } | undefined,
  active: undefined as undefined | { server_origin:string; workspace_id:string; workspace_epoch:string; device_id:string; device_stream_id:string }, syncStatus:"ready",
  prepare: vi.fn(), commit: vi.fn(), inspect: vi.fn(), save: vi.fn(), queue: vi.fn(), transmit: vi.fn(), pair: vi.fn(), recover: vi.fn(), outbox: vi.fn(), generation:"0", deleted: false, close:vi.fn(), transmitPort:undefined as undefined | (() => Promise<unknown>),
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
  return { ...actual, AutoTransmit:class extends actual.AutoTransmit {
    constructor(ports: ConstructorParameters<typeof actual.AutoTransmit>[0]) { super(ports); fakes.transmitPort = ports.transmit; }
  } };
});
vi.mock("./ipc", async (original) => {
  const actual = await original<typeof import("./ipc")>();
  return { ...actual,
    localDrafts: async () => [], pendingPairStatus: async () => "none",
    workspaceSyncState: async () => ({ binding:fakes.active, status:fakes.syncStatus }),
    workspaceInventory: async () => ({ nodes: fakes.deleted ? [rootNode] : [rootNode, node], next_after_id: null }),
    openWorkspaceDocument: async () => ({ binding, document_id: nodeId, edit_session_id: "open-session", generation: fakes.generation, bytes_base64: btoa("resolved remote"), readonly: false, local_draft: false, has_session: true, cache_warning: null, open_notice: null }),
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
  fakes.active = binding; fakes.syncStatus = "ready";
  fakes.transmitPort = undefined;
  fakes.generation = "2";
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



test("등록 binding이 교체된 뒤 기존 편집 세션에 새로 입력하면 이전 서버 반영 성공으로 표시하지 않는다", async () => {
  const committed = { operation_id:"019a1000-0000-7000-8000-000000000003", operation_seq:"1", document_id:nodeId, generation:"2", status:"committed", frozen:false, abandon_state:"none", failure:null, payload_retired:false };
  fakes.outbox.mockResolvedValue({ binding, document_id:nodeId, first_content_conflict:null, has_conflict:false, has_failure:false, latest:committed, confirmed_through:"1" });
  const view = await openEditor(false);
  await vi.waitFor(() => expect(document.querySelector('#sync-status')?.textContent).toContain("서버 반영됨"));
  fakes.active = { ...binding, workspace_epoch:"new-epoch" };
  fakes.syncStatus = "connecting";
  fakes.listeners.get("workspace-sync-status-changed")?.();
  await vi.waitFor(() => expect(document.querySelector('#workspace-sync-status')?.textContent).toContain("서버 동기화 중"));
  view.dispatch({ changes:{ from:view.state.doc.length, insert:" new input" }, userEvent:"input.type" });
  expect(view.state.doc.toString()).toBe("resolved remote new input");
  await vi.waitFor(() => expect(document.querySelector('#sync-status')?.textContent ?? "").not.toContain("서버 반영됨"));
  expect(document.querySelector('#sync-status')?.textContent ?? "").not.toContain("서버 저장됨");
  expect(fakes.close).not.toHaveBeenCalled();
});

test("이전 등록의 늦은 outbox ACK 조회 결과는 새 등록에 반영 성공으로 표시하지 않는다", async () => {
  const committed = { operation_id:"019a1000-0000-7000-8000-000000000003", operation_seq:"1", document_id:nodeId, generation:"2", status:"committed", frozen:false, abandon_state:"none", failure:null, payload_retired:false };
  let release!: () => void;
  fakes.outbox.mockImplementationOnce(() => new Promise((resolve) => { release = () => resolve({ binding, document_id:nodeId, first_content_conflict:null, has_conflict:false, has_failure:false, latest:committed, confirmed_through:"1" }); }));
  const view = await openEditor(false);
  await vi.waitFor(() => expect(fakes.outbox).toHaveBeenCalled());
  fakes.active = { ...binding, workspace_epoch:"new-epoch" };
  fakes.syncStatus = "connecting";
  fakes.listeners.get("workspace-sync-status-changed")?.();
  await vi.waitFor(() => expect(document.querySelector('#workspace-sync-status')?.textContent).toContain("서버 동기화 중"));
  release();
  await new Promise((resolve) => setTimeout(resolve, 10));
  expect(document.querySelector('#sync-status')?.textContent ?? "").not.toContain("서버 반영됨");
  expect(document.querySelector('#sync-status')?.textContent ?? "").not.toContain("서버 저장됨");
  expect(view.state.doc.toString()).toBe("resolved remote");
  expect(fakes.close).not.toHaveBeenCalled();
});
