// @vitest-environment jsdom
import { afterEach, beforeEach, expect, test, vi } from "vitest";

const fakes = vi.hoisted(() => ({
  root: undefined as { unmount(): void } | undefined,
  status: vi.fn(),
  pair: vi.fn(),
  listeners: new Map<string, () => void>(),
}));

vi.mock("react-dom/client", async (original) => {
  const actual = await original<typeof import("react-dom/client")>();
  return { ...actual, createRoot: (...args: Parameters<typeof actual.createRoot>) => { const root = actual.createRoot(...args); fakes.root = root; return root; } };
});
vi.mock("@tauri-apps/api/event", () => ({ listen: async (name: string, handler: () => void) => { fakes.listeners.set(name, handler); return () => fakes.listeners.delete(name); } }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
vi.mock("./migration-recovery", () => ({ StartupGate: ({ ready }: { ready: () => unknown }) => ready() }));
vi.mock("./credential-status", async (original) => {
  const actual = await original<typeof import("./credential-status")>();
  return { ...actual, workspaceCredentialStatus: fakes.status };
});
vi.mock("./ipc", async (original) => {
  const actual = await original<typeof import("./ipc")>();
  return { ...actual, localDrafts: async () => [], pendingPairStatus: async () => "none", workspaceSyncState: async () => ({ binding, status: "offline" }), workspaceInventory: async () => ({ nodes: [rootNode], next_after_id: null }), pairWorkspace: fakes.pair };
});

const binding = { server_origin: "https://example.invalid", workspace_id: "workspace", workspace_epoch: "epoch", device_id: "device", device_stream_id: "stream" };
const rootNode = { node_id: "019a1000-0000-7000-8000-000000000001", parent_id: null, name: "", kind: "folder" as const, deleted: false, content_rev: null, content_etag: null, meta_rev: "meta", meta_etag: '"meta"', node_etag: '"node"', content_hash: null, size: null, children_generation: "0", last_change_seq: "1" };

beforeEach(() => {
  vi.resetModules();
  fakes.status.mockReset(); fakes.pair.mockReset(); fakes.listeners.clear();
  fakes.status.mockResolvedValue("none"); fakes.pair.mockResolvedValue(undefined);
});
afterEach(async () => {
  fakes.root?.unmount(); fakes.root = undefined;
  await new Promise((resolve) => setTimeout(resolve, 0)); document.body.replaceChildren();
});

async function startApp(): Promise<void> {
  document.body.innerHTML = '<div id="root"></div>';
  await import("./main");
  await vi.waitFor(() => expect(fakes.listeners.has("workspace-credential-status-changed")).toBe(true));
  await vi.waitFor(() => expect(document.querySelector<HTMLButtonElement>(".workspace-connect button")?.disabled).toBe(false));
}

function message(): string | null {
  return document.querySelector('[data-testid="credential-status"]')?.textContent ?? null;
}

test("시작과 상태 이벤트에서 인증정보 저장 안내를 갱신한다", async () => {
  fakes.status.mockResolvedValue("session_only");
  await startApp();
  await vi.waitFor(() => expect(message()).toBe("인증정보를 이 실행 동안만 사용합니다. 앱을 다시 시작하면 다시 등록해야 합니다."));
  fakes.status.mockResolvedValue("pending");
  fakes.listeners.get("workspace-credential-status-changed")!();
  await vi.waitFor(() => expect(message()).toContain("대기 중인 등록"));
  fakes.status.mockResolvedValue("unavailable");
  fakes.listeners.get("workspace-credential-status-changed")!();
  await vi.waitFor(() => expect(message()).toBe("저장된 인증정보를 복원하지 못했습니다. 비밀 저장소와 서버 상태를 확인해 주세요."));
  fakes.status.mockResolvedValue("identity_unsupported");
  fakes.listeners.get("workspace-credential-status-changed")!();
  await vi.waitFor(() => expect(message()).toBe("서버에서 기기 신원 확인 API를 사용할 수 없어 인증정보를 자동 복원할 수 없습니다. 서버 설정과 버전을 확인해 주세요."));
  fakes.status.mockResolvedValue("registration_required");
  fakes.listeners.get("workspace-credential-status-changed")!();
  await vi.waitFor(() => expect(message()).toBe("이전에 저장한 인증정보가 없어 다시 등록해야 합니다. 기기 자료는 보존됩니다."));
  fakes.status.mockResolvedValue("secure");
  fakes.listeners.get("workspace-credential-status-changed")!();
  await vi.waitFor(() => expect(message()).toBeNull());
});

test("페어링이 끝난 뒤 session-only 상태를 다시 확인한다", async () => {
  await startApp();
  expect(message()).toBeNull();
  const form = document.querySelector<HTMLFormElement>(".workspace-connect")!;
  (form.elements.namedItem("origin") as HTMLInputElement).value = "https://example.invalid";
  (form.elements.namedItem("workspace_id") as HTMLInputElement).value = "synthetic-workspace";
  (form.elements.namedItem("pairing_secret") as HTMLInputElement).value = "synthetic-pairing-secret";
  fakes.status.mockResolvedValue("session_only");
  form.dispatchEvent(new Event("submit", { bubbles: true, cancelable: true }));
  await vi.waitFor(() => expect(fakes.pair).toHaveBeenCalledOnce());
  await vi.waitFor(() => expect(message()).toContain("이 실행 동안만 사용"));
  expect((form.elements.namedItem("pairing_secret") as HTMLInputElement).value).toBe("");
});

test("늦게 도착한 이전 상태와 오류의 비밀 텍스트를 표시하지 않는다", async () => {
  await startApp();
  let resolvePrevious!: (value: string) => void;
  fakes.status.mockImplementationOnce(() => new Promise((resolve) => { resolvePrevious = resolve; }));
  fakes.listeners.get("workspace-credential-status-changed")!();
  fakes.status.mockResolvedValueOnce("session_only");
  fakes.listeners.get("workspace-credential-status-changed")!();
  await vi.waitFor(() => expect(message()).toContain("이 실행 동안만 사용"));
  resolvePrevious("secure");
  await new Promise((resolve) => setTimeout(resolve, 0));
  expect(message()).toContain("이 실행 동안만 사용");
  fakes.status.mockRejectedValueOnce(new Error("synthetic-credential-value"));
  fakes.listeners.get("workspace-credential-status-changed")!();
  await vi.waitFor(() => expect(message()).toBe("인증정보 저장 상태를 확인하지 못했습니다."));
  expect(document.body.textContent).not.toContain("synthetic-credential-value");
});
