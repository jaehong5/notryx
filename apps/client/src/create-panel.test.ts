// @vitest-environment jsdom
import { act, createElement } from "react";
import { createRoot } from "react-dom/client";
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { CreatePanel, type CreateDisplay, type CreatePanelPorts } from "./create-panel";
import type { NodeState, WorkspaceRegistration } from "../../../packages/protocol-types";

const binding: WorkspaceRegistration = { server_origin: "https://example.invalid", workspace_id: "workspace", workspace_epoch: "epoch", device_id: "device", device_stream_id: "stream" };
const folder: NodeState = { node_id: "root", parent_id: null, name: "", kind: "folder", deleted: false, content_rev: null, content_etag: null, meta_rev: "meta", meta_etag: '"meta"', node_etag: '"node"', content_hash: null, size: null, children_generation: "0", last_change_seq: "1" };
const entry: CreateDisplay = { operation_id: "op", operation_seq: "1", node_id: "new", parent_id: "root", name: "Notes", kind: "folder", status: "queued", reason: null };
function ports(): CreatePanelPorts {
  return {
    folders: vi.fn(async () => ({ nodes: [folder], next_after_id: null })),
    queue: vi.fn(async (_binding, request) => ({ ...entry, ...request, status: "queued" as const })),
    status: vi.fn(async () => ({ entries: [], next_after_seq: null })),
    retry: vi.fn(async () => {}),
    cancel: vi.fn(async () => {}),
  };
}
function setInput(input: HTMLInputElement, value: string) {
  Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!.call(input, value);
  input.dispatchEvent(new Event("input", { bubbles: true }));
}
beforeEach(() => { vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true); });
afterEach(() => { document.body.replaceChildren(); vi.unstubAllGlobals(); });

test("확정된 폴더만 선택하고 생성 요청을 서버 트리와 별도로 표시한다", async () => {
  const api = ports();
  api.folders = vi.fn(async () => ({ nodes: [folder, { ...folder, node_id: "deleted", parent_id: "root", name: "deleted", deleted: true }, { ...folder, node_id: "file", parent_id: "root", name: "file", kind: "file" as const, content_rev: "c", content_etag: '"c"', content_hash: "sha256:abc", size: "0" }], next_after_id: "next" }));
  const ready = vi.fn();
  const container = document.createElement("div"); document.body.append(container); const root = createRoot(container);
  await act(async () => { root.render(createElement(CreatePanel, { binding, ports: api, revision: 0, onWorkReady: ready })); });
  await vi.waitFor(() => expect(container.querySelector('[value="root"]')).not.toBeNull());
  expect(container.querySelector('[value="deleted"]')).toBeNull(); expect(container.querySelector('[data-testid="create-parent"] [value="file"]')).toBeNull();
  const select = container.querySelector<HTMLSelectElement>('[data-testid="create-parent"]')!;
  await act(async () => { select.value = "root"; select.dispatchEvent(new Event("change", { bubbles: true })); setInput(container.querySelector<HTMLInputElement>('[data-testid="create-name"]')!, "Notes"); });
  await act(async () => { container.querySelector<HTMLButtonElement>('[data-testid="create-submit"]')!.click(); });
  expect(api.queue).toHaveBeenCalledExactlyOnceWith(binding, { parent_id: "root", name: "Notes", kind: "folder" });
  expect(ready).toHaveBeenCalledOnce(); expect(container.textContent).toContain("전송 대기");
  expect(container.querySelector('[value="new"]')).toBeNull();
  expect(container.textContent).toContain("다음 폴더");
  await act(async () => { root.unmount(); });
});

test("등록 변경 뒤 지연된 생성 응답은 새 등록에 표시하거나 전송을 시작하지 않는다", async () => {
  const api = ports(); let release!: (item: CreateDisplay) => void;
  api.queue = vi.fn(() => new Promise<CreateDisplay>((resolve) => { release = resolve; }));
  const ready = vi.fn(); const container = document.createElement("div"); document.body.append(container); const root = createRoot(container);
  const props = { binding, ports: api, revision: 0, onWorkReady: ready };
  await act(async () => { root.render(createElement(CreatePanel, props)); });
  await vi.waitFor(() => expect(container.querySelector('[value="root"]')).not.toBeNull());
  await act(async () => { const select = container.querySelector<HTMLSelectElement>('[data-testid="create-parent"]')!; select.value = "root"; select.dispatchEvent(new Event("change", { bubbles: true })); setInput(container.querySelector<HTMLInputElement>('[data-testid="create-name"]')!, "Notes"); });
  await act(async () => { container.querySelector<HTMLButtonElement>('[data-testid="create-submit"]')!.click(); });
  await act(async () => { root.render(createElement(CreatePanel, { ...props, binding: { ...binding, workspace_epoch: "new-epoch" } })); });
  await act(async () => { release(entry); });
  expect(ready).not.toHaveBeenCalled(); expect(container.textContent).not.toContain("Notes: 전송 대기");
  await act(async () => { root.unmount(); });
});

