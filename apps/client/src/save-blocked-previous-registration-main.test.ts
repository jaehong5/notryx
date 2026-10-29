// @vitest-environment jsdom
import { EditorState } from "@codemirror/state";
import { EditorView } from "@codemirror/view";
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import type { AutoTransmit } from "./auto-transmit";
import type { EditorController } from "./editor-controller";

const fakes = vi.hoisted(() => ({
  listeners: new Map<string, () => void>(),
  root: undefined as { unmount(): void } | undefined,
  controller: undefined as EditorController | undefined,
  binding: undefined as undefined | { server_origin: string; workspace_id: string; workspace_epoch: string; device_id: string; device_stream_id: string },
  status: "ready", failRelease: false,
  open: vi.fn(), close: vi.fn(), save: vi.fn(), queue: vi.fn(), recover: vi.fn(),
}));
vi.mock("react-dom/client", async (original) => {
  const actual = await original<typeof import("react-dom/client")>();
  return { ...actual, createRoot: (...args: Parameters<typeof actual.createRoot>) => { const root = actual.createRoot(...args); fakes.root = root; return root; } };
});
vi.mock("@tauri-apps/api/event", () => ({ listen: async (name: string, callback: () => void) => {
  fakes.listeners.set(name, callback); return () => { fakes.listeners.delete(name); };
} }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
vi.mock("./migration-recovery", () => ({ StartupGate: ({ ready }: { ready: () => unknown }) => ready() }));
vi.mock("./editor-controller", async (original) => {
  const actual = await original<typeof import("./editor-controller")>();
  return { ...actual, EditorController: class extends actual.EditorController {
    constructor(ports: ConstructorParameters<typeof actual.EditorController>[0]) { super(ports); fakes.controller = this; }
  } };
});
vi.mock("./auto-transmit", async (original) => {
  const actual = await original<typeof import("./auto-transmit")>();
  return { ...actual, AutoTransmit: class extends actual.AutoTransmit {
    releaseConflictSession(scope: Parameters<AutoTransmit["releaseConflictSession"]>[0]) {
      if (fakes.failRelease) throw new Error("synthetic release failure");
      return super.releaseConflictSession(scope);
    }
  } };
});
vi.mock("./ipc", async (original) => {
  const actual = await original<typeof import("./ipc")>();
  return { ...actual,
    localDrafts: async () => [], pendingPairStatus: async () => "none",
    workspaceSyncState: async () => ({ binding: fakes.binding, status: fakes.status }),
    workspaceInventory: async () => ({ nodes: [rootNode, node, otherNode], next_after_id: null }),
    openWorkspaceDocument: fakes.open,
    workspaceDocumentOutbox: async (registration: typeof binding, id: string) => ({ binding: registration, document_id: id, first_content_conflict: null, has_conflict: false, has_failure: false, latest: null, confirmed_through: "0" }),
    prepareWorkspaceRemoteRefresh: async () => null, inspectWorkspaceRemoteChange: async () => "same",
    saveWorkspaceDocument: fakes.save, queueWorkspaceDocument: fakes.queue,
    sendNextWorkspaceOperation: async () => ({ status: "idle", binding }),
    closeWorkspaceDocument: fakes.close, recoverWorkspaceSource: fakes.recover,
  };
});

const binding = { server_origin: "https://example.invalid", workspace_id: "workspace", workspace_epoch: "epoch", device_id: "device", device_stream_id: "stream" };
const rootId = "019a1000-0000-7000-8000-000000000001", nodeId = "019a1000-0000-7000-8000-000000000002", otherId = "019a1000-0000-7000-8000-000000000003";
const rootNode = { node_id: rootId, parent_id: null, name: "", kind: "folder" as const, deleted: false, content_rev: null, content_etag: null, meta_rev: "meta", meta_etag: '"meta"', node_etag: '"node"', content_hash: null, size: null, children_generation: "0", last_change_seq: "1" };
const node = { ...rootNode, node_id: nodeId, parent_id: rootId, name: "synthetic.md", kind: "file" as const, content_rev: "content", content_etag: '"content"', content_hash: "sha256:" + "a".repeat(64), size: "7" };
const otherNode = { ...node, node_id: otherId, name: "other.md" };
const rangeRects = Object.getOwnPropertyDescriptor(Range.prototype, "getClientRects");
const rangeBounds = Object.getOwnPropertyDescriptor(Range.prototype, "getBoundingClientRect");
const memoryText = "base 한글🧭";

beforeEach(() => {
  Object.defineProperty(Range.prototype, "getClientRects", { configurable: true, value: () => [] });
  Object.defineProperty(Range.prototype, "getBoundingClientRect", { configurable: true, value: () => ({ left: 0, right: 0, top: 0, bottom: 0, width: 0, height: 0 }) });
  vi.resetModules(); fakes.binding = binding; fakes.status = "ready"; fakes.failRelease = false; fakes.controller = undefined;
  fakes.open.mockReset().mockImplementation(async (selected: typeof node) => ({ binding, document_id: selected.node_id, edit_session_id: selected.node_id === nodeId ? "open-session" : "other-session", generation: "0", bytes_base64: selected.node_id === nodeId ? btoa("\xef\xbb\xbfbase") : btoa("other"), readonly: false, local_draft: false, has_session: true, cache_warning: null, open_notice: null }));
  fakes.close.mockReset().mockResolvedValue(undefined);
  fakes.save.mockReset().mockImplementation(async (request: { edit_session_id: string; generation: string }) => ({ edit_session_id: request.edit_session_id, generation: request.generation }));
  fakes.queue.mockReset().mockImplementation(async (id: string, _session: string, generation: string) => ({ operation_id: "019a1000-0000-7000-8000-000000000004", document_id: id, generation, committed: false }));
  fakes.recover.mockReset().mockRejectedValue(new Error("stale_session"));
});
afterEach(async () => {
  fakes.root?.unmount(); fakes.root = undefined; fakes.controller = undefined;
  await new Promise((resolve) => setTimeout(resolve, 0));
  fakes.listeners.clear(); document.body.replaceChildren();
  if (rangeRects) Object.defineProperty(Range.prototype, "getClientRects", rangeRects); else delete (Range.prototype as Partial<Range>).getClientRects;
  if (rangeBounds) Object.defineProperty(Range.prototype, "getBoundingClientRect", rangeBounds); else delete (Range.prototype as Partial<Range>).getBoundingClientRect;
});

// Fake IPC models invalid native session; it does not exercise the system clipboard or keyboard.
async function blockedEditor() {
  document.body.innerHTML = '<div id="root"></div>';
  await import("./main");
  await vi.waitFor(() => expect(document.querySelector(`[data-document="${nodeId}"]`)).not.toBeNull());
  document.querySelector<HTMLButtonElement>(`[data-document="${nodeId}"]`)!.click();
  await vi.waitFor(() => expect(document.querySelector(".cm-editor")).not.toBeNull());
  const view = EditorView.findFromDOM(document.querySelector(".cm-editor")!)!;
  const controller = fakes.controller!;
  fakes.save.mockRejectedValue(new Error("stale_session"));
  fakes.failRelease = true;
  view.dispatch({ changes: { from: view.state.doc.length, insert: " 한글🧭" }, userEvent: "input.type" });
  const snapshot = controller.recoverySnapshot();
  const action = vi.fn(async () => ({ status: "pending" as const, version: "0" }));
  await expect(controller.withConflictSnapshot(snapshot, action)).rejects.toThrow();
  expect(action).not.toHaveBeenCalled();
  expect(view.state.facet(EditorState.readOnly)).toBe(true);
  expect(view.state.doc.toString()).toBe(memoryText);
  await vi.waitFor(() => expect(document.body.textContent).toContain("원문을 복구"));
  return { view, controller };
}

function clickCopy() { document.querySelector<HTMLButtonElement>("#copy-source")!.click(); }
function fallback() { return document.querySelector<HTMLTextAreaElement>('[data-testid="memory-source-fallback"]'); }

test("복구 시작 전에 등록이 교체된 경우 이전 등록의 메모리 원문임을 밝히고 수동 복구를 제공한다", async () => {
  const { view } = await blockedEditor();
  const currentBinding = { ...binding, server_origin: "https://new.example.invalid", workspace_id: "new-workspace", workspace_epoch: "new-epoch" };
  fakes.binding = currentBinding; fakes.status = "connecting";
  fakes.listeners.get("workspace-sync-status-changed")?.();
  await vi.waitFor(() => expect(document.querySelector("#workspace-sync-status")?.textContent).toContain("서버 동기화 중"));
  expect(view.state.doc.toString()).toBe(memoryText);

  clickCopy();
  await vi.waitFor(() => expect(fakes.recover).toHaveBeenCalledOnce());
  await vi.waitFor(() => expect(fallback()).not.toBeNull());
  expect(fallback()!.value).toBe(memoryText);
  expect(fallback()!.readOnly).toBe(true);
  const region = document.querySelector('[data-testid="memory-source-recovery"]');
  expect(region).not.toBeNull();
  expect(region!.contains(fallback())).toBe(true);
  const guidance = region!.textContent ?? "";
  expect(guidance).toContain(binding.server_origin);
  expect(guidance).toContain(binding.workspace_id);
  expect(guidance).toContain(binding.workspace_epoch);
  expect(guidance).not.toContain(currentBinding.server_origin);
  expect(guidance).not.toContain(currentBinding.workspace_id);
  expect(guidance).not.toContain(currentBinding.workspace_epoch);
  expect(guidance).toMatch(/이전.*(등록|작업 공간)|(등록|작업 공간).*이전/s);
  expect(guidance).toMatch(/(수동|직접).*(선택|복사)|(선택|복사).*(수동|직접)/s);
  expect(guidance).toMatch(/BOM/i);
  expect(guidance).toMatch(/바이트|bytes/i);
  expect(guidance).not.toContain("원문을 복사했습니다.");
  expect(fakes.recover.mock.calls[0][3]).toBe("copy");
  expect(view.state.doc.toString()).toBe(memoryText);
  expect(view.state.facet(EditorState.readOnly)).toBe(true);
});
