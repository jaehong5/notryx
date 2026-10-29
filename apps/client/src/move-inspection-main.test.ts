// @vitest-environment jsdom
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { createElement } from "react";
import { EditorView } from "@codemirror/view";
import type { WorkspaceRegistration } from "../../../packages/protocol-types";

const fakes = vi.hoisted(() => ({
  root: undefined as { unmount(): void } | undefined,
  listeners: new Map<string, () => void>(), active: undefined as WorkspaceRegistration | undefined,
  noNode: false, meta: '"original-meta"', queue: vi.fn(), status: vi.fn(), cancel: vi.fn(), kick: vi.fn(), send: vi.fn(),
  inventory: vi.fn(), prepare: vi.fn(), commit: vi.fn(), recover: vi.fn(),
  movePorts: undefined as undefined | { inspect?: (binding: WorkspaceRegistration, node: string, request: { expected_parent_id: string; parent_id: string; name: string }) => { status: string; isCurrent?: () => boolean } },
}));
vi.mock("./move-panel", async (original) => {
  const actual = await original<typeof import("./move-panel")>();
  return { ...actual, MovePanel: (props: Parameters<typeof actual.MovePanel>[0]) => { fakes.movePorts = props.ports; return createElement(actual.MovePanel, props); } };
});
vi.mock("react-dom/client", async (original) => {
  const actual = await original<typeof import("react-dom/client")>();
  return { ...actual, createRoot: (...args: Parameters<typeof actual.createRoot>) => { const root = actual.createRoot(...args); fakes.root = root; return root; } };
});
vi.mock("@tauri-apps/api/event", () => ({ listen: async (name: string, callback: () => void) => { fakes.listeners.set(name, callback); return () => fakes.listeners.delete(name); } }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
vi.mock("./migration-recovery", () => ({ StartupGate: ({ ready }: { ready: () => unknown }) => ready() }));
vi.mock("./auto-transmit", async (original) => { const actual = await original<typeof import("./auto-transmit")>(); return { ...actual, AutoTransmit: class extends actual.AutoTransmit { override kick() { fakes.kick(); super.kick(); } } }; });
vi.mock("./ipc", async (original) => {
  const actual = await original<typeof import("./ipc")>();
  return { ...actual, localDrafts: async () => [], pendingPairStatus: async () => "none",
    workspaceSyncState: async () => ({ binding: fakes.active, status: "ready" }),
    workspaceInventory: fakes.inventory,
    workspaceCreateParents: async () => ({ nodes: [rootNode, destination], next_after_id: null }),
    workspaceMoveStatus: fakes.status, queueWorkspaceMove: fakes.queue, requestWorkspaceMoveAbandon: fakes.cancel,
    openWorkspaceDocument: async () => ({ binding, document_id:nodeId, edit_session_id:"session", generation:"0", bytes_base64:btoa("[sibling](Sibling.md)"), readonly:false, local_draft:false, has_session:true, cache_warning:null, open_notice:null }),
    workspaceDocumentOutbox: async () => ({ binding:fakes.active, document_id:nodeId, first_content_conflict:null, has_conflict:false, has_failure:false, latest:null, confirmed_through:"0" }),
    prepareWorkspaceRemoteRefresh:fakes.prepare, commitWorkspaceRemoteRefresh:fakes.commit, inspectWorkspaceRemoteChange:async()=>"same", sendNextWorkspaceOperation:fakes.send, recoverWorkspaceSource:fakes.recover,
  };
});

const binding: WorkspaceRegistration = { server_origin:"https://example.invalid", workspace_id:"workspace", workspace_epoch:"epoch", device_id:"device", device_stream_id:"stream" };
const rootId = "019a1000-0000-7000-8000-000000000001", nodeId = "019a1000-0000-7000-8000-000000000002", destId = "019a1000-0000-7000-8000-000000000003", operationId = "019a1000-0000-7000-8000-000000000004";
const rootNode = { node_id:rootId, parent_id:null, name:"", kind:"folder" as const, deleted:false, content_rev:null, content_etag:null, meta_rev:"meta", meta_etag:'"meta"', node_etag:'"root"', content_hash:null, size:null, children_generation:"0", last_change_seq:"1" };
const destination = { ...rootNode, node_id:destId, parent_id:rootId, name:"destination" };
const node = { ...rootNode, node_id:nodeId, parent_id:rootId, name:"Synthetic.md", kind:"file" as const, content_rev:"content", content_etag:'"content"', content_hash:"sha256:" + "a".repeat(64), size:"1" };
const sibling = { ...node, node_id:"019a1000-0000-7000-8000-000000000005", name:"Sibling.md" };
const request = { expected_parent_id:rootId, parent_id:destId, name:"renamed.md" };
const entry = { operation_id:operationId, operation_seq:"1", node_id:nodeId, expected_meta_etag:'"original-meta"', request, status:"queued" as const, reason:null };

const rangeRects = Object.getOwnPropertyDescriptor(Range.prototype, "getClientRects");
const rangeBounds = Object.getOwnPropertyDescriptor(Range.prototype, "getBoundingClientRect");
beforeEach(() => {
  Object.defineProperty(Range.prototype, "getClientRects", { configurable:true, value:()=>[] });
  Object.defineProperty(Range.prototype, "getBoundingClientRect", { configurable:true, value:()=>({left:0,right:0,top:0,bottom:0,width:0,height:0}) });
  vi.resetModules(); fakes.listeners.clear(); fakes.active = binding; fakes.noNode = false; fakes.meta = '"original-meta"';
  fakes.queue.mockReset().mockResolvedValue(entry); fakes.status.mockReset().mockResolvedValue({ entries:[], next_after_seq:null });
  fakes.cancel.mockReset().mockResolvedValue(undefined); fakes.kick.mockReset();
  fakes.movePorts = undefined;
  fakes.inventory.mockReset().mockImplementation(async () => ({ nodes: fakes.noNode ? [rootNode, destination] : [rootNode, { ...node, meta_rev:fakes.meta.slice(1, -1), meta_etag:fakes.meta }, destination, sibling], next_after_id:null }));
  fakes.prepare.mockReset().mockResolvedValue(null); fakes.commit.mockReset().mockResolvedValue(true);
  fakes.recover.mockReset().mockResolvedValue(true);
  fakes.send.mockReset().mockResolvedValue({ binding, status:"idle", operation_id:null, document_id:null, generation:null });
});
afterEach(async () => { fakes.root?.unmount(); fakes.root = undefined; await new Promise((resolve) => setTimeout(resolve, 0)); document.body.replaceChildren(); vi.restoreAllMocks(); if (rangeRects) Object.defineProperty(Range.prototype,"getClientRects",rangeRects); else delete (Range.prototype as Partial<Range>).getClientRects; if (rangeBounds) Object.defineProperty(Range.prototype,"getBoundingClientRect",rangeBounds); else delete (Range.prototype as Partial<Range>).getBoundingClientRect; });
function change(element: HTMLInputElement | HTMLSelectElement, value: string) {
  const prototype = element instanceof HTMLInputElement ? HTMLInputElement.prototype : HTMLSelectElement.prototype;
  Object.getOwnPropertyDescriptor(prototype, "value")!.set!.call(element, value);
  element.dispatchEvent(new Event(element instanceof HTMLInputElement ? "input" : "change", { bubbles:true }));
}
async function openApp() {
  document.body.innerHTML = '<div id="root"></div>';
  await import("./main");
  await vi.waitFor(() => expect(document.querySelector('[data-testid="move-requests-toggle"]')).not.toBeNull());
}
async function openDocumentAndReview() {
  await openApp();
  await vi.waitFor(() => expect(document.querySelector(`[data-document="${nodeId}"]:not(:disabled)`)).not.toBeNull());
  document.querySelector<HTMLButtonElement>(`[data-document="${nodeId}"]`)!.click();
  await vi.waitFor(() => expect(document.querySelector('.cm-content[contenteditable=true]')).not.toBeNull());
  document.querySelector<HTMLButtonElement>(`[data-move-node="${nodeId}"]`)!.click();
  await vi.waitFor(() => expect(document.querySelector<HTMLSelectElement>('[data-testid="move-parent"]')?.options.length).toBeGreaterThan(1));
  change(document.querySelector<HTMLSelectElement>('[data-testid="move-parent"]')!, destId);
  change(document.querySelector<HTMLInputElement>('[data-testid="move-name"]')!, "renamed.md");
  document.querySelector<HTMLButtonElement>('[data-testid="move-prepare"]')!.click();
  await vi.waitFor(() => expect(document.querySelector('[data-testid="move-confirm"]')).not.toBeNull());
}

test("실제 열린 문서의 미리보기 링크만 검사하고 나머지 범위를 미검사로 표시한다", async () => {
  await openDocumentAndReview();
  const text = document.querySelector('[aria-label="문서 이름 변경 또는 이동"]')!.textContent;
  expect(text).toContain("열린 문서");
  expect(text).toContain("영향 가능 1");
  expect(text).toContain("다른 문서");
  expect(text).toContain("검사하지 않았습니다");
});

test("링크 검사 후 실제 편집기 generation이 바뀌면 확인으로 요청을 보낼 수 없다", async () => {
  await openDocumentAndReview();
  const element = document.querySelector<HTMLElement>('.cm-content')!;
  const view = EditorView.findFromDOM(element)!;
  view.dispatch({ changes:{ from:view.state.doc.length, insert:" updated" } });
  document.querySelector<HTMLButtonElement>('[data-testid="move-confirm"]')!.click();
  await vi.waitFor(() => expect(document.querySelector('[aria-label="문서 이름 변경 또는 이동"]')?.textContent).toMatch(/문서.*변경|다시 검사/));
  expect(fakes.queue).not.toHaveBeenCalled();
});

test("원격 본문 교체가 같은 session과 generation을 유지해도 이전 검사 snapshot은 무효다", async () => {
  await openDocumentAndReview();
  const inspection = fakes.movePorts!.inspect!(binding, nodeId, request);
  expect(inspection.status).toBe("checked");
  expect(inspection.isCurrent?.()).toBe(true);
  let release!: () => void;
  fakes.inventory.mockImplementation(() => new Promise((resolve) => { release = () => resolve({ nodes:[rootNode, node, destination, sibling], next_after_id:null }); }));
  fakes.prepare.mockResolvedValueOnce({ token:"remote-candidate", bytes_base64:btoa("remote replacement") });
  fakes.listeners.get("workspace-metadata-changed")?.();
  await vi.waitFor(() => expect(EditorView.findFromDOM(document.querySelector<HTMLElement>('.cm-content')!)?.state.doc.toString()).toBe("remote replacement"));
  expect(inspection.isCurrent?.()).toBe(false);
  release();
});

test("metadata 갱신 신호 뒤 새 목록을 받기 전에는 이전 트리로 검사 결과를 내지 않는다", async () => {
  await openDocumentAndReview();
  let release!: () => void;
  fakes.inventory.mockImplementation(() => new Promise((resolve) => { release = () => resolve({ nodes:[rootNode, node, destination, sibling], next_after_id:null }); }));
  fakes.listeners.get("workspace-metadata-changed")?.();
  const inspection = fakes.movePorts!.inspect!(binding, nodeId, request);
  expect(inspection.status).toBe("unavailable");
  release();
});

test("복사하는 동안 입력 없이 한글 조합만 시작해도 이후 입력이 있었다고 단정하지 않는다", async () => {
  let release!: (value: boolean) => void;
  fakes.recover.mockImplementation(() => new Promise<boolean>((resolve) => { release = resolve; }));
  await openDocumentAndReview();
  document.querySelector<HTMLButtonElement>("#copy-source")!.click();
  await vi.waitFor(() => expect(fakes.recover).toHaveBeenCalledOnce());
  document.querySelector<HTMLElement>('.cm-content')!.dispatchEvent(new CompositionEvent("compositionstart", { bubbles:true }));
  release(true);
  await vi.waitFor(() => expect(document.body.textContent).toContain("시작 당시 원문을 복사했습니다"));
  expect(document.body.textContent).not.toContain("이후 입력은 포함되지 않았습니다");
  expect(document.body.textContent).toContain("현재 문서 상태를 다시 확인하세요");
});
