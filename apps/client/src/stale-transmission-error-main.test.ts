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
    workspaceInventory: async () => ({ nodes: fakes.deleted ? [rootNode] : [rootNode, node, otherNode], next_after_id: null }),
    openWorkspaceDocument: async (target:{node_id:string}) => ({ binding, document_id: target.node_id, edit_session_id: target.node_id === nodeId ? "open-session" : "other-session", generation: fakes.generation, bytes_base64: btoa(target.node_id === nodeId ? "resolved remote" : "other document"), readonly: false, local_draft: false, has_session: true, cache_warning: null, open_notice: null }),
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
const otherId = "019a1000-0000-7000-8000-000000000004";
const otherNode = { ...node, node_id:otherId, name:"other.md" };
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




test.each(["OutboxBlocked", "ReconciliationRequired"])("이전 등록의 늦은 %s 오류가 새 등록 상태를 덮지 않는다", async (error) => {
  const view = await openEditor(false);
  let reject!: (error:Error) => void;
  fakes.transmit.mockImplementationOnce(() => new Promise((_resolve, fail) => { reject = fail; }));
  const sending = fakes.transmitPort!();
  fakes.active = { ...binding, workspace_epoch:"new-epoch" }; fakes.syncStatus = "connecting";
  fakes.listeners.get("workspace-sync-status-changed")?.();
  await vi.waitFor(() => expect(document.querySelector('#workspace-sync-status')?.textContent).toContain("서버 동기화 중"));
  reject(new Error(error)); await sending;
  await new Promise((resolve) => setTimeout(resolve, 0));
  expect(document.querySelector('#sync-status')?.textContent ?? "").not.toContain("서버 결과 대조 필요");
  expect(view.state.doc.toString()).toBe("resolved remote");
});

test.each(["OutboxBlocked", "ReconciliationRequired"])("이전 문서의 늦은 %s 오류가 현재 문서 상태를 덮지 않는다", async (error) => {
  await openEditor(false);
  let reject!: (error:Error) => void;
  fakes.transmit.mockImplementationOnce(() => new Promise((_resolve, fail) => { reject = fail; }));
  const sending = fakes.transmitPort!();
  document.querySelector<HTMLButtonElement>(`[data-document="${otherId}"]`)!.click();
  await vi.waitFor(() => expect(EditorView.findFromDOM(document.querySelector('.cm-editor')!)?.state.doc.toString()).toBe("other document"));
  reject(new Error(error)); await sending;
  await new Promise((resolve) => setTimeout(resolve, 0));
  expect(document.querySelector('#sync-status')?.textContent ?? "").not.toContain("서버 결과 대조 필요");
  expect(EditorView.findFromDOM(document.querySelector('.cm-editor')!)?.state.doc.toString()).toBe("other document");
});
