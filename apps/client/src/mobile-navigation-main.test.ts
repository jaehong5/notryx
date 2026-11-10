// @vitest-environment jsdom
import { undo } from "@codemirror/commands";
import { EditorView } from "@codemirror/view";
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import type { NodeState } from "../../../packages/protocol-types";

const fakes = vi.hoisted(() => ({
  root: undefined as { unmount(): void } | undefined,
  open: vi.fn(), save: vi.fn(), close: vi.fn(), workspace: false, openWorkspace: vi.fn(),
}));

vi.mock("react-dom/client", async (original) => {
  const actual = await original<typeof import("react-dom/client")>();
  return { ...actual, createRoot: (...args: Parameters<typeof actual.createRoot>) => {
    const root = actual.createRoot(...args); fakes.root = root; return root;
  } };
});
vi.mock("@tauri-apps/api/event", () => ({ listen: async () => () => {} }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: async () => ({ selection: "light", custom: null, safe_mode: false, warning: null }) }));
vi.mock("./migration-recovery", () => ({ StartupGate: ({ ready }: { ready: () => unknown }) => ready() }));
vi.mock("./credential-status", async (original) => ({
  ...await original<typeof import("./credential-status")>(),
  workspaceCredentialStatus: async () => ({ status: "none", renewal_status: "none" }),
}));
vi.mock("./ipc", async (original) => ({
  ...await original<typeof import("./ipc")>(),
  localDrafts: async () => [], pendingPairStatus: async () => "none",
  workspaceSyncState: async () => {
    if (!fakes.workspace) throw new Error("not_connected");
    return { binding, status: "ready" };
  },
  workspaceInventory: async () => ({ nodes: [rootNode, smallNode, largeNode], next_after_id: null }),
  openWorkspaceDocument: fakes.openWorkspace, closeWorkspaceDocument: fakes.close,
  prepareWorkspaceRemoteRefresh: async () => null, inspectWorkspaceRemoteChange: async () => "same",
  workspaceDocumentOutbox: async () => ({ binding, has_conflict: false, has_failure: false, latest: null, confirmed_through: "0", first_content_conflict: null }),
  sendNextWorkspaceOperation: async () => ({ status: "idle", binding }),
  openDocument: fakes.open, saveDocument: fakes.save, closeDocument: fakes.close,
}));

const originalText = "# Synthetic note\n\nOriginal body\n";
const binding = { server_origin: "https://example.invalid", workspace_id: "workspace", workspace_epoch: "epoch", device_id: "device", device_stream_id: "stream" };
const rootNode: NodeState = { node_id: "019a1000-0000-7000-8000-000000000001", parent_id: null, name: "", kind: "folder", deleted: false, content_rev: null, content_etag: null, meta_rev: "meta", meta_etag: '"meta"', node_etag: '"node"', content_hash: null, size: null, children_generation: "0", last_change_seq: "1" };
const smallNode: NodeState = { ...rootNode, node_id: "019a1000-0000-7000-8000-000000000002", parent_id: rootNode.node_id, name: "small.md", kind: "file", content_rev: "content", content_etag: '"content"', content_hash: "sha256:" + "a".repeat(64), size: "25" };
const largeNode: NodeState = { ...smallNode, node_id: "019a1000-0000-7000-8000-000000000003", name: "large.md", size: String(5 * 1024 * 1024 + 1) };
const rangeRects = Object.getOwnPropertyDescriptor(Range.prototype, "getClientRects");
const rangeBounds = Object.getOwnPropertyDescriptor(Range.prototype, "getBoundingClientRect");

