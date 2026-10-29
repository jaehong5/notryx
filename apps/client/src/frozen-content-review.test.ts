// @vitest-environment jsdom
import { act, createElement } from "react";
import { createRoot } from "react-dom/client";
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { FrozenContentReview, type FrozenContentReviewItem, type FrozenContentReviewPorts, type FrozenContentReviewSnapshot } from "./frozen-content-review";
import type { WorkspaceRegistration } from "../../../packages/protocol-types";

const current: WorkspaceRegistration = { server_origin: "https://example.invalid", workspace_id: "workspace", workspace_epoch: "current-epoch", device_id: "device", device_stream_id: "stream" };
const old: WorkspaceRegistration = { ...current, workspace_epoch: "old-epoch", device_id: "old-device", device_stream_id: "old-stream" };
const first: FrozenContentReviewItem & { expected_revision: string } = { old_binding: old, operation_id: "old-operation-1", operation_seq: "1", document_id: "document", generation: "1", expected_revision: "draft-base-revision", status: "sent" };
const second: FrozenContentReviewItem & { expected_revision: string } = { ...first, operation_id: "old-operation-2", operation_seq: "2", generation: "2", expected_revision: "queued-expected-revision", status: "queued" };
const utf8 = (value: string) => new TextEncoder().encode(value);
const snapshot: FrozenContentReviewSnapshot = {
  binding: current, old_binding: old, operation_id: first.operation_id, document_id: first.document_id,
  operation_seq: first.operation_seq, generation: first.generation, status: "sent", outcome_unknown: true,
  expected_revision: first.expected_revision, base_revision: "draft-base-revision", remote_revision: "server-revision",
  operation_bytes: utf8("sent payload\n"), base_bytes: utf8("draft base\n"), local_bytes: utf8("latest draft\n"), remote_bytes: utf8("server source\n"),
};
const sections = [
  ["operation_bytes", "frozen-content-payload", "sent payload\n"],
  ["base_bytes", "frozen-content-base", "draft base\n"],
  ["local_bytes", "frozen-content-draft", "latest draft\n"],
  ["remote_bytes", "frozen-content-remote", "server source\n"],
] as const;

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((done) => { resolve = done; });
  return { promise, resolve };
}
function ports(): FrozenContentReviewPorts {
  return {
    list: vi.fn(async (binding) => ({ binding, entries: [first, second], next_after_id: null })),
    load: vi.fn(async (_binding, _old, operation) => operation === first.operation_id ? snapshot : { ...snapshot, operation_id: second.operation_id, operation_seq: second.operation_seq, generation: second.generation, status: "queued" as const, outcome_unknown: false, expected_revision: second.expected_revision, operation_bytes: utf8("queued payload\n") }),
  };
}
async function mount(api: FrozenContentReviewPorts) {
  const container = document.createElement("div"); document.body.append(container);
  const root = createRoot(container);
  const props = { binding: current, ports: api };
  await act(async () => { root.render(createElement(FrozenContentReview, props)); });
  await vi.waitFor(() => expect(container.querySelector(`[data-review-operation="${first.operation_id}"]`)).not.toBeNull());
  return { container, root, props };
}
async function select(container: HTMLElement, operation: string) {
  const button = container.querySelector<HTMLButtonElement>(`[data-review-operation="${operation}"]`);
  expect(button).not.toBeNull();
  await act(async () => { button!.click(); });
}
function area(container: HTMLElement, id: string) { return container.querySelector<HTMLElement>(`[data-testid="${id}"]`); }
function source(container: HTMLElement, id: string) { return area(container, id)?.querySelector<HTMLTextAreaElement>("textarea"); }
function noActions(container: HTMLElement) {
  expect(container.querySelector('[data-testid="frozen-content-apply"]')).toBeNull();
  expect(container.querySelector('[data-testid="frozen-content-dismiss"]')).toBeNull();
  expect(container.querySelector('[data-testid="frozen-content-send"]')).toBeNull();
  expect(Array.from(container.querySelectorAll("button")).some((button) => /^(적용|폐기|자동 전송|전송)$/.test(button.textContent?.trim() ?? ""))).toBe(false);
}

