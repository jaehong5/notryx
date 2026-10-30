// @vitest-environment jsdom
import { afterEach, beforeEach, expect, test, vi } from "vitest";

const fakes = vi.hoisted(() => ({
  listeners: new Map<string, () => void>(),
  status: "connecting",
  inventory: vi.fn(),
  sync: vi.fn(),
  pair: vi.fn(),
  root: undefined as { unmount(): void } | undefined,
}));

vi.mock("react-dom/client", async (original) => {
  const actual = await original<typeof import("react-dom/client")>();
  return { ...actual, createRoot: (...args: Parameters<typeof actual.createRoot>) => {
    const root = actual.createRoot(...args);
    fakes.root = root;
    return root;
  } };
});
vi.mock("@tauri-apps/api/event", () => ({
  listen: async (name: string, callback: () => void) => {
    fakes.listeners.set(name, callback);
    return () => { fakes.listeners.delete(name); };
  },
}));
vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
vi.mock("./migration-recovery", () => ({ StartupGate: ({ ready }: { ready: () => unknown }) => ready() }));
vi.mock("./ipc", async (original) => {
  const actual = await original<typeof import("./ipc")>();
  return {
    ...actual,
    localDrafts: async () => [],
    pendingPairStatus: async () => "none",
    workspaceSyncState: fakes.sync,
    pairWorkspace: fakes.pair,
    workspaceInventory: fakes.inventory,
    sendNextWorkspaceOperation: async () => ({ status: "idle", binding: { server_origin: "https://example.invalid", workspace_id: "workspace", workspace_epoch: "epoch", device_id: "device", device_stream_id: "stream" } }),
  };
});

beforeEach(() => {
  vi.resetModules();
  fakes.sync.mockReset();
  fakes.pair.mockReset();
  fakes.sync.mockImplementation(async () => ({
    binding: { server_origin: "https://example.invalid", workspace_id: "workspace", workspace_epoch: "epoch", device_id: "device", device_stream_id: "stream" },
    status: fakes.status,
  }));
  fakes.pair.mockResolvedValue(undefined);
});

afterEach(() => {
  fakes.root?.unmount();
  fakes.root = undefined;
  fakes.listeners.clear();
  document.body.replaceChildren();
});

test("sync events re-read current status and preserve the selected inventory page", async () => {
  fakes.status = "connecting";
  fakes.inventory.mockReset();
  fakes.inventory.mockImplementation(async (after: string | null) => {
    const number = after === null ? 1 : Number(after.slice(2)) + 1;
    const nodeId = `id${String(number).padStart(4, "0")}`;
    return {
      nodes: [{ node_id: nodeId, parent_id: null, name: `doc-${number}.md`, kind: "file", deleted: false, content_rev: "c", content_etag: "c", meta_rev: "m", meta_etag: "m", node_etag: "n", content_hash: "hash", size: "1", children_generation: "0", last_change_seq: "1" }],
      next_after_id: number <= 512 ? nodeId : null,
    };
  });
  document.body.innerHTML = '<div id="root"></div>';
  await import("./main");
  await vi.waitFor(() => expect(document.querySelector("#workspace-sync-status")?.textContent).toContain("서버 동기화 중"));
  fakes.status = "needs_review";
  fakes.listeners.get("workspace-sync-status-changed")?.();
  await vi.waitFor(() => expect(document.querySelector("#workspace-sync-status")?.textContent).toContain("서버 상태 대조 필요"));
  fakes.status = "offline";
  fakes.listeners.get("workspace-sync-status-changed")?.();
  await vi.waitFor(() => expect(document.querySelector("#workspace-sync-status")?.textContent).toContain("서버에 연결되지 않음"));
  const next = [...document.querySelectorAll("button")].find((button) => button.textContent === "다음 목록");
  expect(next).toBeDefined();
  next!.click();
  await vi.waitFor(() => expect(document.querySelector('[data-document="id0513"]')).not.toBeNull());
  fakes.inventory.mockClear();
  fakes.listeners.get("workspace-metadata-changed")?.();
  await vi.waitFor(() => expect(fakes.inventory).toHaveBeenCalledWith("id0512"));
  expect(document.querySelector('[data-document="id0513"]')).not.toBeNull();
  fakes.inventory.mockClear();
  fakes.status = "ready";
  fakes.listeners.get("workspace-sync-status-changed")?.();
  await vi.waitFor(() => expect(fakes.inventory).toHaveBeenCalledWith("id0512"));
  expect(document.querySelector('[data-document="id0513"]')).not.toBeNull();
  fakes.inventory.mockClear();
  const statusReads = fakes.sync.mock.calls.length;
  fakes.listeners.get("workspace-sync-status-changed")?.();
  await vi.waitFor(() => expect(fakes.sync).toHaveBeenCalledTimes(statusReads + 1));
  expect(fakes.inventory).not.toHaveBeenCalled();
});

