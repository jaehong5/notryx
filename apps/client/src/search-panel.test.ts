// @vitest-environment jsdom
import { act, createElement } from "react";
import { createRoot } from "react-dom/client";
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { SearchPanel, advanceSearchRequestId, type SearchPage, type SearchPanelPorts } from "./search-panel";
import type { WorkspaceRegistration } from "../../../packages/protocol-types";

const binding: WorkspaceRegistration = { server_origin: "https://example.invalid", workspace_id: "workspace", workspace_epoch: "epoch", device_id: "device", device_stream_id: "stream" };
const first: SearchPage = { results: [{ node_id: "first", scope: "workspace", source: "draft" }], available_documents: 1, total_documents: 2, recovery_available_documents: 0, recovery_total_documents: 0, next_after_id: "cursor" };
const second: SearchPage = { results: [{ node_id: "second", scope: "workspace", source: "cache" }], available_documents: 1, total_documents: 1, recovery_available_documents: 0, recovery_total_documents: 0, next_after_id: null };

function ports(): SearchPanelPorts {
  return { scan: vi.fn(async (_binding, request) => request.after ? second : first), cancel: vi.fn(async () => {}) };
}
function input(container: HTMLElement, value: string) {
  const element = container.querySelector<HTMLInputElement>('input')!;
  Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!.call(element, value);
  element.dispatchEvent(new Event("input", { bubbles: true }));
}
function scope(container: HTMLElement, value: string) {
  const element = container.querySelector<HTMLSelectElement>('select')!;
  element.value = value;
  element.dispatchEvent(new Event("change", { bubbles: true }));
}
function button(container: HTMLElement, text: string) {
  return [...container.querySelectorAll("button")].find((item) => item.textContent === text)!;
}
function submit(container: HTMLElement) {
  container.querySelector("form")!.dispatchEvent(new Event("submit", { bubbles: true, cancelable: true }));
}
beforeEach(() => { vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true); });
afterEach(() => { document.body.replaceChildren(); vi.unstubAllGlobals(); });

test("검색 범위를 명시하고 구간별 coverage와 다음 결과를 확인해 문서를 연다", async () => {
  const api = ports(), open = vi.fn();
  const container = document.createElement("div"); document.body.append(container); const root = createRoot(container);
  await act(async () => { root.render(createElement(SearchPanel, { binding, ports: api, open, label: (result) => `${result.node_id}.md` })); });
  await act(async () => { input(container, "가"); });
  expect(button(container, "검색").disabled).toBe(true);
  await act(async () => { scope(container, "workspace"); });
  await act(async () => { submit(container); });
  expect(api.scan).toHaveBeenCalledTimes(1);
  expect(container.textContent).toContain("이번 구간에서 검색 가능한 문서: 1 / 2");
  expect(container.textContent).toContain("first.md (기기 초안)");
  await act(async () => { container.querySelector<HTMLButtonElement>('[data-search-node="first"]')!.click(); });
  expect(open).toHaveBeenCalledExactlyOnceWith(first.results[0]);
  const request = vi.mocked(api.scan).mock.calls[0][1];
  await act(async () => { button(container, "다음 결과").click(); });
  expect(api.scan).toHaveBeenCalledTimes(2);
  expect(vi.mocked(api.scan).mock.calls[1][1]).toEqual({ ...request, after: "cursor" });
  expect(container.textContent).toContain("이번 구간에서 검색 가능한 문서: 1 / 1");
  expect(container.textContent).not.toContain("first.md");
  expect(container.textContent).toContain("second.md (저장된 본문)");
  await act(async () => { root.unmount(); });
});

test("새 검색 뒤 늦게 도착한 이전 응답을 표시하지 않는다", async () => {
  const api = ports(); let resolveOld!: (page: SearchPage) => void;
  api.scan = vi.fn((_binding, request) => request.query === "old" ? new Promise<SearchPage>((resolve) => { resolveOld = resolve; }) : Promise.resolve(second));
  const container = document.createElement("div"); document.body.append(container); const root = createRoot(container);
  await act(async () => { root.render(createElement(SearchPanel, { binding, ports: api, open: vi.fn() })); });
  await act(async () => { input(container, "old"); scope(container, "workspace"); });
  await act(async () => { submit(container); });
  const oldId = vi.mocked(api.scan).mock.calls[0][1].request_id;
  await act(async () => { input(container, "new"); });
  await act(async () => { submit(container); });
  expect(vi.mocked(api.scan).mock.calls[1][1].request_id).not.toBe(oldId);
  expect(api.cancel).toHaveBeenCalledWith(binding, oldId);
  await act(async () => { resolveOld(first); });
  expect(container.querySelector('[data-search-node="first"]')).toBeNull();
  expect(container.querySelector('[data-search-node="second"]')).not.toBeNull();
  await act(async () => { root.unmount(); });
});

