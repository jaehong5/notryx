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
type LocalSnapshot = Omit<FrozenContentReviewSnapshot, "remote_bytes" | "remote_revision" | "inspected"> & { dependency: string | null; remote_status: "deleted" | "too_large" | "unknown"; remote_index_error: boolean };
const local: LocalSnapshot = { ...item, binding: current, dependency: null, expected_revision: "base-rev", local_generation: "2", outcome_unknown: true, base_revision: "base-rev", operation_bytes: bytes("old payload\n"), base_bytes: bytes("old base\n"), local_bytes: bytes("latest draft\n"), remote_status: "unknown", remote_index_error: false };
type DismissResult = { binding: WorkspaceRegistration; old_operation_id: string };
type LocalPorts = FrozenContentReviewPorts & {
  loadLocal: (binding: WorkspaceRegistration, oldBinding: WorkspaceRegistration, operationId: string) => Promise<LocalSnapshot>;
  dismissLocal: (review: LocalSnapshot) => Promise<DismissResult>;
};
function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((done) => { resolve = done; });
  return { promise, resolve };
}
function ports(review = local): LocalPorts {
  return {
    list: vi.fn(async (binding) => ({ binding, entries: [item], next_after_id: null })),
    status: vi.fn(async () => null),
    load: vi.fn(async () => { throw new Error("remote_content_unavailable"); }),
    loadLocal: vi.fn(async () => review),
    apply: vi.fn(async () => ({ binding: current, operation_id: "new-operation" })),
    dismissLocal: vi.fn(async () => ({ binding: current, old_operation_id: item.operation_id })),
  };
}
async function mount(api: LocalPorts) {
  const container = document.createElement("div"); document.body.append(container); const root = createRoot(container);
  await act(async () => { root.render(createElement(FrozenContentReview, { binding: current, ports: api })); });
  await vi.waitFor(() => expect(container.querySelector(`[data-review-operation="${item.operation_id}"]`)).not.toBeNull());
  await act(async () => { container.querySelector<HTMLButtonElement>(`[data-review-operation="${item.operation_id}"]`)!.click(); });
  await vi.waitFor(() => expect(container.querySelector('[data-testid="frozen-content-draft"] textarea')).not.toBeNull());
  return { container, root };
}
function select(container: HTMLElement) {
  const input = container.querySelector<HTMLInputElement>('[data-testid="frozen-content-select-dismiss"]');
  expect(input).not.toBeNull(); input!.click();
}
function confirm(container: HTMLElement) {
  const button = container.querySelector<HTMLButtonElement>('[data-testid="frozen-content-dismiss"]');
  expect(button).not.toBeNull(); return button!;
}
beforeEach(() => vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true));
afterEach(() => { document.body.replaceChildren(); vi.unstubAllGlobals(); });

test.each([
  ["deleted", /삭제/], ["too_large", /크기|25 MiB|너무 큼/], ["unknown", /확인하지 못|확인되지|미확인/],
] as const)("서버 원문을 읽지 못한 %s 상태에서 로컬 원문과 확인된 상태를 구분해 보여준다", async (remote_status, wording) => {
  const review = { ...local, remote_status }; const api = ports(review); const { container, root } = await mount(api);
  expect(api.loadLocal).toHaveBeenCalledExactlyOnceWith(current, old, item.operation_id);
  expect(container.querySelector<HTMLTextAreaElement>('[data-testid="frozen-content-payload"] textarea')?.value).toBe("old payload\n");
  expect(container.querySelector<HTMLTextAreaElement>('[data-testid="frozen-content-base"] textarea')?.value).toBe("old base\n");
  expect(container.querySelector<HTMLTextAreaElement>('[data-testid="frozen-content-draft"] textarea')?.value).toBe("latest draft\n");
  expect(container.textContent).toMatch(wording);
  expect(container.querySelector('[data-testid="frozen-content-remote"] textarea')).toBeNull();
  expect(container.querySelector<HTMLButtonElement>('[data-testid="frozen-content-apply"]')?.disabled ?? true).toBe(true);
  expect(confirm(container).disabled).toBe(true); expect(api.dismissLocal).not.toHaveBeenCalled(); expect(api.apply).not.toHaveBeenCalled();
  if (remote_status === "unknown") expect(container.textContent).not.toMatch(/서버.{0,20}(삭제됐|삭제되었|삭제됨)/s);
  await act(async () => { select(container); });
  expect(api.dismissLocal).not.toHaveBeenCalled();
  await act(async () => { confirm(container).click(); });
  expect(api.dismissLocal).toHaveBeenCalledExactlyOnceWith(review); expect(api.apply).not.toHaveBeenCalled();
  expect(container.textContent).toMatch(/재발급.{0,15}포기.{0,15}기록/s);
  expect(container.textContent).toMatch(/초안|다른 작업/);
  expect(container.textContent).toMatch(/미확정|알 수 없/);
  await act(async () => { root.unmount(); });
});