beforeEach(() => vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true));
afterEach(() => { document.body.replaceChildren(); vi.unstubAllGlobals(); });

test("이전 content 작업별 목록과 선택한 네 원문 및 서로 다른 revision을 읽기 전용으로 표시한다", async () => {
  const api = ports(); const { container, root } = await mount(api);
  expect(container.querySelector(`[data-review-operation="${second.operation_id}"]`)).not.toBeNull();
  await select(container, first.operation_id);
  await vi.waitFor(() => expect(source(container, "frozen-content-payload")?.value).toBe("sent payload\n"));
  expect(api.load).toHaveBeenCalledExactlyOnceWith(current, old, first.operation_id);
  for (const [, id, value] of sections) { expect(source(container, id)?.value).toBe(value); expect(source(container, id)?.readOnly).toBe(true); }
  expect(container.textContent).toContain(old.workspace_epoch);
  expect(container.textContent).toContain(current.workspace_epoch);
  expect(container.textContent).toMatch(/이전.{0,40}old-epoch/s);
  expect(container.textContent).toMatch(/현재.{0,40}current-epoch/s);
  expect(container.textContent).not.toMatch(/(이전|두).{0,20}epoch.{0,20}(같|동일)/i);
  expect(container.textContent).toMatch(/전송.*(결과|처리).*(미확정|알 수 없|불명)/s);
  expect(container.textContent).not.toMatch(/이전.*(전송|요청).*완료/s);
  expect(area(container, "frozen-content-payload")?.textContent).toContain("draft-base-revision");
  expect(area(container, "frozen-content-base")?.textContent).toContain("draft-base-revision");
  expect(area(container, "frozen-content-remote")?.textContent).toContain("server-revision");
  noActions(container);
  await select(container, second.operation_id);
  await vi.waitFor(() => expect(source(container, "frozen-content-payload")?.value).toBe("queued payload\n"));
  expect(api.load).toHaveBeenCalledWith(current, old, second.operation_id);
  expect(area(container, "frozen-content-payload")?.textContent).toContain("queued-expected-revision");
  expect(area(container, "frozen-content-base")?.textContent).toContain("draft-base-revision");
  noActions(container); await act(async () => { root.unmount(); });
});

test("다른 workspace의 작업은 노출하거나 불러오지 않는다", async () => {
  const api = ports();
  const foreign = { ...first, old_binding: { ...old, workspace_id: "foreign-workspace" }, operation_id: "foreign-operation" };
  api.list = vi.fn(async (binding) => ({ binding, entries: [first, foreign], next_after_id: null }));
  const { container, root } = await mount(api);
  expect(container.querySelector('[data-review-operation="foreign-operation"]')).toBeNull();
  expect(container.textContent).not.toContain("foreign-workspace");
  expect(api.load).not.toHaveBeenCalled(); noActions(container);
  await act(async () => { root.unmount(); });
});

test.each(sections.flatMap(([field, id]) => [[field, id, "비UTF8"], [field, id, "5 MiB 초과"]] as const).filter(([field, , kind]) => field !== "remote_bytes" || kind !== "5 MiB 초과"))(
  "%s의 %s 원문 %s은 텍스트로 표시하지 않고 저장 자료를 변경하지 않는다", async (field, id, kind) => {
    const raw = kind === "비UTF8" ? new Uint8Array([0xff, 0xfe]) : new Uint8Array(5 * 1024 * 1024 + 1).fill(120);
    const item = { ...snapshot, [field]: raw };
    const before = raw.slice();
    const api = ports(); api.load = vi.fn(async () => item);
    const { container, root } = await mount(api); await select(container, first.operation_id);
    await vi.waitFor(() => expect(area(container, id)).not.toBeNull());
    expect(source(container, id)).toBeNull();
    expect(area(container, id)?.textContent).toMatch(/표시|텍스트|크기|UTF.?8/i);
    expect(raw.byteLength).toBe(before.byteLength);
    expect(raw.every((value, index) => value === before[index])).toBe(true);
    for (const [otherField, otherId, value] of sections) if (otherField !== field) {
      expect(source(container, otherId)?.value).toBe(value);
      expect(source(container, otherId)?.readOnly).toBe(true);
    }
    noActions(container); await act(async () => { root.unmount(); });
  },
);

