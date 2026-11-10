// @vitest-environment jsdom
import { undo } from "@codemirror/commands";
import { EditorView } from "@codemirror/view";
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import type { NodeState } from "../../../packages/protocol-types";

const fakes = vi.hoisted(() => ({
  root: undefined as { unmount(): void } | undefined,
  open: vi.fn(), save: vi.fn(), close: vi.fn(), workspace: false, openWorkspace: vi.fn(),
  background: vi.fn(), prepareRefresh: vi.fn(), compact: true, events: new Map<string, (event: { payload: boolean }) => void>(),
}));

vi.mock("react-dom/client", async (original) => {
  const actual = await original<typeof import("react-dom/client")>();
  return { ...actual, createRoot: (...args: Parameters<typeof actual.createRoot>) => {
    const root = actual.createRoot(...args); fakes.root = root; return root;
  } };
});
vi.mock("@tauri-apps/api/event", () => ({ listen: async (name: string, handler: (event: { payload: boolean }) => void) => {
  fakes.events.set(name, handler);
  return () => { fakes.events.delete(name); };
} }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: async () => ({ selection: "light", custom: null, safe_mode: false, warning: null }) }));
vi.mock("./migration-recovery", () => ({ StartupGate: ({ ready }: { ready: () => unknown }) => ready() }));
vi.mock("./credential-status", async (original) => ({
  ...await original<typeof import("./credential-status")>(),
  workspaceCredentialStatus: async () => ({ status: "none", renewal_status: "none" }),
}));
vi.mock("./ipc", async (original) => ({
  ...await original<typeof import("./ipc")>(),
  localDrafts: async () => [], pendingPairStatus: async () => "none",
  backgroundApp: fakes.background,
  workspaceSyncState: async () => {
    if (!fakes.workspace) throw new Error("not_connected");
    return { binding, status: "ready" };
  },
  workspaceInventory: async () => ({ nodes: [rootNode, smallNode, largeNode], next_after_id: null }),
  openWorkspaceDocument: fakes.openWorkspace, closeWorkspaceDocument: fakes.close,
  prepareWorkspaceRemoteRefresh: fakes.prepareRefresh, inspectWorkspaceRemoteChange: async () => "same",
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
  fakes.compact = true; fakes.prepareRefresh.mockReset().mockResolvedValue(null); fakes.events.clear(); fakes.background.mockReset().mockResolvedValue(undefined);
  vi.stubGlobal("matchMedia", vi.fn((query: string) => ({
    matches: fakes.compact && query === "(max-width: 650px)", media: query, onchange: null,
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

function systemBack(): void {
  const event = new Event("android-back-requested", { cancelable: true });
  window.dispatchEvent(event);
  expect(event.defaultPrevented).toBe(true);
}

test("Android 뒤로 가기는 저장을 기다려 읽기와 목록으로 이동하고 목록에서 앱을 배경으로 보낸다", async () => {
  await startApp();
  const view = await readDocument();
  button("mobile-edit").click();
  await vi.waitFor(() => expect(screen()).toBe("edit"));
  view.dispatch({ changes: { from: view.state.doc.length, insert: "Back checkpoint\n" } });
  let finishSave!: (value: { edit_session_id: string; generation: string }) => void;
  fakes.save.mockImplementationOnce(() => new Promise((resolve) => { finishSave = resolve; }));
  systemBack(); systemBack();
  await vi.waitFor(() => expect(fakes.save).toHaveBeenCalledOnce());
  expect(screen()).toBe("edit");
  expect(fakes.background).not.toHaveBeenCalled();
  const request = fakes.save.mock.calls[0][0];
  finishSave({ edit_session_id: request.edit_session_id, generation: request.generation });
  await vi.waitFor(() => expect(screen()).toBe("read"));
  systemBack();
  await vi.waitFor(() => expect(screen()).toBe("list"));
  systemBack();
  await vi.waitFor(() => expect(fakes.background).toHaveBeenCalledOnce());
  expect(editor()).toBe(view);
  expect(view.state.doc.toString()).toBe(originalText + "Back checkpoint\n");
});

test("Android 뒤로 가기는 저장 실패와 조합 중에 편집 화면과 입력을 보존한다", async () => {
  await startApp();
  const view = await readDocument();
  button("mobile-edit").click();
  await vi.waitFor(() => expect(screen()).toBe("edit"));
  view.dispatch({ changes: { from: view.state.doc.length, insert: "Unsaved back\n" } });
  fakes.save.mockRejectedValue(new Error("synthetic_back_save_failure"));
  systemBack();
  await vi.waitFor(() => expect(document.body.textContent).toContain("이 기기에 저장하지 못했습니다"));
  expect(screen()).toBe("edit");
  fakes.save.mockClear();
  view.contentDOM.dispatchEvent(new CompositionEvent("compositionstart", { bubbles: true }));
  systemBack();
  await vi.waitFor(() => expect(document.body.textContent).toContain("한글 조합을 마친 뒤"));
  expect(fakes.save).not.toHaveBeenCalled();
  expect(fakes.background).not.toHaveBeenCalled();
  expect(view.state.doc.toString()).toBe(originalText + "Unsaved back\n");
});

test("넓은 Android 화면도 뒤로 가기 전에 저장하고 앱 이동 실패 시 편집을 유지한다", async () => {
  fakes.compact = false;
  await startApp();
  const view = editor();
  view.dispatch({ changes: { from: view.state.doc.length, insert: "Wide checkpoint\n" } });
  fakes.background.mockRejectedValueOnce(new Error("background_failed"));
  systemBack();
  await vi.waitFor(() => expect(fakes.background).toHaveBeenCalledOnce());
  expect(fakes.save).toHaveBeenCalledOnce();
  expect(fakes.save.mock.invocationCallOrder[0]).toBeLessThan(fakes.background.mock.invocationCallOrder[0]);
  expect(editor()).toBe(view);
  expect(view.contentDOM.getAttribute("contenteditable")).toBe("true");
  await vi.waitFor(() => expect(document.body.textContent).toContain("앱을 백그라운드로 전환하지 못했습니다"));
  expect(undo(view)).toBe(true);
  expect(view.state.doc.toString()).toBe(originalText);
});

test("앱 background 전환은 즉시 기기 저장하고 복귀는 같은 편집기와 undo를 유지한다", async () => {
  await startApp();
  const view = await readDocument();
  button("mobile-edit").click();
  await vi.waitFor(() => expect(screen()).toBe("edit"));
  fakes.open.mockClear(); fakes.close.mockClear();
  view.dispatch({ changes: { from: view.state.doc.length, insert: "Background checkpoint\n" } });
  expect(fakes.events.has("app-foreground-changed")).toBe(true);
  fakes.events.get("app-foreground-changed")!({ payload: false });
  await vi.waitFor(() => expect(fakes.save).toHaveBeenCalledOnce(), { timeout: 500 });
  expect(atob(fakes.save.mock.calls[0][0].bytes_base64)).toBe(originalText + "Background checkpoint\n");
  fakes.events.get("app-foreground-changed")!({ payload: true });
  await new Promise((resolve) => setTimeout(resolve, 0));
  expect(editor()).toBe(view);
  expect(screen()).toBe("edit");
  expect(fakes.open).not.toHaveBeenCalled(); expect(fakes.close).not.toHaveBeenCalled();
  expect(undo(view)).toBe(true);
  expect(view.state.doc.toString()).toBe(originalText);
});

test("앱 복귀는 실패한 background 저장을 다시 시도하고 저장 오류 중에도 입력을 보존한다", async () => {
  await startApp();
  const view = await readDocument();
  button("mobile-edit").click();
  await vi.waitFor(() => expect(screen()).toBe("edit"));
  view.dispatch({ changes: { from: view.state.doc.length, insert: "Retry checkpoint\n" } });
  fakes.save.mockRejectedValueOnce(new Error("synthetic_background_failure"));
  expect(fakes.events.has("app-foreground-changed")).toBe(true);
  fakes.events.get("app-foreground-changed")!({ payload: false });
  await vi.waitFor(() => expect(document.body.textContent).toContain("synthetic_background_failure"));
  expect(view.state.doc.toString()).toBe(originalText + "Retry checkpoint\n");
  fakes.events.get("app-foreground-changed")!({ payload: true });
  await vi.waitFor(() => expect(fakes.save).toHaveBeenCalledTimes(2));
  expect(atob(fakes.save.mock.calls[1][0].bytes_base64)).toBe(originalText + "Retry checkpoint\n");
  expect(editor()).toBe(view);
});

test.each([true, false])("저장 대기 중 새 조합을 시작하면 Android 화면 이탈을 보류한다 (compact=%s)", async (compact) => {
  fakes.compact = compact;
  await startApp();
  const view = compact ? await readDocument() : editor();
  if (compact) {
    button("mobile-edit").click();
    await vi.waitFor(() => expect(screen()).toBe("edit"));
  }
  view.dispatch({ changes: { from: view.state.doc.length, insert: "Pending save\n" } });
  let finishSave!: (value: { edit_session_id: string; generation: string }) => void;
  fakes.save.mockImplementationOnce(() => new Promise((resolve) => { finishSave = resolve; }));
  systemBack();
  await vi.waitFor(() => expect(fakes.save).toHaveBeenCalledOnce());
  view.contentDOM.dispatchEvent(new CompositionEvent("compositionstart", { bubbles: true }));
  const request = fakes.save.mock.calls[0][0];
  finishSave({ edit_session_id: request.edit_session_id, generation: request.generation });
  await vi.waitFor(() => expect(document.body.textContent).toContain("한글 조합을 마친 뒤"));
  if (compact) expect(screen()).toBe("edit");
  expect(view.contentDOM.getAttribute("contenteditable")).toBe("true");
  expect(fakes.background).not.toHaveBeenCalled();
});

test("앱 복귀는 기기 저장을 마친 뒤 열린 문서의 최신 본문을 확인한다", async () => {
  fakes.workspace = true;
  fakes.openWorkspace.mockImplementation(async (node: NodeState) => ({
    binding, document_id: node.node_id, edit_session_id: `session-${node.node_id}`, generation: "0",
    bytes_base64: btoa(originalText), readonly: false, local_draft: false, has_session: true,
    cache_warning: null, open_notice: null,
  }));
  document.body.innerHTML = '<div id="root"></div>';
  await import("./main");
  await vi.waitFor(() => expect(document.querySelector<HTMLButtonElement>(`[data-document="${smallNode.node_id}"]`)?.disabled).toBe(false));
  document.querySelector<HTMLButtonElement>(`[data-document="${smallNode.node_id}"]`)!.click();
  await vi.waitFor(() => expect(screen()).toBe("read"));
  const view = editor();
  fakes.prepareRefresh.mockClear();
  fakes.events.get("app-foreground-changed")!({ payload: true });
  await vi.waitFor(() => expect(fakes.prepareRefresh).toHaveBeenCalled(), { timeout: 500 });
  expect(editor()).toBe(view);
  expect(fakes.save).not.toHaveBeenCalled();
});

test("편집 보조 도구는 편집 화면에서만 표시하고 선택과 포커스 및 저장 경로를 유지한다", async () => {
  await startApp();
  const view = await readDocument();
  expect(document.querySelector('[data-testid="markdown-heading"]')).toBeNull();
  button("mobile-edit").click();
  await vi.waitFor(() => expect(screen()).toBe("edit"));
  const from = originalText.indexOf("Original");
  view.dispatch({ selection: { anchor: from, head: from + 8 } });
  view.focus();
  const pointer = new MouseEvent("pointerdown", { bubbles: true, cancelable: true });
  button("markdown-link").dispatchEvent(pointer);
  expect(pointer.defaultPrevented).toBe(true);
  button("markdown-link").click();
  expect(view.state.doc.toString()).toBe(originalText.replace("Original", "[Original](https://)"));
  expect(view.state.sliceDoc(view.state.selection.main.from, view.state.selection.main.to)).toBe("https://");
  expect(view.hasFocus).toBe(true);
  document.querySelector<HTMLButtonElement>("#save")!.click();
  await vi.waitFor(() => expect(fakes.save).toHaveBeenCalled());
  expect(atob(fakes.save.mock.calls[0][0].bytes_base64)).toBe(view.state.doc.toString());
  expect(editor()).toBe(view);
  expect(undo(view)).toBe(true);
  expect(view.state.doc.toString()).toBe(originalText);
});

test("조합 중 보조 도구는 원문과 조합을 유지하며 삽입하지 않는다", async () => {
  await startApp();
  const view = await readDocument();
  button("mobile-edit").click();
  await vi.waitFor(() => expect(screen()).toBe("edit"));
  view.contentDOM.dispatchEvent(new CompositionEvent("compositionstart", { bubbles: true }));
  button("markdown-heading").click();
  await vi.waitFor(() => expect(document.body.textContent).toContain("한글 조합을 마친 뒤 편집 도구"));
  expect(view.state.doc.toString()).toBe(originalText);
  expect(fakes.save).not.toHaveBeenCalled();
});
