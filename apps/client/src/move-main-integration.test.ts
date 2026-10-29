// @vitest-environment jsdom
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import type { WorkspaceRegistration } from "../../../packages/protocol-types";

const fakes = vi.hoisted(() => ({
  root: undefined as { unmount(): void } | undefined,
  listeners: new Map<string, () => void>(), active: undefined as WorkspaceRegistration | undefined,
  noNode: false, meta: '"original-meta"', queue: vi.fn(), status: vi.fn(), cancel: vi.fn(), kick: vi.fn(), send: vi.fn(),
}));
vi.mock("react-dom/client", async (original) => {
  const actual = await original<typeof import("react-dom/client")>();
  return { ...actual, createRoot: (...args: Parameters<typeof actual.createRoot>) => { const root = actual.createRoot(...args); fakes.root = root; return root; } };
});
vi.mock("@tauri-apps/api/event", () => ({ listen: async (name: string, callback: () => void) => { fakes.listeners.set(name, callback); return () => fakes.listeners.delete(name); } }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
vi.mock("./migration-recovery", () => ({ StartupGate: ({ ready }: { ready: () => unknown }) => ready() }));
vi.mock("./auto-transmit", async (original) => { const actual = await original<typeof import("./auto-transmit")>(); return { ...actual, AutoTransmit: class extends actual.AutoTransmit { override kick() { fakes.kick(); super.kick(); } } }; });
vi.mock("./ipc", async (original) => {
  const actual = await original<typeof import("./ipc")>();
  return { ...actual, localDrafts: async () => [], pendingPairStatus: async () => "none",
    workspaceSyncState: async () => ({ binding: fakes.active, status: "ready" }),
    workspaceInventory: async () => ({ nodes: fakes.noNode ? [rootNode, destination] : [rootNode, { ...node, meta_rev: fakes.meta.slice(1, -1), meta_etag: fakes.meta }, destination], next_after_id: null }),
    workspaceCreateParents: async () => ({ nodes: [rootNode, destination], next_after_id: null }),
    workspaceMoveStatus: fakes.status, queueWorkspaceMove: fakes.queue, requestWorkspaceMoveAbandon: fakes.cancel,
    openWorkspaceDocument: async () => ({ binding, document_id:nodeId, edit_session_id:"session", generation:"0", bytes_base64:btoa("base"), readonly:false, local_draft:false, has_session:true, cache_warning:null, open_notice:null }),
    workspaceDocumentOutbox: async () => ({ binding:fakes.active, document_id:nodeId, first_content_conflict:null, has_conflict:false, has_failure:false, latest:null, confirmed_through:"0" }),
    prepareWorkspaceRemoteRefresh:async()=>null, inspectWorkspaceRemoteChange:async()=>"same", sendNextWorkspaceOperation:fakes.send,
  };
});

const binding: WorkspaceRegistration = { server_origin:"https://example.invalid", workspace_id:"workspace", workspace_epoch:"epoch", device_id:"device", device_stream_id:"stream" };
const rootId = "019a1000-0000-7000-8000-000000000001", nodeId = "019a1000-0000-7000-8000-000000000002", destId = "019a1000-0000-7000-8000-000000000003", operationId = "019a1000-0000-7000-8000-000000000004";
const rootNode = { node_id:rootId, parent_id:null, name:"", kind:"folder" as const, deleted:false, content_rev:null, content_etag:null, meta_rev:"meta", meta_etag:'"meta"', node_etag:'"root"', content_hash:null, size:null, children_generation:"0", last_change_seq:"1" };
const destination = { ...rootNode, node_id:destId, parent_id:rootId, name:"destination" };
const node = { ...rootNode, node_id:nodeId, parent_id:rootId, name:"Synthetic.md", kind:"file" as const, content_rev:"content", content_etag:'"content"', content_hash:"sha256:" + "a".repeat(64), size:"1" };
const request = { expected_parent_id:rootId, parent_id:destId, name:"renamed.md" };
const entry = { operation_id:operationId, operation_seq:"1", node_id:nodeId, expected_meta_etag:'"original-meta"', request, status:"queued" as const, reason:null };

beforeEach(() => {
  vi.resetModules(); fakes.listeners.clear(); fakes.active = binding; fakes.noNode = false; fakes.meta = '"original-meta"';
  fakes.queue.mockReset().mockResolvedValue(entry); fakes.status.mockReset().mockResolvedValue({ entries:[], next_after_seq:null });
  fakes.cancel.mockReset().mockResolvedValue(undefined); fakes.kick.mockReset();
  fakes.send.mockReset().mockResolvedValue({ binding, status:"idle", operation_id:null, document_id:null, generation:null });
});
afterEach(async () => { fakes.root?.unmount(); fakes.root = undefined; await new Promise((resolve) => setTimeout(resolve, 0)); document.body.replaceChildren(); vi.restoreAllMocks(); });
function change(element: HTMLInputElement | HTMLSelectElement, value: string) {
  const prototype = element instanceof HTMLInputElement ? HTMLInputElement.prototype : HTMLSelectElement.prototype;
  Object.getOwnPropertyDescriptor(prototype, "value")!.set!.call(element, value);
  element.dispatchEvent(new Event(element instanceof HTMLInputElement ? "input" : "change", { bubbles:true }));
}
async function openApp() {
  document.body.innerHTML = '<div id="root"></div>';
  await import("./main");
  await vi.waitFor(() => expect(document.querySelector('[data-testid="move-requests-toggle"]')).not.toBeNull());
}
async function showMove() {
  await openApp();
  await vi.waitFor(() => expect(document.querySelector(`[data-move-node="${nodeId}"]`)).not.toBeNull());
  document.querySelector<HTMLButtonElement>(`[data-move-node="${nodeId}"]`)!.click();
  await vi.waitFor(() => expect(document.querySelector<HTMLSelectElement>('[data-testid="move-parent"]')?.options.length).toBeGreaterThan(1));
  change(document.querySelector<HTMLSelectElement>('[data-testid="move-parent"]')!, destId);
  change(document.querySelector<HTMLInputElement>('[data-testid="move-name"]')!, "renamed.md");
  document.querySelector<HTMLButtonElement>('[data-testid="move-prepare"]')!.click();
  await vi.waitFor(() => expect(document.querySelector('[data-testid="move-confirm"]')).not.toBeNull());
}

test("목록의 이동 확인에서 본 파일과 meta ETag를 그대로 대기열에 저장한다", async () => {
  await showMove();
  expect(fakes.queue).not.toHaveBeenCalled();
  expect(document.querySelector('[aria-label="문서 이름 변경 또는 이동"]')?.textContent).toContain("본문을 검사하지 않았습니다");
  document.querySelector<HTMLButtonElement>('[data-testid="move-confirm"]')!.click();
  await vi.waitFor(() => expect(fakes.queue).toHaveBeenCalledExactlyOnceWith(binding,{ node_id:nodeId, expected_meta_etag:'"original-meta"', request }));
});

test("live 문서가 없어도 독립 이동 요청을 열고 취소할 수 있다", async () => {
  fakes.noNode = true; fakes.status.mockResolvedValue({ entries:[{ ...entry, status:"sent" }], next_after_seq:null });
  await openApp(); document.querySelector<HTMLButtonElement>('[data-testid="move-requests-toggle"]')!.click();
  await vi.waitFor(() => expect(document.querySelector(`[data-move-cancel="${operationId}"]`)).not.toBeNull());
  expect(document.querySelector('[data-testid="move-confirm"]')).toBeNull();
  document.querySelector<HTMLButtonElement>(`[data-move-cancel="${operationId}"]`)!.click();
  await vi.waitFor(() => expect(fakes.cancel).toHaveBeenCalledExactlyOnceWith(binding, operationId));
});

test("이동 패널이 닫혀도 같은 등록에 저장된 요청은 전송을 시작한다", async () => {
  let release!: (value: typeof entry) => void;
  fakes.queue.mockImplementation(() => new Promise<typeof entry>((resolve) => { release = resolve; }));
  await showMove();
  document.querySelector<HTMLButtonElement>('[data-testid="move-confirm"]')!.click();
  await vi.waitFor(() => expect(fakes.queue).toHaveBeenCalledOnce());
  document.querySelector<HTMLButtonElement>('[data-testid="move-requests-toggle"]')!.click();
  await vi.waitFor(() => expect(document.querySelector('[data-testid="move-confirm"]')).toBeNull());
  const previous = fakes.kick.mock.calls.length;
  release(entry);
  await vi.waitFor(() => expect(fakes.kick.mock.calls.length).toBeGreaterThan(previous));
});

test("이전 등록의 늦은 큐 완료는 새 등록의 전송을 시작하지 않는다", async () => {
  let release!: (value: typeof entry) => void;
  fakes.queue.mockImplementation(() => new Promise<typeof entry>((resolve) => { release = resolve; }));
  await showMove();
  document.querySelector<HTMLButtonElement>('[data-testid="move-confirm"]')!.click();
  await vi.waitFor(() => expect(fakes.queue).toHaveBeenCalledOnce());
  fakes.active = { ...binding, workspace_epoch:"new-epoch" };
  fakes.listeners.get("workspace-sync-status-changed")?.();
  await new Promise((resolve) => setTimeout(resolve, 0));
  const previous = fakes.kick.mock.calls.length;
  release(entry); await new Promise((resolve) => setTimeout(resolve, 20));
  expect(fakes.kick.mock.calls.length).toBe(previous);
});
