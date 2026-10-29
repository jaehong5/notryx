// @vitest-environment jsdom
import { act, createElement } from "react";
import { createRoot } from "react-dom/client";
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { FrozenContentReview, type FrozenContentReviewItem, type FrozenContentReviewPorts, type FrozenContentReviewSnapshot } from "./frozen-content-review";
import type { WorkspaceRegistration } from "../../../packages/protocol-types";

const current: WorkspaceRegistration = { server_origin: "https://example.invalid", workspace_id: "workspace", workspace_epoch: "epoch", device_id: "current-device", device_stream_id: "current-stream" };
const old: WorkspaceRegistration = { ...current, device_id: "old-device", device_stream_id: "old-stream" };
const item: FrozenContentReviewItem = { old_binding: old, operation_id: "old-operation", operation_seq: "1", document_id: "document", generation: "1", expected_revision: "base-rev", status: "sent", abandon_state: "none" };
const bytes = (value: string) => new TextEncoder().encode(value);
const snapshot: FrozenContentReviewSnapshot = { ...item, binding: current, expected_revision: "base-rev", local_generation: "2", outcome_unknown: true, base_revision: "base-rev", remote_revision: "remote-rev", operation_bytes: bytes("old payload\n"), base_bytes: bytes("old base\n"), local_bytes: bytes("latest draft\n"), remote_bytes: bytes("remote content\n") };
type DismissResult = { binding: WorkspaceRegistration; old_operation_id: string };
type DismissPorts = FrozenContentReviewPorts & { dismiss: (review: FrozenContentReviewSnapshot) => Promise<DismissResult> };
function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((done) => { resolve = done; });
  return { promise, resolve };
}
function ports(review = snapshot): DismissPorts {
  return {
    list: vi.fn(async (binding) => ({ binding, entries: [review], next_after_id: null })),
    status: vi.fn(async () => null),
    load: vi.fn(async () => review),
    apply: vi.fn(async () => ({ binding: current, operation_id: "new-operation" })),
    dismiss: vi.fn(async () => ({ binding: current, old_operation_id: review.operation_id })),
  };
}
async function mount(api: DismissPorts) {
  const container = document.createElement("div"); document.body.append(container);
  const root = createRoot(container);
  await act(async () => { root.render(createElement(FrozenContentReview, { binding: current, ports: api })); });
  await vi.waitFor(() => expect(container.querySelector(`[data-review-operation="${item.operation_id}"]`)).not.toBeNull());
  await act(async () => { container.querySelector<HTMLButtonElement>(`[data-review-operation="${item.operation_id}"]`)!.click(); });
  await vi.waitFor(() => expect(container.querySelector('[data-testid="frozen-content-draft"] textarea')).not.toBeNull());
  return { container, root };
}
function selection(container: HTMLElement) {
  const input = container.querySelector<HTMLInputElement>('[data-testid="frozen-content-select-dismiss"]');
  expect(input).not.toBeNull(); return input!;
}
function button(container: HTMLElement) {
  const result = container.querySelector<HTMLButtonElement>('[data-testid="frozen-content-dismiss"]');
  expect(result).not.toBeNull(); return result!;
}
beforeEach(() => vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true));
afterEach(() => { document.body.replaceChildren(); vi.unstubAllGlobals(); });

test("표시한 작업의 재발급 포기를 고르고 명시 확인할 때만 해당 snapshot을 전달한다", async () => {
  const api = ports(); const { container, root } = await mount(api);
  expect(container.querySelector<HTMLTextAreaElement>('[data-testid="frozen-content-payload"] textarea')?.value).toBe("old payload\n");
  expect(container.querySelector<HTMLTextAreaElement>('[data-testid="frozen-content-draft"] textarea')?.value).toBe("latest draft\n");
  expect(container.textContent).toMatch(/전송.{0,20}(미확정|알 수 없)/s);
  expect(button(container).disabled).toBe(true); expect(api.dismiss).not.toHaveBeenCalled();
  await act(async () => { selection(container).click(); });
  expect(api.dismiss).not.toHaveBeenCalled(); expect(button(container).disabled).toBe(false);
  await act(async () => { button(container).click(); });
  expect(api.dismiss).toHaveBeenCalledExactlyOnceWith(snapshot); expect(api.apply).not.toHaveBeenCalled();
  expect(container.textContent).toMatch(/재발급.{0,15}포기.{0,15}기록/s);
  expect(container.textContent).toMatch(/초안|다른 작업/);
  expect(container.textContent).toMatch(/미확정|알 수 없/);
  expect(container.textContent).not.toMatch(/이전.{0,15}요청.{0,15}(성공|완료)|서버.{0,10}(반영|저장).{0,10}완료/s);
  await act(async () => { root.unmount(); });
});

test("포기 기록 중 중복 클릭은 동일 작업을 한번만 요청한다", async () => {
  const pending = deferred<DismissResult>(); const api = ports(); api.dismiss = vi.fn(() => pending.promise);
  const { container, root } = await mount(api);
  await act(async () => { selection(container).click(); });
  await act(async () => { button(container).click(); button(container).click(); });
  expect(api.dismiss).toHaveBeenCalledTimes(1);
  await act(async () => { pending.resolve({ binding: current, old_operation_id: item.operation_id }); });
  expect(container.textContent).toMatch(/재발급.{0,15}포기.{0,15}기록/s);
  await act(async () => { root.unmount(); });
});