test("권한이 거절된 요청은 명시적으로 같은 작업을 재시도하거나 취소한다", async () => {
  const api = ports(); api.status = vi.fn(async () => ({ entries: [{ ...entry, status: "sent" as const }], next_after_seq: null }));
  const ready = vi.fn(); const container = document.createElement("div"); document.body.append(container); const root = createRoot(container);
  await act(async () => { root.render(createElement(CreatePanel, { binding, ports: api, revision: 0, permissionDeniedOperationId: "op", onWorkReady: ready })); });
  await vi.waitFor(() => expect(container.textContent).toContain("생성 권한이 없습니다"));
  expect(api.retry).not.toHaveBeenCalled(); expect(api.cancel).not.toHaveBeenCalled();
  await act(async () => { container.querySelector<HTMLButtonElement>('[data-create-retry="op"]')!.click(); });
  expect(api.retry).toHaveBeenCalledExactlyOnceWith(binding, "op"); expect(api.queue).not.toHaveBeenCalled();
  await act(async () => { container.querySelector<HTMLButtonElement>('[data-create-cancel="op"]')!.click(); });
  expect(api.cancel).toHaveBeenCalledExactlyOnceWith(binding, "op"); expect(container.textContent).toContain("취소 처리 중");
  await act(async () => { root.unmount(); });
});

test("지연된 취소가 먼저 수신된 생성 완료 상태를 덮어쓰지 않는다", async () => {
  const api = ports(); let release!: () => void;
  api.cancel = vi.fn(() => new Promise<void>((resolve) => { release = resolve; }));
  api.status = vi.fn(async () => ({ entries: [{ ...entry, status: "sent" as const }], next_after_seq: null }));
  const container = document.createElement("div"); document.body.append(container); const root = createRoot(container);
  const props = { binding, ports: api, revision: 0, onWorkReady: vi.fn() };
  await act(async () => { root.render(createElement(CreatePanel, props)); });
  await vi.waitFor(() => expect(container.querySelector('[data-create-cancel="op"]')).not.toBeNull());
  await act(async () => { container.querySelector<HTMLButtonElement>('[data-create-cancel="op"]')!.click(); });
  api.status = vi.fn(async () => ({ entries: [{ ...entry, status: "committed" as const }], next_after_seq: null }));
  await act(async () => { root.render(createElement(CreatePanel, { ...props, revision: 1 })); });
  await vi.waitFor(() => expect(container.textContent).toContain("생성 완료"));
  await act(async () => { release(); });
  expect(container.textContent).toContain("생성 완료"); expect(container.textContent).not.toContain("취소 처리 중");
  await act(async () => { root.unmount(); });
});

test("취소할 때 서버에 이미 결과가 있으면 해당 작업의 결과 조회를 재개한다", async () => {
  const api = ports();
  api.status = vi.fn(async () => ({ entries: [{ ...entry, status: "sent" as const }], next_after_seq: null }));
  api.cancel = vi.fn(async () => { throw new Error("operation_has_result"); });
  const recheck = vi.fn();
  const container = document.createElement("div"); document.body.append(container); const root = createRoot(container);
  await act(async () => { root.render(createElement(CreatePanel, { binding, ports: api, revision: 0, permissionDeniedOperationId: "op", onWorkReady: vi.fn(), onRecheck: recheck })); });
  await vi.waitFor(() => expect(container.querySelector('[data-create-cancel="op"]')).not.toBeNull());
  await act(async () => { container.querySelector<HTMLButtonElement>('[data-create-cancel="op"]')!.click(); });
  expect(recheck).toHaveBeenCalledExactlyOnceWith("op");
  await act(async () => { root.unmount(); });
});

test("빠른 중복 클릭은 한 생성 요청만 저장한다", async () => {
  const api = ports(); let release!: (item: CreateDisplay) => void;
  api.queue = vi.fn(() => new Promise<CreateDisplay>((resolve) => { release = resolve; }));
  const container = document.createElement("div"); document.body.append(container); const root = createRoot(container);
  await act(async () => { root.render(createElement(CreatePanel, { binding, ports: api, revision: 0, onWorkReady: vi.fn() })); });
  await vi.waitFor(() => expect(container.querySelector('[value="root"]')).not.toBeNull());
  await act(async () => { const select = container.querySelector<HTMLSelectElement>('[data-testid="create-parent"]')!; select.value = "root"; select.dispatchEvent(new Event("change", { bubbles: true })); setInput(container.querySelector<HTMLInputElement>('[data-testid="create-name"]')!, "Notes"); });
  await act(async () => { const submit = container.querySelector<HTMLButtonElement>('[data-testid="create-submit"]')!; submit.click(); submit.click(); });
  expect(api.queue).toHaveBeenCalledTimes(1);
  await act(async () => { release(entry); });
  await act(async () => { root.unmount(); });
});