test("등록이 바뀌거나 검색을 취소하면 이전 결과를 표시하지 않는다", async () => {
  const api = ports(); let resolveOld!: (page: SearchPage) => void;
  api.scan = vi.fn(() => new Promise<SearchPage>((resolve) => { resolveOld = resolve; }));
  const container = document.createElement("div"); document.body.append(container); const root = createRoot(container);
  await act(async () => { root.render(createElement(SearchPanel, { binding, ports: api, open: vi.fn() })); });
  await act(async () => { input(container, "가나"); scope(container, "recovery"); });
  await act(async () => { submit(container); });
  const oldId = vi.mocked(api.scan).mock.calls[0][1].request_id;
  const replacement = { ...binding, workspace_epoch: "new-epoch" };
  await act(async () => { root.render(createElement(SearchPanel, { binding: replacement, ports: api, open: vi.fn() })); });
  expect(api.cancel).toHaveBeenCalledWith(binding, oldId);
  await act(async () => { resolveOld(first); });
  expect(container.querySelector('[data-search-node="first"]')).toBeNull();
  await act(async () => { root.unmount(); });
});

test("다음 구간 검색이 무효화되면 이전 결과도 지운다", async () => {
  const api = ports();
  api.scan = vi.fn(async (_binding, request) => { if (request.after) throw new Error("stale_search"); return first; });
  const container = document.createElement("div"); document.body.append(container); const root = createRoot(container);
  await act(async () => { root.render(createElement(SearchPanel, { binding, ports: api, open: vi.fn() })); });
  await act(async () => { input(container, "가나"); scope(container, "workspace"); });
  await act(async () => { submit(container); });
  expect(container.querySelector('[data-search-node="first"]')).not.toBeNull();
  await act(async () => { button(container, "다음 결과").click(); });
  expect(container.querySelector('[data-search-node="first"]')).toBeNull();
  expect(container.textContent).toContain("검색을 다시 시작하세요");
  await act(async () => { root.unmount(); });
});

test("같은 renderer에서 등록 교체와 화면 재생성 후에도 검색 식별자가 계속 증가한다", async () => {
  const api = ports(), open = vi.fn();
  const container = document.createElement("div");
  document.body.append(container);
  let root = createRoot(container);
  await act(async () => { root.render(createElement(SearchPanel, { binding, ports:api, open })); });
  await act(async () => { input(container, "가나"); scope(container, "workspace"); });
  await act(async () => { submit(container); });
  const replacement = { ...binding, workspace_epoch:"new-epoch" };
  await act(async () => { root.render(createElement(SearchPanel, { binding:replacement, ports:api, open })); });
  await act(async () => { input(container, "가나"); scope(container, "workspace"); });
  await act(async () => { submit(container); });
  await act(async () => { root.unmount(); });
  root = createRoot(container);
  await act(async () => { root.render(createElement(SearchPanel, { binding:replacement, ports:api, open })); });
  await act(async () => { input(container, "가나"); scope(container, "workspace"); });
  await act(async () => { submit(container); });
  const calls = vi.mocked(api.scan).mock.calls;
  expect(calls).toHaveLength(3);
  expect(calls[0][0]).toEqual(binding);
  expect(calls[1][0]).toEqual(replacement);
  expect(calls[2][0]).toEqual(replacement);
  const ids = calls.map((call) => call[1].request_id);
  expect(ids.every((id) => /^[1-9][0-9]*$/.test(id))).toBe(true);
  expect(BigInt(ids[1])).toBeGreaterThan(BigInt(ids[0]));
  expect(BigInt(ids[2])).toBeGreaterThan(BigInt(ids[1]));
  await act(async () => { root.unmount(); });
});

test("검색 식별자는 안전한 정수 상한까지만 발급한다", () => {
  expect(advanceSearchRequestId(0)).toBe(1);
  expect(advanceSearchRequestId(Number.MAX_SAFE_INTEGER - 1)).toBe(Number.MAX_SAFE_INTEGER);
  expect(advanceSearchRequestId(Number.MAX_SAFE_INTEGER)).toBeNull();
  expect(advanceSearchRequestId(Number.MAX_SAFE_INTEGER)).toBeNull();
  expect(advanceSearchRequestId(-1)).toBeNull();
  expect(advanceSearchRequestId(1.5)).toBeNull();
  expect(advanceSearchRequestId(Number.MAX_SAFE_INTEGER + 1)).toBeNull();
});