beforeEach(() => {
  vi.resetModules();
  fakes.workspace = false; fakes.openWorkspace.mockReset();
  vi.stubGlobal("matchMedia", vi.fn((query: string) => ({
    matches: query === "(max-width: 650px)", media: query, onchange: null,
    addListener: vi.fn(), removeListener: vi.fn(), addEventListener: vi.fn(), removeEventListener: vi.fn(), dispatchEvent: vi.fn(),
  })));
  Object.defineProperty(Range.prototype, "getClientRects", { configurable: true, value: () => [] });
  Object.defineProperty(Range.prototype, "getBoundingClientRect", { configurable: true, value: () => ({ left: 0, right: 0, top: 0, bottom: 0, width: 0, height: 0 }) });
  fakes.open.mockReset().mockImplementation(async (id: string) => ({
    document_id: id, edit_session_id: `session-${id}`, generation: "0", bytes_base64: btoa(originalText),
  }));
  fakes.save.mockReset().mockImplementation(async (request: { edit_session_id: string; generation: string }) => ({
    edit_session_id: request.edit_session_id, generation: request.generation,
  }));
  fakes.close.mockReset().mockResolvedValue(undefined);
});

afterEach(async () => {
  fakes.root?.unmount(); fakes.root = undefined;
  await new Promise((resolve) => setTimeout(resolve, 0));
  document.body.replaceChildren(); vi.unstubAllGlobals();
  if (rangeRects) Object.defineProperty(Range.prototype, "getClientRects", rangeRects);
  else delete (Range.prototype as Partial<Range>).getClientRects;
  if (rangeBounds) Object.defineProperty(Range.prototype, "getBoundingClientRect", rangeBounds);
  else delete (Range.prototype as Partial<Range>).getBoundingClientRect;
});

function screen(): string | null {
  return document.querySelector("main")?.getAttribute("data-mobile-screen") ?? null;
}

function button(testId: string): HTMLButtonElement {
  const element = document.querySelector<HTMLButtonElement>(`[data-testid="${testId}"]`);
  expect(element).not.toBeNull();
  return element!;
}

function editor(): EditorView {
  return EditorView.findFromDOM(document.querySelector(".cm-editor")!)!;
}

async function startApp(): Promise<void> {
  document.body.innerHTML = '<div id="root"></div>';
  await import("./main");
  await vi.waitFor(() => expect(fakes.open).toHaveBeenCalledWith("welcome"));
  await vi.waitFor(() => expect(document.querySelector<HTMLButtonElement>('[data-document="unicode"]')?.disabled).toBe(false));
  expect(screen()).toBe("list");
}

async function readDocument(): Promise<EditorView> {
  document.querySelector<HTMLButtonElement>('[data-document="unicode"]')!.click();
  await vi.waitFor(() => expect(screen()).toBe("read"));
  expect(document.querySelector('.cm-content')?.getAttribute("contenteditable")).toBe("false");
  await vi.waitFor(() => expect(document.querySelector(".preview-pane")?.textContent).toContain("Original body"));
  return editor();
}

test("모바일 목록과 읽기 및 편집 이동은 저장 완료를 기다리고 편집 세션과 undo를 보존한다", async () => {
  await startApp();
  const view = await readDocument();
  fakes.open.mockClear(); fakes.close.mockClear();
  button("mobile-edit").click();
  await vi.waitFor(() => expect(screen()).toBe("edit"));
  expect(document.querySelector(".preview-pane")).toBeNull();
  expect(view.contentDOM.getAttribute("contenteditable")).toBe("true");
  view.dispatch({ changes: { from: view.state.doc.length, insert: "New text\n" } });
  let finishSave!: (value: { edit_session_id: string; generation: string }) => void;
  fakes.save.mockImplementationOnce(() => new Promise((resolve) => { finishSave = resolve; }));
  button("mobile-back").click();
  await vi.waitFor(() => expect(fakes.save).toHaveBeenCalled());
  expect(screen()).toBe("edit");
  const request = fakes.save.mock.calls[0][0];
  expect(atob(request.bytes_base64)).toBe(originalText + "New text\n");
  finishSave({ edit_session_id: request.edit_session_id, generation: request.generation });
  await vi.waitFor(() => expect(screen()).toBe("read"));
  expect(editor()).toBe(view);
  expect(view.contentDOM.getAttribute("contenteditable")).toBe("false");
  await vi.waitFor(() => expect(document.querySelector(".preview-pane")?.textContent).toContain("New text"));
  button("mobile-back").click();
  await vi.waitFor(() => expect(screen()).toBe("list"));
  await readDocument();
  button("mobile-edit").click();
  await vi.waitFor(() => expect(screen()).toBe("edit"));
  expect(editor()).toBe(view);
  expect(fakes.open).not.toHaveBeenCalled();
  expect(fakes.close).not.toHaveBeenCalled();
  expect(undo(view)).toBe(true);
  expect(view.state.doc.toString()).toBe(originalText);
});