test("포기 기록 중 같은 항목 재선택으로 동시에 다른 결정을 시작할 수 없다", async () => {
  const pending = deferred<DismissResult>(); const api = ports(); api.dismiss = vi.fn(() => pending.promise);
  const { container, root } = await mount(api);
  await act(async () => { selection(container).click(); });
  await act(async () => { button(container).click(); });
  const itemButton = container.querySelector<HTMLButtonElement>(`[data-review-operation="${item.operation_id}"]`)!;
  expect(itemButton.disabled).toBe(true);
  await act(async () => { itemButton.click(); });
  expect(api.load).toHaveBeenCalledTimes(1); expect(api.dismiss).toHaveBeenCalledTimes(1); expect(api.apply).not.toHaveBeenCalled();
  await act(async () => { pending.resolve({ binding: current, old_operation_id: item.operation_id }); });
  await act(async () => { root.unmount(); });
});

test("포기와 페이지 전환을 같은 이벤트 묶음에서 시도해도 진행 중 결정을 유지한다", async () => {
  const pending = deferred<DismissResult>(); const api = ports(); api.dismiss = vi.fn(() => pending.promise);
  const { container, root } = await mount(api);
  await act(async () => { selection(container).click(); });
  const firstPage = Array.from(container.querySelectorAll<HTMLButtonElement>("button")).find((candidate) => candidate.textContent === "처음 이전 요청");
  expect(firstPage).toBeDefined();
  await act(async () => { button(container).click(); firstPage!.click(); });
  expect(api.dismiss).toHaveBeenCalledTimes(1); expect(api.list).toHaveBeenCalledTimes(1);
  expect(api.apply).not.toHaveBeenCalled();
  await act(async () => { pending.resolve({ binding: current, old_operation_id: item.operation_id }); });
  expect(container.textContent).toMatch(/재발급.{0,15}포기.{0,15}기록/s);
  await act(async () => { root.unmount(); });
});

test("포기 응답 전에 등록이 교체되면 이전 결과를 새 등록에 표시하지 않는다", async () => {
  const pending = deferred<DismissResult>(); const api = ports(); api.dismiss = vi.fn(() => pending.promise);
  const { container, root } = await mount(api);
  await act(async () => { selection(container).click(); });
  await act(async () => { button(container).click(); });
  const replacement = { ...current, device_id: "replacement", device_stream_id: "replacement-stream" };
  api.list = vi.fn(async (binding) => ({ binding, entries: [], next_after_id: null }));
  await act(async () => { root.render(createElement(FrozenContentReview, { binding: replacement, ports: api })); });
  await act(async () => { pending.resolve({ binding: current, old_operation_id: item.operation_id }); });
  expect(container.textContent).not.toMatch(/재발급.{0,15}포기.{0,15}기록/s);
  expect(container.querySelector('[data-testid="frozen-content-draft"]')).toBeNull();
  await act(async () => { root.unmount(); });
});

test("과거 취소 요청이 미확정이어도 선택된 작업의 재발급 포기만 기록한다", async () => {
  const review = { ...snapshot, status: "abandoning" as const, abandon_state: "sent" as const, outcome_unknown: false };
  const api = ports(review); const { container, root } = await mount(api);
  expect(container.textContent).toMatch(/취소.{0,20}미확정/s);
  await act(async () => { selection(container).click(); });
  await act(async () => { button(container).click(); });
  expect(api.dismiss).toHaveBeenCalledExactlyOnceWith(review);
  expect(container.textContent).toMatch(/취소.{0,20}미확정/s);
  expect(container.textContent).not.toMatch(/취소.{0,10}(성공|완료)/s);
  await act(async () => { root.unmount(); });
});

test("포기 기록이 거절되면 비교 원문을 유지하고 다시 확인하도록 안내한다", async () => {
  const api = ports(); api.dismiss = vi.fn(async () => { throw new Error("stale_inspection"); });
  const { container, root } = await mount(api);
  await act(async () => { selection(container).click(); });
  await act(async () => { button(container).click(); });
  await vi.waitFor(() => expect(container.textContent).toMatch(/다시.{0,20}(대조|확인)|재대조/s));
  expect(container.querySelector<HTMLTextAreaElement>('[data-testid="frozen-content-payload"] textarea')?.value).toBe("old payload\n");
  expect(container.querySelector<HTMLTextAreaElement>('[data-testid="frozen-content-draft"] textarea')?.value).toBe("latest draft\n");
  expect(api.apply).not.toHaveBeenCalled();
  await act(async () => { root.unmount(); });
});

test("기록된 포기는 재시작 후 오프라인에서 다른 작업이나 초안을 해결하지 않은 상태로 표시한다", async () => {
  const api = ports();
  Object.assign(api, { status: vi.fn(async () => ({ kind: "dismissed", binding: current, old_binding: old,
    document_id: item.document_id, old_operation_id: item.operation_id, operation_seq: item.operation_seq,
    old_status: "sent", abandon_state: "none" })) });
  api.load = vi.fn(async () => { throw new Error("offline"); });
  const container = document.createElement("div"); document.body.append(container); const root = createRoot(container);
  await act(async () => { root.render(createElement(FrozenContentReview, { binding: current, ports: api })); });
  await vi.waitFor(() => expect(container.querySelector(`[data-review-operation="${item.operation_id}"]`)).not.toBeNull());
  await act(async () => { container.querySelector<HTMLButtonElement>(`[data-review-operation="${item.operation_id}"]`)!.click(); });
  await vi.waitFor(() => expect(container.textContent).toMatch(/재발급.{0,15}포기.{0,15}기록/s));
  expect(api.load).not.toHaveBeenCalled(); expect(api.dismiss).not.toHaveBeenCalled(); expect(api.apply).not.toHaveBeenCalled();
  expect(container.textContent).toMatch(/미확정|알 수 없/);
  expect(container.textContent).toMatch(/초안|다른 작업/);
  expect(container.textContent).not.toMatch(/이전.{0,15}요청.{0,15}(성공|완료)|서버.{0,10}(반영|저장).{0,10}완료/s);
  await act(async () => { root.unmount(); });
});
