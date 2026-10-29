// @vitest-environment jsdom
import { act, createElement } from "react";
import { createRoot } from "react-dom/client";
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { FrozenContentReview, type FrozenContentReviewItem, type FrozenContentReviewPorts, type FrozenContentReviewSnapshot } from "./frozen-content-review";
import type { WorkspaceRegistration } from "../../../packages/protocol-types";

const current: WorkspaceRegistration = { server_origin: "https://example.invalid", workspace_id: "workspace", workspace_epoch: "new-epoch", device_id: "current-device", device_stream_id: "current-stream" };
const old: WorkspaceRegistration = { ...current, workspace_epoch: "old-epoch", device_id: "old-device", device_stream_id: "old-stream" };
const item: FrozenContentReviewItem = { old_binding: old, operation_id: "old-operation", operation_seq: "1", document_id: "node-1", generation: "1", expected_revision: "base-rev", status: "sent", abandon_state: "none" };
const bytes = (value: string) => new TextEncoder().encode(value);
type TargetSnapshot = FrozenContentReviewSnapshot & { current_target: { node_id: string; parent_id: string; name: string; path: string } };
const snapshot: TargetSnapshot = { ...item, binding: current, expected_revision: "base-rev", local_generation: "2", outcome_unknown: true, base_revision: "base-rev", remote_revision: "remote-rev", operation_bytes: bytes("old payload\n"), base_bytes: bytes("old base\n"), local_bytes: bytes("latest draft\n"), remote_bytes: bytes("remote content\n"), current_target: { node_id: "node-1", parent_id: "parent-1", name: "current.md", path: "/folder/current.md" } };
type Result = { binding: WorkspaceRegistration; operation_id: string };
type Ports = FrozenContentReviewPorts & { loadCrossEpoch: (current: WorkspaceRegistration, old: WorkspaceRegistration, operation: string) => Promise<TargetSnapshot>; applyCrossEpoch: (review: TargetSnapshot, confirmed: true) => Promise<Result> };
function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((done) => { resolve = done; });
  return { promise, resolve };
}
function ports(review = snapshot): Ports {
  return {
    list: vi.fn(async (binding) => ({ binding, entries: [item], next_after_id: null })),
    status: vi.fn(async () => null), load: vi.fn(async () => review), loadCrossEpoch: vi.fn(async () => review),
    apply: vi.fn(async () => ({ binding: current, operation_id: "new-operation" })), applyCrossEpoch: vi.fn(async () => ({ binding: current, operation_id: "new-operation" })),
  };
}
async function mount(api: Ports) {
  const container = document.createElement("div"); document.body.append(container); const root = createRoot(container);
  await act(async () => { root.render(createElement(FrozenContentReview, { binding: current, ports: api })); });
  await vi.waitFor(() => expect(container.querySelector(`[data-review-operation="${item.operation_id}"]`)).not.toBeNull());
  await act(async () => { container.querySelector<HTMLButtonElement>(`[data-review-operation="${item.operation_id}"]`)!.click(); });
  await vi.waitFor(() => expect(container.querySelector('[data-testid="frozen-content-draft"] textarea')).not.toBeNull());
  return { container, root };
}
function element<T extends HTMLElement>(container: HTMLElement, testId: string): T {
  const value = container.querySelector<T>(`[data-testid="${testId}"]`); expect(value).not.toBeNull(); return value!;
}
beforeEach(() => vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true));
afterEach(() => { document.body.replaceChildren(); vi.unstubAllGlobals(); });

test("다른 epoch의 현재 대상과 네 원문을 대조하고 두 확인 전에는 적용하지 않는다", async () => {
  const api = ports(); const { container, root } = await mount(api);
  expect(container.textContent).toContain("old-epoch"); expect(container.textContent).toContain("new-epoch");
  expect(container.textContent).toContain("node-1"); expect(container.textContent).toContain("/folder/current.md");
  expect(container.textContent).toMatch(/ID.{0,40}(같아도|같더라도|동일해도).{0,40}(확인|알 수 없|판단)/s);
  expect(container.textContent).toMatch(/전송.{0,20}미확정|이전.{0,20}결과.{0,20}알 수 없/s);
  for (const [id, value] of [["payload", "old payload\n"], ["base", "old base\n"], ["draft", "latest draft\n"], ["remote", "remote content\n"]])
    expect(container.querySelector<HTMLTextAreaElement>(`[data-testid="frozen-content-${id}"] textarea`)?.value).toBe(value);
  const draft = element<HTMLInputElement>(container, "frozen-content-select-draft");
  const target = element<HTMLInputElement>(container, "frozen-content-confirm-target");
  const apply = element<HTMLButtonElement>(container, "frozen-content-apply");
  expect(apply.disabled).toBe(true); expect(api.applyCrossEpoch).not.toHaveBeenCalled();
  await act(async () => { draft.click(); });
  expect(apply.disabled).toBe(true); expect(api.applyCrossEpoch).not.toHaveBeenCalled();
  await act(async () => { target.click(); });
  expect(apply.disabled).toBe(false); expect(api.applyCrossEpoch).not.toHaveBeenCalled();
  await act(async () => { apply.click(); });
  expect(api.applyCrossEpoch).toHaveBeenCalledExactlyOnceWith(snapshot, true);
  expect(container.textContent).toMatch(/요청.{0,10}기록/);
  expect(container.textContent).not.toMatch(/이전.{0,20}요청.{0,20}(성공|완료)|서버.{0,15}(반영|저장).{0,15}완료/s);
  await act(async () => { root.unmount(); });
});

