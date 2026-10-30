// @vitest-environment jsdom
import { afterEach, beforeEach, expect, test, vi } from "vitest";

const fakes = vi.hoisted(() => ({
  root: undefined as { unmount(): void } | undefined,
  scan: vi.fn(), cancel: vi.fn(), open: vi.fn(), inventory: vi.fn(), sync: vi.fn(),
}));
vi.mock("react-dom/client", async (original) => {
  const actual = await original<typeof import("react-dom/client")>();
  return { ...actual, createRoot: (...args: Parameters<typeof actual.createRoot>) => { const root = actual.createRoot(...args); fakes.root = root; return root; } };
});
vi.mock("@tauri-apps/api/event", () => ({ listen: async () => () => {} }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
vi.mock("./migration-recovery", () => ({ StartupGate: ({ ready }: { ready: () => unknown }) => ready() }));
vi.mock("./ipc", async (original) => {
  const actual = await original<typeof import("./ipc")>();
  return { ...actual, localDrafts: async () => [], pendingPairStatus: async () => "none", workspaceSyncState: fakes.sync, workspaceInventory:fakes.inventory, workspaceSearch:fakes.scan, cancelWorkspaceSearch:fakes.cancel, openWorkspaceDocument:fakes.open };
});
const binding = { server_origin:"https://example.invalid", workspace_id:"workspace", workspace_epoch:"epoch", device_id:"device", device_stream_id:"stream" };
const rootId = "019a1000-0000-7000-8000-000000000001", nodeId = "019a1000-0000-7000-8000-000000000002";
const rootNode = { node_id:rootId, parent_id:null, name:"", kind:"folder" as const, deleted:false, content_rev:null, content_etag:null, meta_rev:"meta", meta_etag:'"meta"', node_etag:'"node"', content_hash:null, size:null, children_generation:"0", last_change_seq:"1" };
const node = { ...rootNode, node_id:nodeId, parent_id:rootId, name:"다른 구간.md", kind:"file" as const, content_rev:"content", content_etag:'"content"', content_hash:"sha256:" + "a".repeat(64), size:"4" };
const page = { results:[{ node_id:nodeId, scope:"workspace", source:"cache", node }], available_documents:1, total_documents:1, recovery_available_documents:0, recovery_total_documents:0, next_after_id:null };
const rangeRects = Object.getOwnPropertyDescriptor(Range.prototype, "getClientRects");
const rangeBounds = Object.getOwnPropertyDescriptor(Range.prototype, "getBoundingClientRect");

beforeEach(() => {
  Object.defineProperty(Range.prototype, "getClientRects", { configurable:true, value:() => [] });
  Object.defineProperty(Range.prototype, "getBoundingClientRect", { configurable:true, value:() => ({ left:0, right:0, top:0, bottom:0, width:0, height:0 }) });
  vi.resetModules();
  fakes.scan.mockReset(); fakes.cancel.mockReset(); fakes.open.mockReset();
  fakes.inventory.mockReset();
  fakes.sync.mockReset(); fakes.sync.mockResolvedValue({ binding, status:"offline" });
  fakes.inventory.mockResolvedValue({ nodes:[rootNode], next_after_id:null });
  fakes.scan.mockResolvedValue(page); fakes.cancel.mockResolvedValue(undefined);
  fakes.open.mockRejectedValue(new Error("synthetic open"));
});
afterEach(async () => {
  fakes.root?.unmount(); fakes.root = undefined;
  await new Promise((resolve) => setTimeout(resolve, 0)); document.body.replaceChildren();
  if (rangeRects) Object.defineProperty(Range.prototype, "getClientRects", rangeRects);
  else delete (Range.prototype as Partial<Range>).getClientRects;
  if (rangeBounds) Object.defineProperty(Range.prototype, "getBoundingClientRect", rangeBounds);
  else delete (Range.prototype as Partial<Range>).getBoundingClientRect;
});
function change(element: HTMLInputElement | HTMLSelectElement, value: string) {
  const prototype = element instanceof HTMLInputElement ? HTMLInputElement.prototype : HTMLSelectElement.prototype;
  Object.getOwnPropertyDescriptor(prototype, "value")!.set!.call(element, value);
  element.dispatchEvent(new Event(element instanceof HTMLInputElement ? "input" : "change", { bubbles:true }));
}
async function startSearch() {
  document.body.innerHTML = '<div id="root"></div>';
  await import("./main");
  await vi.waitFor(() => expect([...document.querySelectorAll("button")].some((item) => item.textContent === "본문 검색")).toBe(true));
  window.dispatchEvent(new KeyboardEvent("keydown", { key:"F", ctrlKey:true, shiftKey:true, bubbles:true }));
  await vi.waitFor(() => expect(document.querySelector('[aria-label="본문 검색"]')).not.toBeNull());
  const panel = document.querySelector<HTMLElement>('[aria-label="본문 검색"]')!;
  change(panel.querySelector("input")!, "가나");
  change(panel.querySelector("select")!, "workspace");
  await vi.waitFor(() => expect(panel.querySelector<HTMLButtonElement>('button[type="submit"]')!.disabled).toBe(false));
  panel.querySelector("form")!.dispatchEvent(new Event("submit", { bubbles:true, cancelable:true }));
  return panel;
}

test("작업 공간 등록 전에는 검색 단축키를 처리하지 않는다", async () => {
  fakes.sync.mockImplementation(() => new Promise(() => {}));
  const added = vi.spyOn(window, "addEventListener");
  try {
    document.body.innerHTML = '<div id="root"></div>';
    await import("./main");
    await vi.waitFor(() => expect(added.mock.calls.some(([type]) => type === "keydown")).toBe(true));
    const event = new KeyboardEvent("keydown", { key: "F", ctrlKey: true, shiftKey: true, bubbles: true, cancelable: true });
    window.dispatchEvent(event);
    expect(event.defaultPrevented).toBe(false);
    expect(document.querySelector('[aria-label="본문 검색"]')).toBeNull();
  } finally {
    added.mockRestore();
  }
});

test("검색 버튼 표시 직후에도 단축키가 처리된다", async () => {
  const added = vi.spyOn(window, "addEventListener");
  const removed = vi.spyOn(window, "removeEventListener");
  let observed: { handled: boolean; added: number; removed: number } | undefined;
  const observer = new MutationObserver(() => {
    if (observed || ![...document.querySelectorAll("button")].some((item) => item.textContent === "본문 검색")) return;
    const event = new KeyboardEvent("keydown", { key: "F", ctrlKey: true, shiftKey: true, bubbles: true, cancelable: true });
    const additions = added.mock.calls.filter(([type]) => type === "keydown").length;
    const removals = removed.mock.calls.filter(([type]) => type === "keydown").length;
    window.dispatchEvent(event);
    observed = { handled: event.defaultPrevented, added: additions, removed: removals };
  });
  try {
    document.body.innerHTML = '<div id="root"></div>';
    observer.observe(document.body, { childList: true, subtree: true });
    await import("./main");
    await vi.waitFor(() => expect(observed).toBeDefined());
    expect(observed!.handled, `keydown listeners added=${observed!.added}, removed=${observed!.removed}`).toBe(true);
    await vi.waitFor(() => expect(document.querySelector('[aria-label="본문 검색"]')).not.toBeNull());
  } finally {
    observer.disconnect();
    added.mockRestore();
    removed.mockRestore();
  }
});

test("단축키로 검색을 열고 현재 목록에 없는 결과도 기존 편집기로 연다", async () => {
  fakes.open.mockResolvedValue({ document_id:nodeId, edit_session_id:"search-open-session", generation:"0", bytes_base64:"YmFzZQ==", readonly:true, local_draft:false, has_session:true, cache_warning:null, binding });
  const panel = await startSearch();
  await vi.waitFor(() => expect(fakes.scan).toHaveBeenCalledOnce());
  await vi.waitFor(() => expect(panel.querySelector(`[data-search-node="${nodeId}"]`)).not.toBeNull());
  panel.querySelector<HTMLButtonElement>(`[data-search-node="${nodeId}"]`)!.click();
  await vi.waitFor(() => expect(fakes.open).toHaveBeenCalledWith(node));
  await vi.waitFor(() => expect(document.querySelector(`[data-search-node="${nodeId}"]`)).toBeNull());
  expect(document.querySelector<HTMLInputElement>('[aria-label="본문 검색"] input')?.value).toBe("가나");
  expect(document.querySelector<HTMLSelectElement>('[aria-label="본문 검색"] select')?.value).toBe("workspace");
});

test("검색 화면을 닫으면 진행 중인 요청을 식별자로 취소하고 늦은 결과를 버린다", async () => {
  let resolve!: (value: typeof page) => void;
  fakes.scan.mockImplementation(() => new Promise((done) => { resolve = done; }));
  await startSearch();
  await vi.waitFor(() => expect(fakes.scan).toHaveBeenCalledOnce());
  const request = fakes.scan.mock.calls[0][1];
  [...document.querySelectorAll("button")].find((item) => item.textContent === "본문 검색")!.click();
  await vi.waitFor(() => expect(fakes.cancel).toHaveBeenCalledWith(binding, request.request_id));
  resolve(page);
  await vi.waitFor(() => expect(document.querySelector('[aria-label="본문 검색"]')).toBeNull());
  expect(fakes.open).not.toHaveBeenCalled();
});

test("다른 문서를 열어 캐시가 바뀔 수 있으면 이전 검색 결과와 coverage를 지운다", async () => {
  const other = { ...node, node_id:"019a1000-0000-7000-8000-000000000003", name:"새로 여는 문서.md" };
  fakes.inventory.mockResolvedValue({ nodes:[rootNode, other], next_after_id:null });
  fakes.open.mockResolvedValue({ document_id:other.node_id, edit_session_id:"open-session", generation:"0", bytes_base64:"YmFzZQ==", readonly:true, local_draft:false, has_session:true, cache_warning:null, binding });
  const panel = await startSearch();
  await vi.waitFor(() => expect(panel.querySelector(`[data-search-node="${nodeId}"]`)).not.toBeNull());
  expect(panel.textContent).toContain("이번 구간에서 검색 가능한 문서: 1 / 1");
  document.querySelector<HTMLButtonElement>(`[data-document="${other.node_id}"]`)!.click();
  await vi.waitFor(() => expect(fakes.open).toHaveBeenCalledWith(other));
  await vi.waitFor(() => expect(document.querySelector(`[data-document="${other.node_id}"]`)?.getAttribute("aria-current")).toBe("page"));
  await vi.waitFor(() => expect(document.querySelector(`[data-search-node="${nodeId}"]`)).toBeNull());
  expect(document.querySelector('[aria-label="본문 검색"]')?.textContent).not.toContain("이번 구간에서 검색 가능한 문서");
  expect(document.querySelector<HTMLInputElement>('[aria-label="본문 검색"] input')?.value).toBe("가나");
  expect(document.querySelector<HTMLSelectElement>('[aria-label="본문 검색"] select')?.value).toBe("workspace");
});
