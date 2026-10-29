// @vitest-environment jsdom
import { act, createElement } from "react";
import { createRoot } from "react-dom/client";
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { FrozenContentReview, type FrozenContentReviewItem, type FrozenContentReviewPorts, type FrozenContentReviewSnapshot } from "./frozen-content-review";
import type { WorkspaceRegistration } from "../../../packages/protocol-types";

const current: WorkspaceRegistration = { server_origin: "https://example.invalid", workspace_id: "workspace", workspace_epoch: "epoch", device_id: "current-device", device_stream_id: "current-stream" };
const old: WorkspaceRegistration = { ...current, device_id: "old-device", device_stream_id: "old-stream" };
const item: FrozenContentReviewItem = { old_binding: old, operation_id: "old-operation", operation_seq: "1", document_id: "document", generation: "1", expected_revision: "base-rev", status: "sent", abandon_state: "none" };
const bytes = (text: string) => new TextEncoder().encode(text);
const snapshot: FrozenContentReviewSnapshot = {
  ...item, binding: current, expected_revision: "base-rev", local_generation: "2", outcome_unknown: true,
  base_revision: "base-rev", remote_revision: "remote-rev", operation_bytes: bytes("old payload\n"),
  base_bytes: bytes("old base\n"), local_bytes: bytes("latest draft\n"), remote_bytes: bytes("remote content\n"),
};
type ApplyPorts = FrozenContentReviewPorts & {
  apply: (reviewed: FrozenContentReviewSnapshot) => Promise<{ binding: WorkspaceRegistration; operation_id: string }>;
};
function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason: unknown) => void;
  const promise = new Promise<T>((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}
function ports(review = snapshot): ApplyPorts {
  return {
    list: vi.fn(async (binding) => ({ binding, entries: [review], next_after_id: null })),
    load: vi.fn(async () => review),
    apply: vi.fn(async () => ({ binding: current, operation_id: "new-operation" })),
  };
}
async function mount(api: ApplyPorts) {
  const container = document.createElement("div"); document.body.append(container);
  const root = createRoot(container);
  await act(async () => { root.render(createElement(FrozenContentReview, { binding: current, ports: api })); });
  await vi.waitFor(() => expect(container.querySelector(`[data-review-operation="${item.operation_id}"]`)).not.toBeNull());
  await act(async () => { container.querySelector<HTMLButtonElement>(`[data-review-operation="${item.operation_id}"]`)!.click(); });
  await vi.waitFor(() => expect(container.querySelector('[data-testid="frozen-content-draft"] textarea')).not.toBeNull());
  return { container, root };
}
function selectDraft(container: HTMLElement) {
  const input = container.querySelector<HTMLInputElement>('[data-testid="frozen-content-select-draft"]');
  expect(input).not.toBeNull();
  input!.click();
}
function confirm(container: HTMLElement) {
  const button = container.querySelector<HTMLButtonElement>('[data-testid="frozen-content-apply"]');
  expect(button).not.toBeNull();
  button!.click();
}
function editableButton(container: HTMLElement) {
  const button = container.querySelector<HTMLButtonElement>('[data-testid="frozen-content-apply"]');
  return !!button && !button.disabled;
}

beforeEach(() => vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true));
afterEach(() => { document.body.replaceChildren(); vi.unstubAllGlobals(); });

test("이전 원문과 최신 초안을 보여주고 명시적으로 최신 초안을 고른 뒤에만 새 작업을 요청한다", async () => {
  const api = ports(); const { container, root } = await mount(api);
  expect(container.querySelector<HTMLTextAreaElement>('[data-testid="frozen-content-payload"] textarea')?.value).toBe("old payload\n");
  expect(container.querySelector<HTMLTextAreaElement>('[data-testid="frozen-content-draft"] textarea')?.value).toBe("latest draft\n");
  expect(container.textContent).toMatch(/전송.*(미확정|알 수 없)/s);
  expect(editableButton(container)).toBe(false);
  expect(api.apply).not.toHaveBeenCalled();
  await act(async () => { selectDraft(container); });
  expect(editableButton(container)).toBe(true);
  await act(async () => { confirm(container); });
  expect(api.apply).toHaveBeenCalledExactlyOnceWith(snapshot);
  expect(container.textContent).toMatch(/요청.{0,10}기록/);
  expect(container.textContent).not.toMatch(/서버.{0,15}(반영|저장).{0,15}완료/s);
  await act(async () => { root.unmount(); });
});