test("선택을 바꾸면 늦게 도착한 이전 작업 상세를 새 선택에 표시하지 않는다", async () => {
  const api = ports(); const delayed = deferred<FrozenContentReviewSnapshot>();
  api.load = vi.fn(async (_binding, _old, operation) => operation === first.operation_id ? delayed.promise : { ...snapshot, operation_id: second.operation_id, operation_seq: second.operation_seq, generation: second.generation, status: "queued" as const, outcome_unknown: false, expected_revision: second.expected_revision, operation_bytes: utf8("queued payload\n") });
  const { container, root } = await mount(api);
  await select(container, first.operation_id); await select(container, second.operation_id);
  await vi.waitFor(() => expect(source(container, "frozen-content-payload")?.value).toBe("queued payload\n"));
  await act(async () => { delayed.resolve(snapshot); });
  expect(source(container, "frozen-content-payload")?.value).toBe("queued payload\n");
  expect(area(container, "frozen-content-payload")?.textContent).toContain("queued-expected-revision");
  noActions(container); await act(async () => { root.unmount(); });
});

test("등록이 바뀌면 늦게 도착한 이전 상세를 새 등록에 표시하지 않는다", async () => {
  const api = ports(); const delayed = deferred<FrozenContentReviewSnapshot>();
  api.load = vi.fn(() => delayed.promise);
  const { container, root, props } = await mount(api); await select(container, first.operation_id);
  const replacement = { ...current, workspace_epoch: "replacement-epoch", device_id: "replacement-device", device_stream_id: "replacement-stream" };
  api.list = vi.fn(async (binding) => ({ binding, entries: [], next_after_id: null }));
  await act(async () => { root.render(createElement(FrozenContentReview, { ...props, binding: replacement, ports: api })); });
  await act(async () => { delayed.resolve(snapshot); });
  expect(source(container, "frozen-content-payload")).toBeUndefined();
  expect(container.textContent).not.toContain("draft-base-revision");
  expect(container.textContent).not.toContain("sent payload");
  noActions(container); await act(async () => { root.unmount(); });
});


test("등록이 바뀐 뒤 도착한 이전 목록은 새 등록에 표시하지 않는다", async () => {
  const api = ports();
  const delayed = deferred<{ binding: WorkspaceRegistration; entries: FrozenContentReviewItem[]; next_after_id: string | null }>();
  api.list = vi.fn((binding) => binding.device_id === current.device_id ? delayed.promise : Promise.resolve({ binding, entries: [], next_after_id: null }));
  const container = document.createElement("div"); document.body.append(container); const root = createRoot(container);
  await act(async () => { root.render(createElement(FrozenContentReview, { binding: current, ports: api })); });
  const replacement = { ...current, device_id: "replacement-device", device_stream_id: "replacement-stream" };
  await act(async () => { root.render(createElement(FrozenContentReview, { binding: replacement, ports: api })); });
  await act(async () => { delayed.resolve({ binding: current, entries: [first], next_after_id: null }); });
  expect(container.querySelector(`[data-review-operation="${first.operation_id}"]`)).toBeNull();
  expect(api.load).not.toHaveBeenCalled(); noActions(container);
  await act(async () => { root.unmount(); });
});
