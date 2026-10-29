// @vitest-environment jsdom
import { EditorView } from "@codemirror/view";
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import type { WorkspaceRegistration } from "../../../packages/protocol-types";

const fakes = vi.hoisted(() => ({
  listeners: new Map<string, () => void>(),
  root: undefined as { unmount(): void } | undefined,
  prepareConflict: vi.fn(), prepare: vi.fn(), commit: vi.fn(), inspect: vi.fn(), save: vi.fn(), queue: vi.fn(), transmit: vi.fn(), pair: vi.fn(), recover: vi.fn(), outbox: vi.fn(), generation:"0", deleted: false, close:vi.fn(), active:undefined as WorkspaceRegistration | undefined,
}));
vi.mock("react-dom/client", async (original) => {
  const actual = await original<typeof import("react-dom/client")>();
  return { ...actual, createRoot: (...args: Parameters<typeof actual.createRoot>) => { const root = actual.createRoot(...args); fakes.root = root; return root; } };
});
vi.mock("@tauri-apps/api/event", () => ({ listen: async (name: string, callback: () => void) => {
  fakes.listeners.set(name, callback);
  return () => { fakes.listeners.delete(name); };
} }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
vi.mock("./migration-recovery", () => ({ StartupGate: ({ ready }: { ready: () => unknown }) => ready() }));
vi.mock("./ipc", async (original) => {
  const actual = await original<typeof import("./ipc")>();
  return { ...actual,
    localDrafts: async () => [], pendingPairStatus: async () => "none",
    workspaceSyncState: async () => ({ binding:fakes.active, status: "ready" }),
    workspaceInventory: async () => ({ nodes: fakes.deleted ? [rootNode] : [rootNode, node], next_after_id: null }),
    openWorkspaceDocument: async () => ({ binding, document_id: nodeId, edit_session_id: "open-session", generation: fakes.generation, bytes_base64: btoa("base"), readonly: false, local_draft: false, has_session: true, cache_warning: null, open_notice: null }),
    workspaceDocumentOutbox: fakes.outbox,
    prepareWorkspaceRemoteRefresh: fakes.prepare, commitWorkspaceRemoteRefresh: fakes.commit,
    inspectWorkspaceRemoteChange: fakes.inspect, pairWorkspace: fakes.pair,
    saveWorkspaceDocument: fakes.save, queueWorkspaceDocument: fakes.queue,
    sendNextWorkspaceOperation: fakes.transmit,
    closeWorkspaceDocument: fakes.close, recoverWorkspaceSource: fakes.recover, prepareWorkspaceContentConflict:fakes.prepareConflict,
  };
});

const binding = { server_origin: "https://example.invalid", workspace_id: "workspace", workspace_epoch: "epoch", device_id: "device", device_stream_id: "stream" };
const rootId = "019a1000-0000-7000-8000-000000000001", nodeId = "019a1000-0000-7000-8000-000000000002";
const rootNode = { node_id: rootId, parent_id: null, name: "", kind: "folder" as const, deleted: false, content_rev: null, content_etag: null, meta_rev: "meta", meta_etag: '"meta"', node_etag: '"node"', content_hash: null, size: null, children_generation: "0", last_change_seq: "1" };
const node = { ...rootNode, node_id: nodeId, parent_id: rootId, name: "synthetic.md", kind: "file" as const, content_rev: "content", content_etag: '"content"', content_hash: "sha256:" + "a".repeat(64), size: "4" };
const snapshot = { binding, node_id:nodeId, operation_id:"019a1000-0000-7000-8000-000000000003", operation_seq:"1", generation:"1", base_revision:"base-rev", base_bytes:new TextEncoder().encode("base"), base_hash:"sha256:cae662172fd450bb0cd710a769079c05bfc5d8e35efa6576edc7d0377afdd4a2", local_bytes:new TextEncoder().encode("local"), local_hash:"sha256:25bf8e1a2393f1108d37029b3df5593236c755742ec93465bbafa9b290bddcf6", conflict_revision:"revision-at-412", remote_revision:"current-rev", remote_bytes:new TextEncoder().encode("remote"), remote_hash:"sha256:b71199ebd070b36beab7317920c2c2f1d777df8d05e5527d8458fda57cb17a7a" };
const rangeRects = Object.getOwnPropertyDescriptor(Range.prototype, "getClientRects");
const rangeBounds = Object.getOwnPropertyDescriptor(Range.prototype, "getBoundingClientRect");

beforeEach(() => {
  Object.defineProperty(Range.prototype, "getClientRects", { configurable: true, value: () => [] });
  Object.defineProperty(Range.prototype, "getBoundingClientRect", { configurable: true, value: () => ({ left: 0, right: 0, top: 0, bottom: 0, width: 0, height: 0 }) });
  vi.resetModules();
  fakes.deleted = false;
  fakes.active = binding;
  fakes.generation = "1";
  fakes.outbox.mockReset().mockResolvedValue({ binding, document_id:nodeId, first_content_conflict:{ operation_id:snapshot.operation_id, operation_seq:snapshot.operation_seq }, has_conflict:true, has_failure:false, latest:null, confirmed_through:"0" });
  fakes.close.mockReset().mockResolvedValue(undefined);
  fakes.recover.mockReset().mockResolvedValue(true);
  fakes.prepareConflict.mockReset().mockResolvedValue(snapshot);
  fakes.prepare.mockReset().mockResolvedValue(null);
  fakes.commit.mockReset().mockResolvedValue(true);
  fakes.inspect.mockReset().mockResolvedValue("same");
  fakes.save.mockReset().mockImplementation(async (request: { edit_session_id: string; generation: string }) => ({ edit_session_id: request.edit_session_id, generation: request.generation }));
  fakes.queue.mockReset();
  fakes.transmit.mockReset().mockResolvedValue({ status: "idle", binding });
  fakes.pair.mockReset().mockResolvedValue(undefined);
});

afterEach(async () => {
  fakes.root?.unmount(); fakes.root = undefined;
  await new Promise((resolve) => setTimeout(resolve, 0));
  fakes.listeners.clear(); document.body.replaceChildren();
  if (rangeRects) Object.defineProperty(Range.prototype, "getClientRects", rangeRects);
  else delete (Range.prototype as Partial<Range>).getClientRects;
  if (rangeBounds) Object.defineProperty(Range.prototype, "getBoundingClientRect", rangeBounds);
  else delete (Range.prototype as Partial<Range>).getBoundingClientRect;
});

async function openEditor(expectPristineRefresh = true): Promise<EditorView> {
  document.body.innerHTML = '<div id="root"></div>';
  await import("./main");
  await vi.waitFor(() => expect(document.querySelector(`[data-document="${nodeId}"]`)).not.toBeNull());
  document.querySelector<HTMLButtonElement>(`[data-document="${nodeId}"]`)!.click();
  await vi.waitFor(() => expect(document.querySelector(".cm-editor")).not.toBeNull());
  if (expectPristineRefresh) await vi.waitFor(() => expect(fakes.prepare).toHaveBeenCalled());
  fakes.prepare.mockClear();
  return EditorView.findFromDOM(document.querySelector(".cm-editor")!)!;
}



async function showConflictButton(): Promise<EditorView> {
  const view = await openEditor(false);
  await vi.waitFor(() => expect(document.querySelector('[data-testid="conflict-review-open"]')).not.toBeNull());
  return view;
}

test("명시적 비교 요청은 현재 충돌의 기준·저장된 초안·조회 당시 서버 본문을 구분한다", async () => {
  const view = await showConflictButton();
  expect(fakes.prepareConflict).not.toHaveBeenCalled();
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-review-open"]')!.click();
  await vi.waitFor(() => expect(document.querySelector<HTMLTextAreaElement>('[aria-label="조회 당시 서버 본문"]')?.value).toBe("remote"));
  expect(fakes.prepareConflict).toHaveBeenCalledExactlyOnceWith(binding, nodeId, "open-session", "1");
  const panel = document.querySelector('[aria-label="충돌 원문 비교"]')!;
  expect(panel.textContent).toContain("서버 본문은 충돌이 발생한 뒤 달라졌을 수 있습니다.");
  expect(panel.textContent).not.toContain("revision-at-412"); expect(panel.textContent).not.toContain("current-rev");
  for (const [label, value] of [["기준 원문", "base"], ["이 기기에 저장된 내 초안", "local"], ["조회 당시 서버 본문", "remote"]]) {
    const field = panel.querySelector<HTMLTextAreaElement>(`[aria-label="${label}"]`);
    expect(field?.value).toBe(value); expect(field?.readOnly).toBe(true);
  }
  expect(view.state.doc.toString()).toBe("base"); expect(fakes.queue).not.toHaveBeenCalled(); expect(fakes.close).not.toHaveBeenCalled();
});

test("미저장 입력은 기기 checkpoint 완료 뒤에만 같은 generation으로 비교한다", async () => {
  const view = await showConflictButton();
  view.dispatch({ changes:{ from:view.state.doc.length, insert:" edited" }, userEvent:"input.type" });
  let release!: () => void;
  fakes.save.mockImplementation((request:{edit_session_id:string;generation:string}) => new Promise((resolve) => { release = () => resolve({ edit_session_id:request.edit_session_id, generation:request.generation }); }));
  fakes.prepareConflict.mockResolvedValue({ ...snapshot, generation:"2", local_bytes:new TextEncoder().encode("base edited") });
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-review-open"]')!.click();
  await vi.waitFor(() => expect(fakes.save).toHaveBeenCalledOnce());
  expect(fakes.prepareConflict).not.toHaveBeenCalled(); expect(fakes.queue).not.toHaveBeenCalled();
  release();
  await vi.waitFor(() => expect(fakes.prepareConflict).toHaveBeenCalledExactlyOnceWith(binding, nodeId, "open-session", "2"));
  await vi.waitFor(() => expect(document.querySelector<HTMLTextAreaElement>('[aria-label="이 기기에 저장된 내 초안"]')?.value).toBe("base edited"));
  expect(view.state.doc.toString()).toBe("base edited");
});

test("checkpoint 저장이 실패하면 비교를 시작하지 않고 현재 입력을 유지한다", async () => {
  const view = await showConflictButton();
  view.dispatch({ changes:{ from:view.state.doc.length, insert:" edited" }, userEvent:"input.type" });
  fakes.save.mockRejectedValue(new Error("synthetic failure"));
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-review-open"]')!.click();
  await vi.waitFor(() => expect(fakes.save).toHaveBeenCalled());
  expect(fakes.prepareConflict).not.toHaveBeenCalled(); expect(view.state.doc.toString()).toBe("base edited");
  expect(fakes.close).not.toHaveBeenCalled();
});

test("비교 응답을 기다리는 동안 새 입력이 생기면 이전 generation의 결과를 표시하지 않는다", async () => {
  const view = await showConflictButton();
  let release!: () => void;
  fakes.prepareConflict.mockImplementation(() => new Promise((resolve) => { release = () => resolve(snapshot); }));
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-review-open"]')!.click();
  await vi.waitFor(() => expect(fakes.prepareConflict).toHaveBeenCalledOnce());
  view.dispatch({ changes:{ from:view.state.doc.length, insert:" later" }, userEvent:"input.type" });
  release();
  await vi.waitFor(() => expect(document.body.textContent).toMatch(/변경|다시/));
  expect(document.querySelector('[aria-label="조회 당시 서버 본문"]')).toBeNull();
  expect(view.state.doc.toString()).toBe("base later"); expect(fakes.close).not.toHaveBeenCalled();
});

test("한글 조합 중 비교 요청은 조합을 끝내거나 서버 비교를 시작하지 않는다", async () => {
  const view = await showConflictButton();
  document.querySelector<HTMLElement>('.cm-content')!.dispatchEvent(new CompositionEvent("compositionstart", { bubbles:true }));
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-review-open"]')!.click();
  await vi.waitFor(() => expect(document.body.textContent).toContain("조합"));
  expect(fakes.prepareConflict).not.toHaveBeenCalled(); expect(fakes.save).not.toHaveBeenCalled(); expect(fakes.close).not.toHaveBeenCalled();
  expect(view.state.doc.toString()).toBe("base");
});

test("등록 binding이 교체되면 이전 등록의 충돌 비교 원문을 닫는다", async () => {
  await showConflictButton();
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-review-open"]')!.click();
  await vi.waitFor(() => expect(document.querySelector<HTMLTextAreaElement>('[aria-label="조회 당시 서버 본문"]')?.value).toBe("remote"));
  fakes.active = { ...binding, workspace_epoch:"new-epoch" };
  fakes.listeners.get("workspace-sync-status-changed")?.();
  await vi.waitFor(() => expect(document.querySelector('[aria-label="충돌 원문 비교"]')).toBeNull());
  expect(document.querySelector('[aria-label="기준 원문"]')).toBeNull();
});
