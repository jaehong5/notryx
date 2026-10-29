// @vitest-environment jsdom
import { EditorView } from "@codemirror/view";
import { afterEach, beforeEach, expect, test, vi } from "vitest";

const fakes = vi.hoisted(() => ({
  listeners: new Map<string, () => void>(),
  root: undefined as { unmount(): void } | undefined,
  prepare: vi.fn(), commit: vi.fn(), inspect: vi.fn(), save: vi.fn(), queue: vi.fn(), transmit: vi.fn(), pair: vi.fn(), recover: vi.fn(), outbox: vi.fn(), generation:"0", localDraft:false, deleted: false, close:vi.fn(), transmitPort:undefined as undefined | (() => Promise<unknown>),
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
    workspaceSyncState: async () => ({ binding, status: "ready" }),
    workspaceInventory: async () => ({ nodes: fakes.deleted ? [rootNode] : [rootNode, node], next_after_id: null }),
    openWorkspaceDocument: async () => ({ binding, document_id: nodeId, edit_session_id: "open-session", generation: fakes.generation, bytes_base64: btoa("resolved remote"), readonly: false, local_draft: fakes.localDraft, has_session: true, cache_warning: null, open_notice: null }),
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
  fakes.transmitPort = undefined;
  fakes.generation = "2";
  fakes.localDraft = false;
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



const previous = { operation_id:"019a1000-0000-7000-8000-000000000003", operation_seq:"1", document_id:nodeId, generation:"1", status:"conflict", frozen:false, abandon_state:"none", failure:null, payload_retired:false };

test("원격판 선택을 끝낸 문서는 과거 412가 마지막 이력이어도 서버 반영 대기로 표시하지 않는다", async () => {
  fakes.outbox.mockResolvedValue({ binding, document_id:nodeId, first_content_conflict:null, has_conflict:false, has_failure:false, latest:previous, confirmed_through:"1" });
  const view = await openEditor(false);
  await vi.waitFor(() => expect(fakes.outbox).toHaveBeenCalled());
  await new Promise((resolve) => setTimeout(resolve, 0));
  expect(view.state.doc.toString()).toBe("resolved remote");
  expect(document.querySelector('#sync-status')?.textContent ?? "").not.toContain("서버 반영 대기");
  expect(document.querySelector('#sync-status')?.textContent ?? "").not.toContain("서버 반영됨");
  expect(document.querySelector('[data-testid="conflict-review-open"]')).toBeNull();
  expect(fakes.queue).not.toHaveBeenCalled(); expect(fakes.close).not.toHaveBeenCalled();
});

test("해결되지 않은 충돌은 과거 이력이 아니라 충돌 있음으로 유지한다", async () => {
  fakes.outbox.mockResolvedValue({ binding, document_id:nodeId, first_content_conflict:{ operation_id:previous.operation_id, operation_seq:previous.operation_seq }, has_conflict:true, has_failure:false, latest:previous, confirmed_through:"1" });
  await openEditor(false);
  await vi.waitFor(() => expect(document.querySelector('#sync-status')?.textContent).toContain("충돌 있음"));
  expect(document.querySelector('[data-testid="conflict-review-open"]')).not.toBeNull();
});

test("해결 이후 새 queued 편집은 서버 반영 대기로 계속 표시한다", async () => {
  const pending = { ...previous, operation_id:"019a1000-0000-7000-8000-000000000004", operation_seq:"2", generation:"2", status:"queued" };
  fakes.outbox.mockResolvedValue({ binding, document_id:nodeId, first_content_conflict:null, has_conflict:false, has_failure:false, latest:pending, confirmed_through:"1" });
  await openEditor(false);
  await vi.waitFor(() => expect(document.querySelector('#sync-status')?.textContent).toContain("서버 반영 대기"));
});

test("terminal 실패는 충돌 해결 여부와 상관없이 대조 필요 상태를 유지한다", async () => {
  fakes.outbox.mockResolvedValue({ binding, document_id:nodeId, first_content_conflict:null, has_conflict:false, has_failure:true, latest:previous, confirmed_through:"1" });
  await openEditor(false);
  await vi.waitFor(() => expect(document.querySelector('#sync-status')?.textContent).toContain("서버 결과 대조 필요"));
});

test("ACK까지 확인된 취소가 마지막 이력일 때 서버 반영 대기로 표시하지 않는다", async () => {
  const abandoned = { ...previous, operation_id:"019a1000-0000-7000-8000-000000000004", operation_seq:"2", generation:"2", status:"abandoned" };
  fakes.outbox.mockResolvedValue({ binding, document_id:nodeId, first_content_conflict:null, has_conflict:false, has_failure:false, latest:abandoned, confirmed_through:"2" });
  await openEditor(false);
  await vi.waitFor(() => expect(fakes.outbox).toHaveBeenCalled());
  await new Promise((resolve) => setTimeout(resolve, 0));
  expect(document.querySelector('#sync-status')?.textContent ?? "").not.toContain("서버 반영 대기");
  expect(document.querySelector('#sync-status')?.textContent ?? "").not.toContain("서버 반영됨");
});

test("취소 receipt의 서버 ACK가 아직 확인되지 않으면 확인 중으로 구분한다", async () => {
  const abandoned = { ...previous, operation_id:"019a1000-0000-7000-8000-000000000004", operation_seq:"2", generation:"2", status:"abandoned" };
  fakes.outbox.mockResolvedValue({ binding, document_id:nodeId, first_content_conflict:null, has_conflict:false, has_failure:false, latest:abandoned, confirmed_through:"1" });
  await openEditor(false);
  await vi.waitFor(() => expect(document.querySelector('#sync-status')?.textContent).toContain("영수증 확인 중"));
  expect(document.querySelector('#sync-status')?.textContent ?? "").not.toContain("서버 반영됨");
});

test.each(["conflict", "abandoned"] as const)("해결된 이전 %s 뒤 새 입력은 다른 문서의 늦은 전송 결과가 대기 표시를 지우지 않는다", async (status) => {
  const latest = { ...previous, status, operation_seq:status === "abandoned" ? "2" : "1" };
  fakes.outbox.mockResolvedValue({ binding, document_id:nodeId, first_content_conflict:null, has_conflict:false, has_failure:false, latest, confirmed_through:"2" });
  const view = await openEditor(false);
  await vi.waitFor(() => expect(fakes.outbox).toHaveBeenCalled());
  let release!: () => void;
  fakes.transmit.mockImplementationOnce(() => new Promise((resolve) => { release = () => resolve({ binding, status:"committed", operation_id:"019a1000-0000-7000-8000-000000000005", document_id:"019a1000-0000-7000-8000-000000000004", generation:"1" }); }));
  const sending = fakes.transmitPort!();
  await vi.waitFor(() => expect(fakes.transmit).toHaveBeenCalled());
  view.dispatch({ changes:{ from:view.state.doc.length, insert:" new input" }, userEvent:"input.type" });
  await vi.waitFor(() => expect(document.querySelector('#sync-status')?.textContent).toContain("서버 반영 대기"));
  const before = fakes.outbox.mock.calls.length;
  release(); await sending;
  await vi.waitFor(() => expect(fakes.outbox.mock.calls.length).toBeGreaterThan(before));
  await new Promise((resolve) => setTimeout(resolve, 0));
  expect(document.querySelector('#sync-status')?.textContent).toContain("서버 반영 대기");
  expect(view.state.doc.toString()).toBe("resolved remote new input");
});

test.skip("저장된 미전송 초안을 다시 열고 queue 응답을 기다리는 동안 이전 해결 이력은 대기 표시를 지우지 않는다", async () => {
  fakes.localDraft = true;
  fakes.outbox.mockResolvedValue({ binding, document_id:nodeId, first_content_conflict:null, has_conflict:false, has_failure:false, latest:previous, confirmed_through:"1" });
  let release!: () => void;
  fakes.queue.mockImplementation(() => new Promise((resolve) => { release = () => resolve({ operation_id:"019a1000-0000-7000-8000-000000000005", operation_seq:"2", document_id:nodeId, generation:"2", committed:false }); }));
  const view = await openEditor(false);
  await vi.waitFor(() => expect(fakes.queue).toHaveBeenCalled());
  await vi.waitFor(() => expect(fakes.outbox).toHaveBeenCalled());
  await vi.waitFor(() => expect(document.querySelector('#sync-status')?.textContent).toContain("서버 반영 대기"));
  expect(view.state.doc.toString()).toBe("resolved remote");
  release();
});

test("이전 generation queue의 위조된 최신 generation 응답은 새 입력의 대기 표시를 지우지 않는다", async () => {
  fakes.outbox.mockResolvedValue({ binding, document_id:nodeId, first_content_conflict:null, has_conflict:false, has_failure:false, latest:previous, confirmed_through:"1" });
  const view = await openEditor(false);
  view.dispatch({ changes:{ from:view.state.doc.length, insert:" first" }, userEvent:"input.type" });
  let release!: () => void;
  fakes.queue.mockImplementation(() => new Promise((resolve) => { release = () => resolve({ operation_id:"019a1000-0000-7000-8000-000000000005", operation_seq:"2", document_id:nodeId, generation:"4", committed:false }); }));
  document.querySelector<HTMLButtonElement>('#save')!.click();
  await vi.waitFor(() => expect(fakes.queue).toHaveBeenCalledWith(nodeId, "open-session", "3"));
  view.dispatch({ changes:{ from:view.state.doc.length, insert:" second" }, userEvent:"input.type" });
  release();
  await new Promise((resolve) => setTimeout(resolve, 0));
  const before = fakes.outbox.mock.calls.length;
  fakes.transmit.mockResolvedValueOnce({ binding, status:"idle", operation_id:null, document_id:null, generation:null });
  await fakes.transmitPort!();
  await vi.waitFor(() => expect(fakes.outbox.mock.calls.length).toBeGreaterThan(before));
  await vi.waitFor(() => expect(document.querySelector('#sync-status')?.textContent).toContain("서버 반영 대기"));
  expect(view.state.doc.toString()).toBe("resolved remote first second");
});
