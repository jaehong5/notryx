// @vitest-environment jsdom
import { EditorView } from "@codemirror/view";
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import type { AutoTransmit } from "./auto-transmit";
import type { WorkspaceRegistration } from "../../../packages/protocol-types";

const fakes = vi.hoisted(() => ({
  listeners: new Map<string, () => void>(),
  root: undefined as { unmount(): void } | undefined,
  scheduler: undefined as AutoTransmit | undefined,
  resumeContent: vi.fn(), kick: vi.fn(),
  syncStatus: "ready",
  choose: vi.fn(), resolution: vi.fn(), reopen: vi.fn(), continueChoice: vi.fn(), completeChoice: vi.fn(),
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
vi.mock("./auto-transmit", async (original) => {
  const actual = await original<typeof import("./auto-transmit")>();
  return { ...actual, AutoTransmit: class extends actual.AutoTransmit {
    constructor(ports: ConstructorParameters<typeof actual.AutoTransmit>[0]) { super(ports); fakes.scheduler = this; }
    resumeContentAfterAbandon(binding: string, operationId: string) {
      const result = super.resumeContentAfterAbandon(binding, operationId);
      fakes.resumeContent(binding, operationId, result);
      return result;
    }
    kick() { fakes.kick(); return super.kick(); }
  } };
});
vi.mock("./ipc", async (original) => {
  const actual = await original<typeof import("./ipc")>();
  return { ...actual,
    localDrafts: async () => [], pendingPairStatus: async () => "none",
    workspaceSyncState: async () => ({ binding:fakes.active, status: fakes.syncStatus }),
    workspaceInventory: async () => ({ nodes: fakes.deleted ? [rootNode] : [rootNode, node], next_after_id: null }),
    openWorkspaceDocument: async () => ({ binding, document_id: nodeId, edit_session_id: "open-session", generation: fakes.generation, bytes_base64: btoa("local"), readonly: false, local_draft: false, has_session: true, cache_warning: null, open_notice: null }),
    workspaceDocumentOutbox: fakes.outbox,
    prepareWorkspaceRemoteRefresh: fakes.prepare, commitWorkspaceRemoteRefresh: fakes.commit,
    inspectWorkspaceRemoteChange: fakes.inspect, pairWorkspace: fakes.pair,
    saveWorkspaceDocument: fakes.save, queueWorkspaceDocument: fakes.queue,
    sendNextWorkspaceOperation: fakes.transmit,
    closeWorkspaceDocument: fakes.close, recoverWorkspaceSource: fakes.recover, prepareWorkspaceContentConflict:fakes.prepareConflict,
    chooseWorkspaceRemoteContentConflict: fakes.choose, workspaceContentConflictResolutionStatus: fakes.resolution,
    reopenWorkspaceContentConflictCheckpoint: fakes.reopen, continueWorkspaceRemoteContentConflict: fakes.continueChoice, completeWorkspacePendingRemoteContentConflict: fakes.completeChoice,
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
  fakes.deleted = false; fakes.scheduler = undefined; fakes.syncStatus = "ready"; fakes.resumeContent.mockReset(); fakes.kick.mockReset();
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
  fakes.queue.mockReset().mockImplementation(async (document_id: string, _session: string, generation: string) => ({ operation_id: "019a1000-0000-7000-8000-000000000004", document_id, generation, committed: false }));
  fakes.choose.mockReset(); fakes.resolution.mockReset(); fakes.reopen.mockReset(); fakes.continueChoice.mockReset(); fakes.completeChoice.mockReset();
  fakes.transmit.mockReset().mockResolvedValue({ status: "idle", binding });
  fakes.pair.mockReset().mockResolvedValue(undefined);
});

afterEach(async () => {
  fakes.root?.unmount(); fakes.root = undefined; fakes.scheduler = undefined;
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

const operationId = snapshot.operation_id;
async function showComparison() {
  const view = await showConflictButton();
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-review-open"]')!.click();
  await vi.waitFor(() => expect(document.querySelector<HTMLTextAreaElement>('[aria-label="조회 당시 서버 본문"]')?.value).toBe("remote"));
  return view;
}

async function chooseRemote() {
  await vi.waitFor(() => expect(document.querySelector('[data-testid="conflict-choose-remote"]')).not.toBeNull());
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-choose-remote"]')!.click();
  await vi.waitFor(() => expect(document.querySelector('[data-testid="conflict-choose-remote-confirm"]')).not.toBeNull());
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-choose-remote-confirm"]')!.click();
}


async function pendingChoice() {
  fakes.choose.mockResolvedValue({ status: "pending", version: "0" });
  fakes.resolution.mockResolvedValue({ binding, document_id: nodeId, operation_id: operationId, state: { status: "not_found" } });
  await showComparison();
  await chooseRemote();
  await vi.waitFor(() => expect(fakes.choose).toHaveBeenCalledOnce());
  await vi.waitFor(() => expect(document.querySelector('[data-testid="conflict-continue"]')).not.toBeNull());
}

const pendingState = { status: "pending" as const, version: "0", generation: "1", choice: "choose_remote" as const, needs_review: false, frozen: false };
const statusResponse = (state: object) => ({ binding, document_id: nodeId, operation_id: operationId, state });
const reopenResponse = (state: typeof pendingState, text: string) => ({
  state, document: { binding, document_id: nodeId, edit_session_id: "reopened-session", generation: state.generation, bytes_base64: btoa(text), readonly: false, local_draft: true, has_session: true, cache_warning: null, open_notice: null },
});

async function completeChoice() {
  await vi.waitFor(() => expect(document.querySelector('[data-testid="conflict-complete"]')).not.toBeNull());
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-complete"]')!.click();
  await vi.waitFor(() => expect(fakes.completeChoice).toHaveBeenCalled());
}

test("결과 확인 후 인증이 필요한 복구 초안은 진행 버튼 대신 연결 상태를 안내한다", async () => {
  fakes.completeChoice.mockRejectedValue(new Error("pending_conflict_resolution"));
  const response = reopenResponse(pendingState, "local");
  fakes.reopen.mockResolvedValue({ ...response, document: { ...response.document, has_session: false } });
  await pendingChoice();
  fakes.resolution.mockResolvedValue(statusResponse(pendingState));
  await completeChoice();
  await vi.waitFor(() => expect(document.querySelector('[data-testid="conflict-recover-result"]')).not.toBeNull());
  const oldView = EditorView.findFromDOM(document.querySelector(".cm-editor")!)!;
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-recover-result"]')!.click();
  await vi.waitFor(() => expect(fakes.reopen).toHaveBeenCalledWith(binding, nodeId, operationId, "open-session"));
  await vi.waitFor(() => expect(EditorView.findFromDOM(document.querySelector(".cm-editor")!)).not.toBe(oldView));
  await vi.waitFor(() => expect(EditorView.findFromDOM(document.querySelector(".cm-editor")!)?.state.doc.toString()).toBe("local"));
  expect(document.querySelector('[data-testid="conflict-continue"]')).toBeNull();
  expect(document.querySelector('[data-testid="conflict-complete"]')).toBeNull();
  expect(document.body.textContent).toMatch(/인증|연결/);
  expect(fakes.continueChoice).not.toHaveBeenCalled();
  expect(fakes.queue).not.toHaveBeenCalled();
});

test("요청할 후속 작업이 없으면 새 abandon 요청을 했다고 표시하지 않는다", async () => {
  fakes.continueChoice.mockResolvedValue({ status: "pending", version: "0", requested_operation_id: null });
  await pendingChoice();
  fakes.kick.mockClear(); fakes.resumeContent.mockClear();
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-continue"]')!.click();
  await vi.waitFor(() => expect(fakes.continueChoice).toHaveBeenCalledOnce());
  await vi.waitFor(() => expect(document.body.textContent).toMatch(/후속.*없/));
  expect(document.body.textContent).not.toContain("후속 처리를 요청했습니다");
  expect(fakes.resumeContent).not.toHaveBeenCalled();
  expect(fakes.kick).not.toHaveBeenCalled();
});
