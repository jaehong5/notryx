// @vitest-environment jsdom
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import type { WorkspaceRegistration } from "../../../packages/protocol-types";

const fakes = vi.hoisted(() => ({
  root: undefined as { unmount(): void } | undefined,
  listeners: new Map<string, () => void>(),
  active: undefined as WorkspaceRegistration | undefined,
  phase: "none" as "none" | "queued" | "committed",
  now: 0,
  queue: vi.fn(),
  status: vi.fn(),
  send: vi.fn(),
  open: vi.fn(),
}));
vi.mock("react-dom/client", async (original) => {
  const actual = await original<typeof import("react-dom/client")>();
  return { ...actual, createRoot: (...args: Parameters<typeof actual.createRoot>) => {
    const root = actual.createRoot(...args); fakes.root = root; return root;
  } };
});
vi.mock("@tauri-apps/api/event", () => ({ listen: async (name: string, callback: () => void) => { fakes.listeners.set(name, callback); return () => fakes.listeners.delete(name); } }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
vi.mock("./migration-recovery", () => ({ StartupGate: ({ ready }: { ready: () => unknown }) => ready() }));
vi.mock("./ipc", async (original) => {
  const actual = await original<typeof import("./ipc")>();
  return {
    ...actual,
    localDrafts: async () => [],
    pendingPairStatus: async () => "none",
    workspaceSyncState: async () => ({ binding:fakes.active, status:"ready" }),
    workspaceInventory: async () => ({ nodes:[rootNode], next_after_id:null }),
    workspaceCreateParents: async () => ({ nodes:[rootNode], next_after_id:null }),
    workspaceCreateStatus: fakes.status,
    queueWorkspaceCreate: fakes.queue,
    sendNextWorkspaceOperation: fakes.send,
    openWorkspaceDocument: fakes.open,
  };
});

const binding: WorkspaceRegistration = { server_origin:"https://example.invalid", workspace_id:"workspace", workspace_epoch:"epoch", device_id:"device", device_stream_id:"stream" };
const replacement: WorkspaceRegistration = { ...binding, device_id:"new-device", device_stream_id:"new-stream" };
const rootId = "019a1000-0000-7000-8000-000000000001";
const nodeId = "019a1000-0000-7000-8000-000000000002";
const operationId = "019a1000-0000-7000-8000-000000000003";
const rootNode = { node_id:rootId, parent_id:null, name:"", kind:"folder" as const, deleted:false, content_rev:null, content_etag:null, meta_rev:"meta", meta_etag:'"meta"', node_etag:'"node"', content_hash:null, size:null, children_generation:"0", last_change_seq:"1" };
const entry = { operation_id:operationId, operation_seq:"1", node_id:nodeId, parent_id:rootId, name:"메모", kind:"folder" as const, status:"queued" as const, reason:null };

beforeEach(() => {
  vi.resetModules(); fakes.listeners.clear(); fakes.active = binding; fakes.phase = "none"; fakes.now = 0;
  vi.spyOn(performance, "now").mockImplementation(() => fakes.now);
  fakes.queue.mockReset(); fakes.status.mockReset(); fakes.send.mockReset(); fakes.open.mockReset();
  fakes.status.mockImplementation(async () => ({ entries:fakes.phase === "none" ? [] : [{ ...entry, status:fakes.phase }], next_after_seq:null }));
  fakes.queue.mockImplementation(async () => { fakes.phase = "queued"; return entry; });
  fakes.send.mockImplementation(async () => {
    if (fakes.phase === "queued") { fakes.phase = "committed"; return { binding, status:"committed", operation_id:operationId, document_id:nodeId, generation:null }; }
    return { binding:fakes.active, status:"idle", operation_id:null, document_id:null, generation:null };
  });
});
afterEach(async () => {
  fakes.root?.unmount(); fakes.root = undefined;
  await new Promise((resolve) => setTimeout(resolve,0)); document.body.replaceChildren(); vi.restoreAllMocks();
});
function change(element: HTMLInputElement | HTMLSelectElement, value: string) {
  const prototype = element instanceof HTMLInputElement ? HTMLInputElement.prototype : HTMLSelectElement.prototype;
  Object.getOwnPropertyDescriptor(prototype, "value")!.set!.call(element,value);
  element.dispatchEvent(new Event(element instanceof HTMLInputElement ? "input" : "change", { bubbles:true }));
}
async function showCreatePanel() {
  document.body.innerHTML = '<div id="root"></div>';
  await import("./main");
  await vi.waitFor(() => expect([...document.querySelectorAll("button")].some((button) => button.textContent === "새 폴더 또는 문서")).toBe(true));
  [...document.querySelectorAll("button")].find((button) => button.textContent === "새 폴더 또는 문서")!.click();
  await vi.waitFor(() => expect(document.querySelector('[aria-label="새 폴더 또는 문서"]')).not.toBeNull());
  await vi.waitFor(() => expect(document.querySelector(`[data-testid="create-parent"] option[value="${rootId}"]`)).not.toBeNull());
  change(document.querySelector<HTMLSelectElement>('[data-testid="create-parent"]')!,rootId);
  change(document.querySelector<HTMLInputElement>('[data-testid="create-name"]')!,"메모");
  await vi.waitFor(() => expect(document.querySelector<HTMLButtonElement>('[data-testid="create-submit"]')!.disabled).toBe(false));
}

test("새 폴더 또는 문서 버튼의 요청을 durable queue에 넣고 전송 scheduler를 깨운다", async () => {
  await showCreatePanel(); fakes.now = 1000;
  document.querySelector<HTMLButtonElement>('[data-testid="create-submit"]')!.click();
  await vi.waitFor(() => expect(fakes.queue).toHaveBeenCalledExactlyOnceWith(binding,{ parent_id:rootId, name:"메모", kind:"folder" }));
  await vi.waitFor(() => expect(fakes.send).toHaveBeenCalled(), { timeout:5000 });
  await vi.waitFor(() => expect(document.body.textContent).toContain("생성 완료"), { timeout:5000 });
  expect(fakes.status).toHaveBeenCalledWith(binding,"0"); expect(fakes.open).not.toHaveBeenCalled();
});

test("등록 변경 뒤 늦게 완료된 이전 생성 요청은 새 등록 화면과 scheduler에 반영하지 않는다", async () => {
  let release!: (value: typeof entry) => void;
  fakes.queue.mockImplementation(() => new Promise<typeof entry>((resolve) => { release = resolve; }));
  await showCreatePanel();
  document.querySelector<HTMLButtonElement>('[data-testid="create-submit"]')!.click();
  await vi.waitFor(() => expect(fakes.queue).toHaveBeenCalledOnce());
  fakes.active = replacement;
  await vi.waitFor(() => expect(fakes.listeners.has("workspace-sync-status-changed")).toBe(true));
  fakes.listeners.get("workspace-sync-status-changed")!();
  await vi.waitFor(() => expect(fakes.status).toHaveBeenCalledWith(replacement,"0"));
  const sendsBefore = fakes.send.mock.calls.length;
  release(entry);
  await new Promise((resolve) => setTimeout(resolve,0));
  expect(document.body.textContent).not.toContain("메모: 전송 대기");
  expect(fakes.send.mock.calls.length).toBe(sendsBefore);
  expect(fakes.open).not.toHaveBeenCalled();
});