test("로컬 자료 확인 실패는 포기 가능 상태로 표시하지 않는다", async () => {
  const api = ports(); api.loadLocal = vi.fn(async () => { throw new Error("invalid_local_data"); });
  const container = document.createElement("div"); document.body.append(container); const root = createRoot(container);
  await act(async () => { root.render(createElement(FrozenContentReview, { binding: current, ports: api })); });
  await vi.waitFor(() => expect(container.querySelector(`[data-review-operation="${item.operation_id}"]`)).not.toBeNull());
  await act(async () => { container.querySelector<HTMLButtonElement>(`[data-review-operation="${item.operation_id}"]`)!.click(); });
  await vi.waitFor(() => expect(container.textContent).toMatch(/불러오지 못|확인하지 못/));
  expect(api.dismissLocal).not.toHaveBeenCalled(); expect(api.apply).not.toHaveBeenCalled();
  expect(container.querySelector('[data-testid="frozen-content-dismiss"]')).toBeNull();
  await act(async () => { root.unmount(); });
});

test("선택한 작업과 다른 로컬 원문 결과는 포기 자료로 사용하지 않는다", async () => {
  const api = ports({ ...local, operation_seq: "2" });
  const container = document.createElement("div"); document.body.append(container); const root = createRoot(container);
  await act(async () => { root.render(createElement(FrozenContentReview, { binding: current, ports: api })); });
  await vi.waitFor(() => expect(container.querySelector(`[data-review-operation="${item.operation_id}"]`)).not.toBeNull());
  await act(async () => { container.querySelector<HTMLButtonElement>(`[data-review-operation="${item.operation_id}"]`)!.click(); });
  await vi.waitFor(() => expect(container.textContent).toMatch(/불러오지 못|확인하지 못/));
  expect(container.querySelector('[data-testid="frozen-content-dismiss"]')).toBeNull();
  expect(api.dismissLocal).not.toHaveBeenCalled(); expect(api.apply).not.toHaveBeenCalled();
  await act(async () => { root.unmount(); });
});

test("보존된 로컬 원문을 텍스트로 확인할 수 없으면 포기를 보류한다", async () => {
  const api = ports({ ...local, operation_bytes: new Uint8Array([0xff]) }); const { container, root } = await mount(api);
  expect(container.textContent).toMatch(/원문.{0,20}표시할 수 없어.{0,20}보류/s);
  expect(confirm(container).disabled).toBe(true); expect(api.dismissLocal).not.toHaveBeenCalled();
  await act(async () => { root.unmount(); });
});

test("선행 작업 의존성을 표시하고 대조한 값으로 포기 요청을 보낸다", async () => {
  const review = { ...local, dependency: "previous-operation" }; const api = ports(review); const { container, root } = await mount(api);
  expect(container.textContent).toContain("previous-operation");
  await act(async () => { select(container); });
  await act(async () => { confirm(container).click(); });
  expect(api.dismissLocal).toHaveBeenCalledExactlyOnceWith(review);
  await act(async () => { root.unmount(); });
});

test("로컬 포기 요청 중 등록이 바뀌면 늦은 결과를 새 등록에 반영하지 않는다", async () => {
  const pending = deferred<DismissResult>(); const api = ports(); api.dismissLocal = vi.fn(() => pending.promise);
  const { container, root } = await mount(api);
  await act(async () => { select(container); });
  await act(async () => { confirm(container).click(); confirm(container).click(); });
  expect(api.dismissLocal).toHaveBeenCalledTimes(1);
  const replacement = { ...current, device_id: "replacement", device_stream_id: "replacement-stream" };
  api.list = vi.fn(async (binding) => ({ binding, entries: [], next_after_id: null }));
  await act(async () => { root.render(createElement(FrozenContentReview, { binding: replacement, ports: api })); });
  await act(async () => { pending.resolve({ binding: current, old_operation_id: item.operation_id }); });
  expect(container.textContent).not.toMatch(/재발급.{0,15}포기.{0,15}기록/s);
  expect(container.querySelector('[data-testid="frozen-content-draft"]')).toBeNull();
  await act(async () => { root.unmount(); });
});