test("an old status response during re-pair cannot replace the new registration status", async () => {
  fakes.status = "connecting";
  fakes.inventory.mockReset();
  fakes.inventory.mockResolvedValue({ nodes: [], next_after_id: null });
  let finishPair!: () => void;
  fakes.pair.mockImplementationOnce(() => new Promise<void>((resolve) => { finishPair = resolve; }));
  document.body.innerHTML = '<div id="root"></div>';
  await import("./main");
  await vi.waitFor(() => expect(document.querySelector("#workspace-sync-status")?.textContent).toContain("서버 동기화 중"));
  const form = document.querySelector("form")!;
  (form.elements.namedItem("origin") as HTMLInputElement).value = "https://example.invalid";
  (form.elements.namedItem("workspace_id") as HTMLInputElement).value = "workspace";
  (form.elements.namedItem("pairing_secret") as HTMLInputElement).value = "synthetic";
  form.dispatchEvent(new Event("submit", { bubbles: true, cancelable: true }));
  await vi.waitFor(() => expect(fakes.pair).toHaveBeenCalledOnce());
  let finishOldStatus!: (value: unknown) => void;
  fakes.sync.mockImplementationOnce(() => new Promise((resolve) => { finishOldStatus = resolve; }));
  fakes.listeners.get("workspace-sync-status-changed")?.();
  await vi.waitFor(() => expect(finishOldStatus).toBeTypeOf("function"));
  fakes.status = "ready";
  finishPair();
  await vi.waitFor(() => expect(document.querySelector("#workspace-sync-status")).toBeNull());
  finishOldStatus({ binding: { server_origin: "https://old.invalid", workspace_id: "old", workspace_epoch: "old", device_id: "old", device_stream_id: "old" }, status: "needs_review" });
  await vi.waitFor(() => expect(document.querySelector("#workspace-sync-status")).toBeNull());
});

const cachedNode = {
  node_id: "019a1000-0000-7000-8000-000000000002", parent_id: null,
  name: "cached.md", kind: "file", deleted: false, content_rev: "c",
  content_etag: "c", meta_rev: "m", meta_etag: "m", node_etag: "n",
  content_hash: "hash", size: "1", children_generation: null, last_change_seq: "1",
};

test("document buttons wait for initial synchronization while offline cache remains accessible", async () => {
  fakes.status = "connecting";
  fakes.inventory.mockReset();
  fakes.inventory.mockResolvedValue({ nodes: [cachedNode], next_after_id: null });
  document.body.innerHTML = '<div id="root"></div>';
  await import("./main");
  await vi.waitFor(() => expect(document.querySelector<HTMLButtonElement>('button[type="submit"]')?.disabled).toBe(false));
  const button = () => document.querySelector<HTMLButtonElement>(`[data-document="${cachedNode.node_id}"]`);
  expect(button()).not.toBeNull();
  expect(button()!.disabled).toBe(true);
  expect(document.querySelector("#workspace-sync-status")?.textContent).toContain("서버 동기화 중");

  fakes.status = "ready";
  fakes.listeners.get("workspace-sync-status-changed")?.();
  await vi.waitFor(() => expect(button()!.disabled).toBe(false));

  fakes.status = "offline";
  fakes.listeners.get("workspace-sync-status-changed")?.();
  await vi.waitFor(() => expect(document.querySelector("#workspace-sync-status")?.textContent).toContain("서버에 연결되지 않음"));
  expect(button()!.disabled).toBe(false);
});

test("reconnection retries a failed inventory even when no metadata change event arrives", async () => {
  fakes.status = "offline";
  fakes.inventory.mockReset();
  fakes.inventory.mockRejectedValueOnce(new Error("Network"));
  fakes.inventory.mockResolvedValue({ nodes: [cachedNode], next_after_id: null });
  document.body.innerHTML = '<div id="root"></div>';
  await import("./main");
  await vi.waitFor(() => expect(document.body.textContent).toContain("Network"));
  expect(document.querySelector(`[data-document="${cachedNode.node_id}"]`)).toBeNull();

  fakes.status = "ready";
  fakes.listeners.get("workspace-sync-status-changed")?.();
  await vi.waitFor(() => expect(document.querySelector(`[data-document="${cachedNode.node_id}"]`)).not.toBeNull());
  expect(fakes.inventory).toHaveBeenCalledTimes(2);
  expect(document.body.textContent).not.toContain("Network");
});