test("모바일 편집에서 읽기로 이동할 때 저장에 실패하면 입력과 편집 화면을 유지한다", async () => {
  await startApp();
  const view = await readDocument();
  button("mobile-edit").click();
  await vi.waitFor(() => expect(screen()).toBe("edit"));
  view.dispatch({ changes: { from: view.state.doc.length, insert: "Unsaved text\n" } });
  fakes.save.mockRejectedValue(new Error("synthetic_storage_unavailable"));
  fakes.close.mockClear();
  button("mobile-back").click();
  await vi.waitFor(() => expect(document.querySelector('[role="alert"]')?.textContent).toContain("이 기기에 저장하지 못했습니다"));
  expect(screen()).toBe("edit");
  expect(editor()).toBe(view);
  expect(view.state.doc.toString()).toBe(originalText + "Unsaved text\n");
  expect(view.contentDOM.getAttribute("contenteditable")).toBe("true");
  expect(fakes.close).not.toHaveBeenCalled();
});

test("모바일 문서 열기가 실패하면 목록을 유지하고 그 화면에 오류를 표시한다", async () => {
  await startApp();
  fakes.open.mockRejectedValueOnce(new Error("synthetic_open_failed"));
  document.querySelector<HTMLButtonElement>('[data-document="unicode"]')!.click();
  await vi.waitFor(() => expect(document.querySelector('nav [role="alert"]')?.textContent).toContain("synthetic_open_failed"));
  expect(screen()).toBe("list");
});

test("모바일 미리보기 내부 링크로 큰 읽기 전용 문서를 열면 미리보기 대신 원문을 표시한다", async () => {
  fakes.workspace = true;
  const largeText = "x".repeat(5 * 1024 * 1024) + "\n";
  fakes.openWorkspace.mockImplementation(async (node: NodeState) => ({
    binding, document_id: node.node_id, edit_session_id: `session-${node.node_id}`, generation: "0",
    bytes_base64: btoa(node.node_id === largeNode.node_id ? largeText : "[Large document](large.md)"),
    readonly: node.node_id === largeNode.node_id, local_draft: false,
    has_session: node.node_id !== largeNode.node_id, cache_warning: null, open_notice: null,
  }));
  document.body.innerHTML = '<div id="root"></div>';
  await import("./main");
  await vi.waitFor(() => expect(document.querySelector<HTMLButtonElement>(`[data-document="${smallNode.node_id}"]`)?.disabled).toBe(false));
  document.querySelector<HTMLButtonElement>(`[data-document="${smallNode.node_id}"]`)!.click();
  await vi.waitFor(() => expect(screen()).toBe("read"));
  await vi.waitFor(() => expect(document.querySelector(".preview-pane button")?.textContent).toBe("Large document"));
  document.querySelector<HTMLButtonElement>(".preview-pane button")!.click();
  await vi.waitFor(() => expect(editor().state.doc.length).toBe(largeText.length));
  await vi.waitFor(() => expect(document.querySelector("main")?.getAttribute("data-mobile-preview")).toBe("false"));
  expect(screen()).toBe("read");
  expect(document.querySelector(".preview-pane")).toBeNull();
  expect(editor().contentDOM.getAttribute("contenteditable")).toBe("false");
  expect(button("mobile-edit").disabled).toBe(true);
});
