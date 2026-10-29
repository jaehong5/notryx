// @vitest-environment jsdom
import { act, createElement } from "react";
import { createRoot } from "react-dom/client";
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { FrozenContentReview, type FrozenContentReviewItem, type FrozenContentReviewPorts, type FrozenContentReviewSnapshot } from "./frozen-content-review";
import type { WorkspaceOutboxStatus, WorkspaceRegistration } from "../../../packages/protocol-types";

const current: WorkspaceRegistration = { server_origin: "https://example.invalid", workspace_id: "workspace", workspace_epoch: "epoch", device_id: "current-device", device_stream_id: "current-stream" };
const old: WorkspaceRegistration = { ...current, device_id: "old-device", device_stream_id: "old-stream" };
const item: FrozenContentReviewItem = { old_binding: old, operation_id: "old-operation", operation_seq: "1", document_id: "document", generation: "1", expected_revision: "base-rev", status: "sent", abandon_state: "none" };
const bytes = (text: string) => new TextEncoder().encode(text);
const snapshot: FrozenContentReviewSnapshot = {
  ...item, binding: current, expected_revision: "base-rev", local_generation: "2", outcome_unknown: true,
  base_revision: "base-rev", remote_revision: "remote-rev", operation_bytes: bytes("old payload\n"),
  base_bytes: bytes("old base\n"), local_bytes: bytes("latest draft\n"), remote_bytes: bytes("remote content\n"),
};
type Recorded = { binding: WorkspaceRegistration; old_binding: WorkspaceRegistration; document_id: string; old_operation_id: string; operation_id: string; operation_seq: string; status: WorkspaceOutboxStatus; approved_generation: string; approved_hash: string };
type StatusPorts = FrozenContentReviewPorts & {
  status: (current: WorkspaceRegistration, previous: WorkspaceRegistration, document: string, operation: string) => Promise<Recorded | null>;
};
const recorded: Recorded = { binding: current, old_binding: old, document_id: item.document_id, old_operation_id: item.operation_id, operation_id: "new-operation", operation_seq: "7", status: "queued", approved_generation: "2", approved_hash: "sha256:" + "a".repeat(64) };
function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((done) => { resolve = done; });
  return { promise, resolve };
}
function ports(): StatusPorts {
  return {
    list: vi.fn(async (binding) => ({ binding, entries: [item], next_after_id: null })),
    load: vi.fn(async () => snapshot),
    apply: vi.fn(async () => ({ binding: current, operation_id: "new-operation" })),
    status: vi.fn(async () => recorded),
  };
}
async function mount(api: StatusPorts) {
  const container = document.createElement("div"); document.body.append(container); const root = createRoot(container);
  await act(async () => { root.render(createElement(FrozenContentReview, { binding: current, ports: api })); });
  await vi.waitFor(() => expect(container.querySelector(`[data-review-operation="${item.operation_id}"]`)).not.toBeNull());
  return { container, root };
}
async function select(container: HTMLElement) {
  await act(async () => { container.querySelector<HTMLButtonElement>(`[data-review-operation="${item.operation_id}"]`)!.click(); });
}

beforeEach(() => vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true));
afterEach(() => { document.body.replaceChildren(); vi.unstubAllGlobals(); });

test.each([
  ["queued", /전송 대기/], ["sent", /전송 중|결과.*미확정/], ["committed", /서버 반영|서버 저장/],
  ["conflict", /충돌/], ["abandoning", /취소.*미확정|취소.*진행/], ["abandoned", /취소|포기/], ["failed", /실패|거절/],
] as const)("재시작 후 기록된 %s 상태를 서버 대조 없이 조회한다", async (status, wording) => {
  const api = ports(); api.status = vi.fn(async () => ({ ...recorded, status }));
  api.load = vi.fn(async () => { throw new Error("offline"); });
  const { container, root } = await mount(api); await select(container);
  await vi.waitFor(() => expect(container.textContent).toContain("new-operation"));
  expect(api.status).toHaveBeenCalledExactlyOnceWith(current, old, item.document_id, item.operation_id);
  expect(api.load).not.toHaveBeenCalled(); expect(api.apply).not.toHaveBeenCalled();
  expect(container.textContent).toContain("7"); expect(container.textContent).toContain(recorded.approved_hash);
  expect(container.textContent).toMatch(/초안/); expect(container.textContent).toMatch(wording);
  expect(container.textContent).not.toContain("remote content");
  expect(container.textContent).not.toMatch(/이전.{0,20}요청.{0,20}(서버 반영|성공|완료)/s);
  expect(container.querySelector('[data-testid="frozen-content-select-draft"]')).toBeNull();
  expect(container.querySelector('[data-testid="frozen-content-apply"]')).toBeNull();
  await act(async () => { root.unmount(); });
});

test("기존 결정이 없을 때만 서버와 네 원문을 대조한다", async () => {
  const pending = deferred<Recorded | null>(); const api = ports(); api.status = vi.fn(() => pending.promise);
  const { container, root } = await mount(api); await select(container);
  expect(api.status).toHaveBeenCalledTimes(1); expect(api.load).not.toHaveBeenCalled(); expect(api.apply).not.toHaveBeenCalled();
  await act(async () => { pending.resolve(null); });
  await vi.waitFor(() => expect(container.querySelector<HTMLTextAreaElement>('[data-testid="frozen-content-draft"] textarea')?.value).toBe("latest draft\n"));
  expect(api.load).toHaveBeenCalledExactlyOnceWith(current, old, item.operation_id);
  expect(api.apply).not.toHaveBeenCalled();
  await act(async () => { root.unmount(); });
});

test("기록 조회 실패는 미결정으로 취급하지 않고 자료 보존과 재시도를 안내한다", async () => {
  const api = ports(); api.status = vi.fn(async () => { throw new Error("storage failed"); });
  const { container, root } = await mount(api); await select(container);
  await vi.waitFor(() => expect(container.textContent).toMatch(/확인하지 못|불러오지 못/));
  expect(api.load).not.toHaveBeenCalled(); expect(api.apply).not.toHaveBeenCalled();
  expect(container.querySelector('[data-testid="frozen-content-apply"]')).toBeNull();
  expect(container.textContent).toMatch(/보존|유지/);
  await act(async () => { root.unmount(); });
});

test("등록 교체 뒤 도착한 이전 결정 상태를 새 등록에 표시하지 않는다", async () => {
  const pending = deferred<Recorded | null>(); const api = ports(); api.status = vi.fn(() => pending.promise);
  const { container, root } = await mount(api); await select(container);
  const replacement = { ...current, device_id: "replacement", device_stream_id: "replacement-stream" };
  api.list = vi.fn(async (binding) => ({ binding, entries: [], next_after_id: null }));
  await act(async () => { root.render(createElement(FrozenContentReview, { binding: replacement, ports: api })); });
  await act(async () => { pending.resolve(recorded); });
  expect(container.textContent).not.toContain("new-operation");
  expect(api.load).not.toHaveBeenCalled(); expect(api.apply).not.toHaveBeenCalled();
  await act(async () => { root.unmount(); });
});
