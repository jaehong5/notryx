// @vitest-environment jsdom
import { EditorView } from "@codemirror/view";
import { afterEach, beforeEach, expect, test, vi } from "vitest";

const fakes = vi.hoisted(() => ({
  root: undefined as { unmount(): void } | undefined,
  save: vi.fn(), queue: vi.fn(), transmit: vi.fn(),
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
  return { ...actual,
    localDrafts: async () => [], pendingPairStatus: async () => "none",
    workspaceSyncState: async () => ({ binding, status: "offline" }),
    workspaceInventory: async () => ({ nodes: [rootNode, node], next_after_id: null }),
    openWorkspaceDocument: async () => ({ binding, document_id: node.node_id, edit_session_id: "open-session", generation: "0", bytes_base64: "YmFzZQo=", readonly: false, local_draft: false, has_session: false, cache_warning: null }),
    workspaceDocumentOutbox: async () => ({ first_content_conflict:null, has_conflict: false, has_failure: false, latest: null }),
    saveWorkspaceDocument: fakes.save, queueWorkspaceDocument: fakes.queue, sendNextWorkspaceOperation: fakes.transmit,
  };
});

const binding = { server_origin: "https://example.invalid", workspace_id: "workspace", workspace_epoch: "epoch", device_id: "device", device_stream_id: "stream" };
const rootId = "019a1000-0000-7000-8000-000000000001", nodeId = "019a1000-0000-7000-8000-000000000002";
const rootNode = { node_id: rootId, parent_id: null, name: "", kind: "folder" as const, deleted: false, content_rev: null, content_etag: null, meta_rev: "meta", meta_etag: '"meta"', node_etag: '"node"', content_hash: null, size: null, children_generation: "0", last_change_seq: "1" };
const node = { ...rootNode, node_id: nodeId, parent_id: rootId, name: "cached.md", kind: "file" as const, content_rev: "content", content_etag: '"content"', content_hash: "sha256:" + "a".repeat(64), size: "5" };
const rangeRects = Object.getOwnPropertyDescriptor(Range.prototype, "getClientRects");
const rangeBounds = Object.getOwnPropertyDescriptor(Range.prototype, "getBoundingClientRect");

beforeEach(() => {
  Object.defineProperty(Range.prototype, "getClientRects", { configurable: true, value: () => [] });
  Object.defineProperty(Range.prototype, "getBoundingClientRect", { configurable: true, value: () => ({ left: 0, right: 0, top: 0, bottom: 0, width: 0, height: 0 }) });
  vi.resetModules();
  fakes.save.mockReset(); fakes.queue.mockReset(); fakes.transmit.mockReset();
  fakes.save.mockImplementation(async (request: { edit_session_id: string; generation: string }) => ({ edit_session_id: request.edit_session_id, generation: request.generation }));
});
afterEach(async () => {
  fakes.root?.unmount(); fakes.root = undefined;
  await new Promise((resolve) => setTimeout(resolve, 0)); document.body.replaceChildren();
  if (rangeRects) Object.defineProperty(Range.prototype, "getClientRects", rangeRects);
  else delete (Range.prototype as Partial<Range>).getClientRects;
  if (rangeBounds) Object.defineProperty(Range.prototype, "getBoundingClientRect", rangeBounds);
  else delete (Range.prototype as Partial<Range>).getBoundingClientRect;
});

test("세션 없는 재시작에서 캐시를 편집해 기기에 저장하고 서버 전송을 만들지 않는다", async () => {
  document.body.innerHTML = '<div id="root"></div>';
  await import("./main");
  await vi.waitFor(() => expect(document.querySelector(`[data-document="${nodeId}"]`)).not.toBeNull());
  expect(document.body.textContent).toContain("서버에 연결되지 않음");
  document.querySelector<HTMLButtonElement>(`[data-document="${nodeId}"]`)!.click();
  await vi.waitFor(() => expect(document.querySelector(".cm-editor")).not.toBeNull());
  const view = EditorView.findFromDOM(document.querySelector(".cm-editor")!);
  expect(view).not.toBeNull();
  view!.dispatch({ changes: { from: 0, insert: "offline " } });
  document.querySelector<HTMLButtonElement>("#save")!.click();
  await vi.waitFor(() => expect(fakes.save).toHaveBeenCalledOnce());
  expect(fakes.save.mock.calls[0][0]).toMatchObject({ document_id: nodeId, edit_session_id: "open-session", generation: "1" });
  expect(fakes.queue).not.toHaveBeenCalled();
  expect(fakes.transmit).not.toHaveBeenCalled();
});