test("대상 확인만으로 최신 초안 적용 요청은 시작하지 않는다", async () => {
  const api = ports(); const { container, root } = await mount(api);
  await act(async () => { element<HTMLInputElement>(container, "frozen-content-confirm-target").click(); });
  expect(element<HTMLButtonElement>(container, "frozen-content-apply").disabled).toBe(true);
  expect(api.applyCrossEpoch).not.toHaveBeenCalled();
  await act(async () => { root.unmount(); });
});

test.each([
  ["대상 누락", undefined], ["다른 ID", { node_id: "another-node", parent_id: "parent-1", name: "current.md", path: "/folder/current.md" }], ["경로 누락", { node_id: "node-1", parent_id: "parent-1", name: "current.md", path: "" }],
])("%s은 대조 대상 확인으로 사용하지 않는다", async (_label, current_target) => {
  const review = { ...snapshot, current_target } as unknown as TargetSnapshot;
  const api = ports(review); const { container, root } = await mount(api);
  expect(container.textContent).toMatch(/보류|확인하지 못|확인할 수 없/);
  const apply = container.querySelector<HTMLButtonElement>('[data-testid="frozen-content-apply"]');
  expect(apply === null || apply.disabled).toBe(true); expect(api.applyCrossEpoch).not.toHaveBeenCalled();
  await act(async () => { root.unmount(); });
});

test("확인 중 중복 클릭은 한 번만 요청하고 원격 변경 거절 뒤 원문을 보존한다", async () => {
  const pending = deferred<Result>(); const api = ports(); api.applyCrossEpoch = vi.fn(() => pending.promise);
  const { container, root } = await mount(api);
  await act(async () => { element<HTMLInputElement>(container, "frozen-content-select-draft").click(); });
  await act(async () => { element<HTMLInputElement>(container, "frozen-content-confirm-target").click(); });
  await act(async () => { const apply = element<HTMLButtonElement>(container, "frozen-content-apply"); apply.click(); apply.click(); });
  expect(api.applyCrossEpoch).toHaveBeenCalledTimes(1);
  await act(async () => { pending.resolve({ binding: current, operation_id: "new-operation" }); });
  await act(async () => { root.unmount(); });
  const rejected = ports(); rejected.applyCrossEpoch = vi.fn(async () => { throw new Error("stale_inspection"); });
  const next = await mount(rejected);
  await act(async () => { element<HTMLInputElement>(next.container, "frozen-content-select-draft").click(); });
  await act(async () => { element<HTMLInputElement>(next.container, "frozen-content-confirm-target").click(); });
  await act(async () => { element<HTMLButtonElement>(next.container, "frozen-content-apply").click(); });
  await vi.waitFor(() => expect(next.container.textContent).toMatch(/다시.{0,15}(대조|확인)|재대조/s));
  expect(next.container.querySelector<HTMLTextAreaElement>('[data-testid="frozen-content-draft"] textarea')?.value).toBe("latest draft\n");
  expect(next.container.querySelector<HTMLTextAreaElement>('[data-testid="frozen-content-remote"] textarea')?.value).toBe("remote content\n");
  expect(next.container.textContent).not.toMatch(/서버.{0,15}(반영|저장).{0,15}완료/s);
  await act(async () => { next.root.unmount(); });
});

test("대상 적용 중 등록 교체 뒤 이전 결과를 새 등록에 표시하지 않는다", async () => {
  const pending = deferred<Result>(); const api = ports(); api.applyCrossEpoch = vi.fn(() => pending.promise);
  const { container, root } = await mount(api);
  await act(async () => { element<HTMLInputElement>(container, "frozen-content-select-draft").click(); });
  await act(async () => { element<HTMLInputElement>(container, "frozen-content-confirm-target").click(); });
  await act(async () => { element<HTMLButtonElement>(container, "frozen-content-apply").click(); });
  const replacement = { ...current, device_id: "replacement", device_stream_id: "replacement-stream" };
  api.list = vi.fn(async (binding) => ({ binding, entries: [], next_after_id: null }));
  await act(async () => { root.render(createElement(FrozenContentReview, { binding: replacement, ports: api })); });
  await act(async () => { pending.resolve({ binding: current, operation_id: "new-operation" }); });
  expect(container.textContent).not.toContain("new-operation");
  expect(container.querySelector('[data-testid="frozen-content-draft"]')).toBeNull();
  await act(async () => { root.unmount(); });
});
