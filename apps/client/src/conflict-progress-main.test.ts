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
  fakes.resolution.mockResolvedValue({ binding, document_id: nodeId, operation_id: snapshot.operation_id, state: { status: "not_found" } });
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

const blockedOperation = "019a1000-0000-7000-8000-000000000004";
const otherOperation = "019a1000-0000-7000-8000-000000000005";
const blocked = { binding, status: "needs_review", operation_id: blockedOperation, document_id: nodeId, generation: "2",
  content_block: { conflict_operation_id: operationId, blocked_operation_id: blockedOperation, blocked_seq: "2" } };

async function pendingChoice() {
  fakes.choose.mockResolvedValue({ status: "pending", version: "0" });
  await showComparison();
  await chooseRemote();
  await vi.waitFor(() => expect(fakes.choose).toHaveBeenCalledOnce());
  await vi.waitFor(() => expect(document.querySelector('[data-testid="conflict-continue"]')).not.toBeNull());
}

async function continueChoice() {
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-continue"]')!.click();
  await vi.waitFor(() => expect(fakes.continueChoice).toHaveBeenCalledOnce());
}

test("pending 선택은 사용자가 후속 처리를 요청할 때만 동일한 차단 작업을 재개한다", async () => {
  fakes.transmit.mockResolvedValueOnce(blocked).mockResolvedValue({ binding, status: "idle", operation_id: null, document_id: null, generation: null, content_block: null });
  fakes.continueChoice.mockResolvedValue({ status: "pending", version: "0", requested_operation_id: blockedOperation });
  await pendingChoice();
  fakes.scheduler!.kick();
  await vi.waitFor(() => expect(fakes.transmit).toHaveBeenCalled(), { timeout: 2500 });
  expect(fakes.continueChoice).not.toHaveBeenCalled();
  await continueChoice();
  expect(fakes.continueChoice).toHaveBeenCalledWith(binding, nodeId, "open-session", operationId, "0");
  expect(fakes.resumeContent).toHaveBeenCalledWith(JSON.stringify(binding), blockedOperation, true);
  await vi.waitFor(() => expect(fakes.transmit.mock.calls.length).toBeGreaterThan(1), { timeout: 2500 });
  expect(fakes.completeChoice).not.toHaveBeenCalled();
});

test("요청된 abandon ID가 차단된 작업과 다르면 kick해도 차단을 해제하지 않는다", async () => {
  fakes.transmit.mockResolvedValueOnce(blocked).mockResolvedValue({ binding, status: "idle", operation_id: null, document_id: null, generation: null, content_block: null });
  fakes.continueChoice.mockResolvedValue({ status: "pending", version: "0", requested_operation_id: otherOperation });
  await pendingChoice();
  fakes.scheduler!.kick();
  await vi.waitFor(() => expect(fakes.transmit).toHaveBeenCalled(), { timeout: 2500 });
  expect(fakes.transmit).toHaveBeenCalledOnce();
  await continueChoice();
  expect(fakes.resumeContent).toHaveBeenCalledWith(JSON.stringify(binding), otherOperation, false);
  fakes.scheduler!.kick();
  await new Promise((resolve) => setTimeout(resolve, 20));
  expect(fakes.transmit).toHaveBeenCalledOnce();
});

test("전송기가 유휴 상태면 명시 진행한 작업을 kick으로 확인한다", async () => {
  fakes.continueChoice.mockResolvedValue({ status: "pending", version: "0", requested_operation_id: blockedOperation });
  await pendingChoice();
  fakes.scheduler!.kick();
  await vi.waitFor(() => expect(fakes.transmit).toHaveBeenCalled(), { timeout: 2500 });
  fakes.transmit.mockClear(); fakes.kick.mockClear();
  await continueChoice();
  expect(fakes.resumeContent).toHaveBeenCalledWith(JSON.stringify(binding), blockedOperation, false);
  expect(fakes.kick).toHaveBeenCalled();
  await vi.waitFor(() => expect(fakes.transmit).toHaveBeenCalled(), { timeout: 2500 });
});

test("인증 차단은 다른 작업의 명시 진행이나 kick으로 해제되지 않는다", async () => {
  fakes.transmit.mockResolvedValueOnce({ binding, status: "auth_required", operation_id: null, document_id: null, generation: null, content_block: null });
  fakes.continueChoice.mockResolvedValue({ status: "pending", version: "0", requested_operation_id: blockedOperation });
  await pendingChoice();
  fakes.scheduler!.kick();
  await vi.waitFor(() => expect(fakes.transmit).toHaveBeenCalledOnce(), { timeout: 2500 });
  await continueChoice();
  expect(fakes.resumeContent).toHaveBeenCalledWith(JSON.stringify(binding), blockedOperation, false);
  fakes.scheduler!.kick();
  await new Promise((resolve) => setTimeout(resolve, 20));
  expect(fakes.transmit).toHaveBeenCalledOnce();
});

test("후속 진행 응답 중 등록이 교체되면 이전 등록의 전송 차단을 해제하지 않는다", async () => {
  let release!: (value: { status: "pending"; version: string; requested_operation_id: string }) => void;
  fakes.continueChoice.mockImplementation(() => new Promise((resolve) => { release = resolve; }));
  await pendingChoice();
  document.querySelector<HTMLButtonElement>('[data-testid="conflict-continue"]')!.click();
  await vi.waitFor(() => expect(fakes.continueChoice).toHaveBeenCalledOnce());
  fakes.active = { ...binding, workspace_epoch: "new-epoch" };
  fakes.syncStatus = "connecting";
  fakes.listeners.get("workspace-sync-status-changed")?.();
  await vi.waitFor(() => expect(document.querySelector("#workspace-sync-status")?.textContent).toContain("서버 동기화 중"));
  release({ status: "pending", version: "0", requested_operation_id: blockedOperation });
  await new Promise((resolve) => setTimeout(resolve, 20));
  expect(fakes.resumeContent).not.toHaveBeenCalledWith(JSON.stringify(binding), blockedOperation, expect.anything());
});
