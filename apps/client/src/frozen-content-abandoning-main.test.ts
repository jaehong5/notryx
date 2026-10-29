// @vitest-environment jsdom
import { act, createElement } from "react";
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import type { WorkspaceOutboxEntry, WorkspaceRegistration } from "../../../packages/protocol-types";
import type { FrozenContentReviewItem, FrozenContentReviewPorts, FrozenContentReviewSnapshot } from "./frozen-content-review";

const fakes = vi.hoisted(() => ({
  root: undefined as { unmount(): void } | undefined,
  drafts: vi.fn(), outbox: vi.fn(), inspect: vi.fn(), queue: vi.fn(), abandon: vi.fn(), dismiss: vi.fn(), send: vi.fn(), kick: vi.fn(),
}));
vi.mock("react-dom/client", async (original) => {
  const actual = await original<typeof import("react-dom/client")>();
  return { ...actual, createRoot: (...args: Parameters<typeof actual.createRoot>) => { const root = actual.createRoot(...args); fakes.root = root; return root; } };
});
vi.mock("@tauri-apps/api/event", () => ({ listen: async () => () => {} }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
vi.mock("./migration-recovery", () => ({ StartupGate: ({ ready }: { ready: () => unknown }) => ready() }));
vi.mock("./auto-transmit", async (original) => {
  const actual = await original<typeof import("./auto-transmit")>();
  return { ...actual, AutoTransmit: class extends actual.AutoTransmit { override kick() { fakes.kick(); super.kick(); } } };
});
vi.mock("./ipc", async (original) => {
  const actual = await original<typeof import("./ipc")>();
  return { ...actual, localDrafts: fakes.drafts, workspaceOutbox: fakes.outbox, workspaceInspectFrozenContent: fakes.inspect,
    workspaceFrozenContentDecisionStatus: async () => null,
    workspaceDismissFrozenContent: fakes.dismiss,
    pendingPairStatus: async () => "none", workspaceSyncState: async () => ({ binding: current, status: "ready" }),
    workspaceInventory: async () => ({ nodes: [rootNode], next_after_id: null }),
    queueWorkspaceDocument: fakes.queue, queueWorkspaceRestore: fakes.queue, queueWorkspaceCreate: fakes.queue,
    queueWorkspaceDelete: fakes.queue, queueWorkspaceMove: fakes.queue,
    requestWorkspaceRestoreAbandon: fakes.abandon, requestWorkspaceCreateAbandon: fakes.abandon,
    requestWorkspaceDeleteAbandon: fakes.abandon, requestWorkspaceMoveAbandon: fakes.abandon,
    requestWorkspaceContentConflictMergeFollowupAbandon: fakes.abandon, requestWorkspaceDeletedDraftCopyFollowupAbandon: fakes.abandon,
    sendNextWorkspaceOperation: fakes.send,
  };
});

const current: WorkspaceRegistration = { server_origin: "https://example.invalid", workspace_id: "workspace", workspace_epoch: "current-epoch", device_id: "device", device_stream_id: "stream" };
const old: WorkspaceRegistration = { ...current, device_id: "old-device", device_stream_id: "old-stream" };
const rootId = "019a1000-0000-7000-8000-000000000001", documentId = "019a1000-0000-7000-8000-000000000002", operationId = "019a1000-0000-7000-8000-000000000003";
const rootNode = { node_id: rootId, parent_id: null, name: "", kind: "folder" as const, deleted: false, content_rev: null, content_etag: null, meta_rev: "meta", meta_etag: '"meta"', node_etag: '"node"', content_hash: null, size: null, children_generation: "0", last_change_seq: "1" };
const bytes = (text: string) => new TextEncoder().encode(text);
const operation = (abandon_state: "requested" | "sent"): WorkspaceOutboxEntry => ({ operation_id: operationId, operation_seq: "1", document_id: documentId, generation: "1", status: "abandoning", frozen: true, abandon_state, failure: null, payload_retired: false });
const inspection = { current_binding: current, old_binding: old, document_id: documentId, operation_id: operationId, operation_seq: "1", operation_status: "abandoning", abandon_state: "sent", operation_generation: "1", local_generation: "2", expected_revision: null, outcome_unknown: false, base_revision: "base", remote_revision: "remote", operation_bytes: bytes("payload"), base_bytes: bytes("base"), local_bytes: bytes("local draft"), remote_bytes: bytes("server") };
const item = { old_binding: old, operation_id: operationId, operation_seq: "1", document_id: documentId, generation: "1", status: "abandoning", abandon_state: "sent" } as unknown as FrozenContentReviewItem;
const snapshot = { binding: current, ...item, expected_revision: null, outcome_unknown: false, base_revision: "base", remote_revision: "remote", operation_bytes: bytes("payload"), base_bytes: bytes("base"), local_bytes: bytes("local draft"), remote_bytes: bytes("server") } as FrozenContentReviewSnapshot;

beforeEach(() => {
  vi.resetModules(); vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  fakes.drafts.mockReset().mockResolvedValue([{ binding: old, document_id: documentId, generation: "2", current_registration: false, has_session: false }]);
  fakes.outbox.mockReset(); fakes.inspect.mockReset().mockResolvedValue(inspection);
  fakes.queue.mockReset(); fakes.abandon.mockReset(); fakes.dismiss.mockReset(); fakes.kick.mockReset();
  fakes.send.mockReset().mockResolvedValue({ status: "idle", binding: current });
});
afterEach(async () => {
  await act(async () => { fakes.root?.unmount(); }); fakes.root = undefined;
  await new Promise((resolve) => setTimeout(resolve, 0));
  document.body.replaceChildren(); vi.unstubAllGlobals(); vi.restoreAllMocks();
});

function checkNoMutation() {
  expect(fakes.queue).not.toHaveBeenCalled(); expect(fakes.abandon).not.toHaveBeenCalled();
  expect(fakes.dismiss).not.toHaveBeenCalled();
  expect(fakes.send).not.toHaveBeenCalled(); expect(fakes.kick).not.toHaveBeenCalled();
}

test.each(["requested", "sent"] as const)("같은 epoch의 이전 등록에서 취소 %s 작업을 대조하고 승인 전에는 변경하지 않는다", async (abandonState) => {
  fakes.outbox.mockResolvedValue({ entries: [operation(abandonState)], next_after_seq: null });
  fakes.inspect.mockResolvedValue({ ...inspection, abandon_state: abandonState });
  document.body.innerHTML = '<div id="root"></div>';
  await act(async () => { await import("./main"); });
  await vi.waitFor(() => expect([...document.querySelectorAll("button")].some((button) => button.textContent === "이전 본문 요청 대조")).toBe(true));
  fakes.queue.mockClear(); fakes.abandon.mockClear(); fakes.send.mockClear(); fakes.kick.mockClear();
  const entry = [...document.querySelectorAll("button")].find((button) => button.textContent === "이전 본문 요청 대조")!;
  await act(async () => { entry.click(); });
  await vi.waitFor(() => expect(document.querySelector(`[data-review-operation="${operationId}"]`)).not.toBeNull());
  const button = document.querySelector<HTMLButtonElement>(`[data-review-operation="${operationId}"]`)!;
  expect(button.textContent).toMatch(/취소.*(결과|처리).*(미확정|불명|알 수 없)/s);
  expect(button.textContent).not.toMatch(/취소.*완료|전송 대기/s);
  expect(fakes.inspect).not.toHaveBeenCalled(); checkNoMutation();
  await act(async () => { button.click(); });
  await vi.waitFor(() => expect(document.querySelector<HTMLTextAreaElement>('[data-testid="frozen-content-payload"] textarea')?.value).toBe("payload"));
  expect(fakes.inspect).toHaveBeenCalledExactlyOnceWith(current, old, operationId, documentId);
  const review = document.querySelector<HTMLElement>('section[aria-label="이전 본문 요청 대조"]')!;
  expect(review.textContent).toMatch(/취소.*(결과|처리).*(미확정|불명|알 수 없)/s);
  expect(review.textContent).not.toMatch(/취소.{0,20}완료/s);
  if (abandonState === "sent") {
    expect(review.textContent).toContain("취소 요청의 전송 또는 처리 결과가 미확정입니다.");
    expect(review.textContent).not.toContain("취소 요청을 전송했습니다.");
  }
  for (const [id, value] of [["payload", "payload"], ["base", "base"], ["draft", "local draft"], ["remote", "server"]] as const) {
    const field = review.querySelector<HTMLTextAreaElement>(`[data-testid="frozen-content-${id}"] textarea`);
    expect(field?.value).toBe(value); expect(field?.readOnly).toBe(true);
  }
  expect(review.querySelector('[data-testid="frozen-content-apply"]')).toBeNull();
  const dismiss = review.querySelector<HTMLButtonElement>('[data-testid="frozen-content-dismiss"]');
  expect(dismiss).not.toBeNull(); expect(dismiss!.disabled).toBe(true);
  await act(async () => { dismiss!.click(); });
  expect(review.querySelector('[data-testid="frozen-content-send"]')).toBeNull();
  checkNoMutation();
});

test("취소 결과 미확정 상세를 독립 비교 화면에서 읽기 전용으로 표시한다", async () => {
  const { createRoot } = await import("react-dom/client");
  const { FrozenContentReview } = await import("./frozen-content-review");
  const api: FrozenContentReviewPorts = { list: vi.fn(async (binding) => ({ binding, entries: [item], next_after_id: null })), load: vi.fn(async () => snapshot) };
  const container = document.createElement("div"); document.body.append(container); const root = createRoot(container);
  await act(async () => { root.render(createElement(FrozenContentReview, { binding: current, ports: api })); });
  await vi.waitFor(() => expect(container.querySelector(`[data-review-operation="${operationId}"]`)).not.toBeNull());
  expect(container.querySelector(`[data-review-operation="${operationId}"]`)?.textContent).toMatch(/취소.*(결과|처리).*(미확정|불명|알 수 없)/s);
  await act(async () => { container.querySelector<HTMLButtonElement>(`[data-review-operation="${operationId}"]`)!.click(); });
  await vi.waitFor(() => expect(container.querySelector<HTMLTextAreaElement>('[data-testid="frozen-content-payload"] textarea')?.value).toBe("payload"));
  expect(container.textContent).toMatch(/취소.*(결과|처리).*(미확정|불명|알 수 없)/s);
  expect(container.textContent).not.toMatch(/취소.{0,20}완료/s);
  expect(container.querySelector('[data-testid="frozen-content-apply"]')).toBeNull();
  expect(container.querySelector('[data-testid="frozen-content-dismiss"]')).toBeNull();
  expect(container.querySelector('[data-testid="frozen-content-send"]')).toBeNull();
  checkNoMutation();
});
