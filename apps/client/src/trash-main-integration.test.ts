// @vitest-environment jsdom
import { afterEach, beforeEach, expect, test, vi } from "vitest";

const fakes = vi.hoisted(() => ({
  root: undefined as { unmount(): void } | undefined,
  phase: "none" as "none" | "queued" | "sent" | "abandoning" | "abandoned" | "committed",
  now: 0,
  open: vi.fn(),
  cancel: vi.fn(),
  queue: vi.fn(),
  send: vi.fn(),
}));

vi.mock("react-dom/client", async (original) => {
  const actual = await original<typeof import("react-dom/client")>();
  return { ...actual, createRoot: (...args: Parameters<typeof actual.createRoot>) => {
    const root = actual.createRoot(...args);
    fakes.root = root;
    return root;
  } };
});
vi.mock("@tauri-apps/api/event", () => ({ listen: async () => () => {} }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
vi.mock("./migration-recovery", () => ({ StartupGate: ({ ready }: { ready: () => unknown }) => ready() }));
vi.mock("./ipc", async (original) => {
  const actual = await original<typeof import("./ipc")>();
  return {
    ...actual,
    localDrafts: async () => [],
    pendingPairStatus: async () => "none",
    workspaceSyncState: async () => ({ binding, status:"ready" }),
    workspaceInventory: async () => ({ nodes:[rootNode], next_after_id:null }),
    workspaceTrash: async () => ({ workspace_epoch:binding.workspace_epoch, nodes:[trashNode], next_after_id:null }),
    workspaceRestoreStatus: async () => ({ entries:fakes.phase === "none" ? [] : [{ operation_id:operationId, operation_seq:"1", node_id:trashNode.node_id, request:restoreRequest, status:fakes.phase, reason:null }], next_after_seq:null }),
    queueWorkspaceRestore: fakes.queue,
    requestWorkspaceRestoreAbandon: fakes.cancel,
    sendNextWorkspaceOperation: fakes.send,
    openWorkspaceDocument: fakes.open,
  };
});

const binding = { server_origin:"https://example.invalid", workspace_id:"workspace", workspace_epoch:"epoch", device_id:"device", device_stream_id:"stream" };
const rootId = "019a1000-0000-7000-8000-000000000001";
const nodeId = "019a1000-0000-7000-8000-000000000002";
const operationId = "019a1000-0000-7000-8000-000000000003";
const rootNode = { node_id:rootId, parent_id:null, name:"", kind:"folder" as const, deleted:false, content_rev:null, content_etag:null, meta_rev:"meta", meta_etag:'"meta"', node_etag:'"node"', content_hash:null, size:null, children_generation:"0", last_change_seq:"1" };
const trashNode = { node_id:nodeId, parent_id:rootId, name:"deleted.md", kind:"file" as const, meta_rev:"old", restorable:true };
const restoreRequest = { expected_meta_rev:"old", parent_id:rootId, name:"deleted.md" };

beforeEach(() => {
  vi.resetModules();
  fakes.phase = "none";
  fakes.now = 0;
  vi.spyOn(performance, "now").mockImplementation(() => fakes.now);
  fakes.open.mockReset();
  fakes.cancel.mockReset();
  fakes.queue.mockReset();
  fakes.send.mockReset();
  fakes.queue.mockImplementation(async () => {
    fakes.phase = "queued";
    return { operation_id:operationId, operation_seq:"1", node_id:nodeId, request:restoreRequest, status:"queued", reason:null };
  });
  fakes.cancel.mockImplementation(async () => { fakes.phase = "abandoning"; });
  fakes.send.mockImplementation(async () => {
    if (fakes.phase === "queued") {
      fakes.phase = "sent";
      return { binding, status:"permission_denied", operation_id:operationId, document_id:nodeId, generation:null };
    }
    if (fakes.phase === "abandoning") {
      fakes.phase = "abandoned";
      return { binding, status:"abandoned", operation_id:operationId, document_id:nodeId, generation:null };
    }
    return { binding, status:"idle", operation_id:null, document_id:null, generation:null };
  });
});

afterEach(async () => {
  fakes.root?.unmount();
  fakes.root = undefined;
  await new Promise((resolve) => setTimeout(resolve, 0));
  document.body.replaceChildren();
  vi.restoreAllMocks();
});

test("열린 문서가 없는 등록에서 권한 거절된 복원은 명시적 취소 후 전송을 재개한다", async () => {
  document.body.innerHTML = '<div id="root"></div>';
  await import("./main");
  await vi.waitFor(() => expect(document.querySelector('[aria-label="휴지통"]') || [...document.querySelectorAll("button")].some((button) => button.textContent === "휴지통")).toBeTruthy());
  if (!document.querySelector('[aria-label="휴지통"]')) [...document.querySelectorAll("button")].find((button) => button.textContent === "휴지통")!.click();
  await vi.waitFor(() => expect(document.querySelector('[aria-label="휴지통"]')).not.toBeNull());
  await vi.waitFor(() => expect(document.querySelector(`[data-trash-node="${nodeId}"]`)).not.toBeNull());
  document.querySelector<HTMLButtonElement>(`[data-trash-node="${nodeId}"]`)!.click();
  await vi.waitFor(() => expect(document.querySelector('[data-testid="restore-parent"]')).not.toBeNull());
  const parent = document.querySelector<HTMLSelectElement>('[data-testid="restore-parent"]')!;
  parent.value = rootId;
  parent.dispatchEvent(new Event("change", { bubbles:true }));
  fakes.now = 1000;
  document.querySelector<HTMLButtonElement>('[data-testid="restore-submit"]')!.click();
  await vi.waitFor(() => expect(fakes.queue).toHaveBeenCalledOnce());
  await vi.waitFor(() => expect(document.body.textContent).toContain("복원 권한이 없습니다"), { timeout:5000 });
  expect(fakes.open).not.toHaveBeenCalled();
  expect(fakes.cancel).not.toHaveBeenCalled();
  document.querySelector<HTMLButtonElement>(`[data-restore-cancel="${operationId}"]`)!.click();
  await vi.waitFor(() => expect(fakes.cancel).toHaveBeenCalledExactlyOnceWith(binding, operationId));
  await vi.waitFor(() => expect(fakes.phase).toBe("abandoned"), { timeout:5000 });
  await vi.waitFor(() => expect(document.body.textContent).toContain("취소 완료"));
  expect(fakes.open).not.toHaveBeenCalled();
});

test("취소 시 서버에 기존 복원 결과가 있으면 같은 작업의 결과를 다시 확인한다", async () => {
  let delivered = false;
  fakes.cancel.mockImplementation(async () => { fakes.phase = "committed"; throw new Error("operation_has_result"); });
  fakes.send.mockImplementation(async () => {
    if (fakes.phase === "queued") {
      fakes.phase = "sent";
      return { binding, status:"permission_denied", operation_id:operationId, document_id:nodeId, generation:null };
    }
    if (fakes.phase === "committed" && !delivered) {
      delivered = true;
      return { binding, status:"committed", operation_id:operationId, document_id:nodeId, generation:null };
    }
    return { binding, status:"idle", operation_id:null, document_id:null, generation:null };
  });
  document.body.innerHTML = '<div id="root"></div>';
  await import("./main");
  await vi.waitFor(() => expect(document.querySelector('[aria-label="휴지통"]') || [...document.querySelectorAll("button")].some((button) => button.textContent === "휴지통")).toBeTruthy());
  if (!document.querySelector('[aria-label="휴지통"]')) [...document.querySelectorAll("button")].find((button) => button.textContent === "휴지통")!.click();
  await vi.waitFor(() => expect(document.querySelector(`[data-trash-node="${nodeId}"]`)).not.toBeNull());
  document.querySelector<HTMLButtonElement>(`[data-trash-node="${nodeId}"]`)!.click();
  await vi.waitFor(() => expect(document.querySelector('[data-testid="restore-parent"]')).not.toBeNull());
  const parent = document.querySelector<HTMLSelectElement>('[data-testid="restore-parent"]')!;
  parent.value = rootId;
  parent.dispatchEvent(new Event("change", { bubbles:true }));
  fakes.now = 1000;
  document.querySelector<HTMLButtonElement>('[data-testid="restore-submit"]')!.click();
  await vi.waitFor(() => expect(document.body.textContent).toContain("복원 권한이 없습니다"), { timeout:5000 });
  document.querySelector<HTMLButtonElement>(`[data-restore-cancel="${operationId}"]`)!.click();
  await vi.waitFor(() => expect(fakes.cancel).toHaveBeenCalledExactlyOnceWith(binding, operationId));
  await vi.waitFor(() => expect(delivered).toBe(true), { timeout:5000 });
  await vi.waitFor(() => expect(document.body.textContent).toContain("복원 완료"));
  expect(document.body.textContent).not.toContain("취소 완료");
  expect(fakes.open).not.toHaveBeenCalled();
});