test("확인 처리 중 중복 클릭은 같은 작업을 두 번 요청하지 않는다", async () => {
  const pending = deferred<{ binding: WorkspaceRegistration; operation_id: string }>();
  const api = ports(); api.apply = vi.fn(() => pending.promise);
  const { container, root } = await mount(api);
  await act(async () => { selectDraft(container); });
  await act(async () => { confirm(container); confirm(container); });
  expect(api.apply).toHaveBeenCalledTimes(1);
  await act(async () => { pending.resolve({ binding: current, operation_id: "new-operation" }); });
  expect(container.textContent).toMatch(/요청.{0,10}기록/);
  await act(async () => { root.unmount(); });
});

test("승인 중 등록이 바뀌면 늦은 결과를 새 등록 화면에 표시하지 않는다", async () => {
  const pending = deferred<{ binding: WorkspaceRegistration; operation_id: string }>();
  const api = ports(); api.apply = vi.fn(() => pending.promise);
  const { container, root } = await mount(api);
  await act(async () => { selectDraft(container); });
  await act(async () => { confirm(container); });
  const replacement = { ...current, device_id: "replacement", device_stream_id: "replacement-stream" };
  api.list = vi.fn(async (binding) => ({ binding, entries: [], next_after_id: null }));
  await act(async () => { root.render(createElement(FrozenContentReview, { binding: replacement, ports: api })); });
  await act(async () => { pending.resolve({ binding: current, operation_id: "new-operation" }); });
  expect(container.textContent).not.toContain("new-operation");
  expect(container.textContent).not.toMatch(/전송 대기 작업.{0,15}(생성|준비)/s);
  expect(container.querySelector('[data-testid="frozen-content-draft"]')).toBeNull();
  await act(async () => { root.unmount(); });
});

test.each([
  ["다른 epoch", { ...snapshot, old_binding: { ...old, workspace_epoch: "previous-epoch" } }],
  ["취소 미확정", { ...snapshot, status: "abandoning" as const, abandon_state: "sent" as const, outcome_unknown: false }],
  ["비UTF-8 서버 본문", { ...snapshot, remote_bytes: new Uint8Array([0xff, 0xfe]) }],
  ["5 MiB 초과 서버 본문", { ...snapshot, remote_bytes: new Uint8Array(5 * 1024 * 1024 + 1).fill(0x61) }],
])("%s은 원문을 보존하고 적용을 보류한다", async (_label, review) => {
  const api = ports(review); const { container, root } = await mount(api);
  expect(container.textContent).toMatch(/보류|지원하지|적용할 수 없/);
  expect(editableButton(container)).toBe(false);
  expect(api.apply).not.toHaveBeenCalled();
  await act(async () => { root.unmount(); });
});

test("확인 이후 상태가 바뀌어 거절되면 원문을 보존하고 다시 대조하도록 알린다", async () => {
  const api = ports(); api.apply = vi.fn(async () => { throw new Error("stale_inspection"); });
  const { container, root } = await mount(api);
  await act(async () => { selectDraft(container); });
  await act(async () => { confirm(container); });
  await vi.waitFor(() => expect(container.textContent).toMatch(/다시.{0,15}(대조|확인)|재대조/s));
  expect(container.querySelector<HTMLTextAreaElement>('[data-testid="frozen-content-draft"] textarea')?.value).toBe("latest draft\n");
  expect(container.querySelector<HTMLTextAreaElement>('[data-testid="frozen-content-payload"] textarea')?.value).toBe("old payload\n");
  expect(container.textContent).not.toMatch(/서버.{0,15}(반영|저장).{0,15}완료/s);
  expect(api.apply).toHaveBeenCalledTimes(1);
  await act(async () => { root.unmount(); });
});
