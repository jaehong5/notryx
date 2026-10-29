// @vitest-environment jsdom
import { act, createElement } from "react";
import { createRoot } from "react-dom/client";
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { DeletePanel, type DeleteDisplay, type DeletePanelPorts } from "./delete-panel";
import type { NodeState, WorkspaceRegistration } from "../../../packages/protocol-types";

const binding: WorkspaceRegistration = { server_origin: "https://example.invalid", workspace_id: "workspace", workspace_epoch: "epoch", device_id: "device", device_stream_id: "stream" };
const node: NodeState = { node_id: "document", parent_id: "root", name: "Synthetic.md", kind: "file", deleted: false, content_rev: "content", content_etag: '"content"', meta_rev: "meta", meta_etag: '"meta"', node_etag: '"confirmed-node"', content_hash: "sha256:" + "a".repeat(64), size: "1", children_generation: "0", last_change_seq: "1" };
const entry: DeleteDisplay = { operation_id: "operation", operation_seq: "1", node_id: "document", expected_node_etag: '"confirmed-node"', status: "queued", reason: null };
function ports(): DeletePanelPorts {
  return {
    queue: vi.fn(async () => entry),
    status: vi.fn(async () => ({ entries: [], next_after_seq: null })),
    retry: vi.fn(async () => {}), cancel: vi.fn(async () => {}),
  };
}
function mount(api: DeletePanelPorts, extra: Partial<Parameters<typeof DeletePanel>[0]> = {}) {
  const container = document.createElement("div"); document.body.append(container);
  const root = createRoot(container);
  const props = { binding, node, ports: api, revision: 0, onClose: vi.fn(), onWorkReady: vi.fn(), ...extra };
  return { container, root, props };
}
beforeEach(() => { vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true); });
afterEach(() => { document.body.replaceChildren(); vi.unstubAllGlobals(); });

test("확인 시 표시했던 문서와 전체 ETag를 그대로 전송한다", async () => {
  const api = ports(); const { container, root, props } = mount(api);
  await act(async () => { root.render(createElement(DeletePanel, props)); });
  expect(container.textContent).toContain("Synthetic.md");
  await act(async () => { root.render(createElement(DeletePanel, { ...props, node: { ...node, name: "changed.md", node_etag: '"new-node"' } })); });
  expect(container.textContent).toContain("Synthetic.md");
  await act(async () => { container.querySelector<HTMLButtonElement>('[data-testid="delete-confirm"]')!.click(); });
  expect(api.queue).toHaveBeenCalledExactlyOnceWith(binding, { node_id: "document", expected_node_etag: '"confirmed-node"' });
  expect(props.onWorkReady).toHaveBeenCalledOnce();
  await act(async () => { root.unmount(); });
});

test("확인 취소는 삭제 요청을 만들지 않는다", async () => {
  const api = ports(); const { container, root, props } = mount(api);
  await act(async () => { root.render(createElement(DeletePanel, props)); });
  await act(async () => { container.querySelector<HTMLButtonElement>('[data-testid="delete-confirm-cancel"]')!.click(); });
  expect(api.queue).not.toHaveBeenCalled(); expect(props.onClose).toHaveBeenCalledOnce();
  await act(async () => { root.unmount(); });
});

test.each([
  ["OpenEditSessions", "열린 문서"],
  ["OutboxBlocked", "초안 또는 전송 대기"],
  ["ReconciliationRequired", "이전 등록"],
])("보존 자료 때문에 삭제를 거절하면 안내하고 자동 전송하지 않는다: %s", async (code, message) => {
  const api = ports(); api.queue = vi.fn(async () => { throw new Error(code); });
  const { container, root, props } = mount(api);
  await act(async () => { root.render(createElement(DeletePanel, props)); });
  await act(async () => { container.querySelector<HTMLButtonElement>('[data-testid="delete-confirm"]')!.click(); });
  expect(container.textContent).toContain(message);
  expect(props.onWorkReady).not.toHaveBeenCalled(); expect(api.cancel).not.toHaveBeenCalled();
  await act(async () => { root.unmount(); });
});

test("확인한 ETag가 오래되면 최신 값으로 바꾸어 다시 요청하지 않는다", async () => {
  const api = ports(); api.queue = vi.fn(async () => { throw new Error("InvalidRequest"); });
  const { container, root, props } = mount(api);
  await act(async () => { root.render(createElement(DeletePanel, props)); });
  await act(async () => { container.querySelector<HTMLButtonElement>('[data-testid="delete-confirm"]')!.click(); });
  expect(container.textContent).toContain("문서 상태가 변경");
  expect(api.queue).toHaveBeenCalledExactlyOnceWith(binding, { node_id: "document", expected_node_etag: '"confirmed-node"' });
  expect(props.onWorkReady).not.toHaveBeenCalled();
  await act(async () => { root.unmount(); });
});

test("삭제 요청 응답이 불확정이면 같은 확인으로 새 작업을 만들지 않고 상태를 다시 확인한다", async () => {
  const api = ports(); api.queue = vi.fn(async () => { throw new Error("lost response"); });
  const { container, root, props } = mount(api);
  await act(async () => { root.render(createElement(DeletePanel, props)); });
  const previousStatusCalls = vi.mocked(api.status).mock.calls.length;
  await act(async () => { container.querySelector<HTMLButtonElement>('[data-testid="delete-confirm"]')!.click(); });
  expect(container.textContent).toContain("삭제 요청 결과를 확인할 수 없습니다");
  expect(container.querySelector('[data-testid="delete-confirm"]')).toBeNull();
  expect(api.queue).toHaveBeenCalledOnce();
  expect(vi.mocked(api.status).mock.calls.length).toBeGreaterThan(previousStatusCalls);
  expect(props.onWorkReady).not.toHaveBeenCalled();
  await act(async () => { root.unmount(); });
});

test("권한 거절은 명시적인 같은 작업 재시도와 취소를 제공한다", async () => {
  const api = ports(); api.status = vi.fn(async () => ({ entries: [{ ...entry, status: "sent" as const }], next_after_seq: null }));
  const { container, root, props } = mount(api, { permissionDeniedOperationId: "operation" });
  await act(async () => { root.render(createElement(DeletePanel, props)); });
  await vi.waitFor(() => expect(container.textContent).toContain("삭제 권한이 없습니다"));
  expect(api.retry).not.toHaveBeenCalled(); expect(api.cancel).not.toHaveBeenCalled();
  await act(async () => { container.querySelector<HTMLButtonElement>('[data-delete-retry="operation"]')!.click(); });
  expect(api.retry).toHaveBeenCalledExactlyOnceWith(binding, "operation"); expect(api.queue).not.toHaveBeenCalled();
  await act(async () => { container.querySelector<HTMLButtonElement>('[data-delete-cancel="operation"]')!.click(); });
  expect(api.cancel).toHaveBeenCalledExactlyOnceWith(binding, "operation");
  await act(async () => { root.unmount(); });
});

test("결과 미확정 요청은 완료로 표시하지 않고 취소 충돌 시 결과를 다시 확인한다", async () => {
  const api = ports(); api.status = vi.fn(async () => ({ entries: [{ ...entry, status: "sent" as const }], next_after_seq: null }));
  api.cancel = vi.fn(async () => { throw new Error("operation_has_result"); });
  const onRecheck = vi.fn(); const { container, root, props } = mount(api, { onRecheck });
  await act(async () => { root.render(createElement(DeletePanel, props)); });
  await vi.waitFor(() => expect(container.textContent).toContain("서버 결과 확인 중"));
  expect(container.textContent).not.toContain("삭제 완료");
  await act(async () => { container.querySelector<HTMLButtonElement>('[data-delete-cancel="operation"]')!.click(); });
  expect(onRecheck).toHaveBeenCalledExactlyOnceWith("operation");
  await act(async () => { root.unmount(); });
});

test("서버의 412 결과는 변경을 안내하고 새 ETag로 자동 제출하지 않는다", async () => {
  const api = ports(); api.status = vi.fn(async () => ({ entries: [{ ...entry, status: "failed" as const, reason: "node_precondition_failed" }], next_after_seq: null }));
  const { container, root, props } = mount(api);
  await act(async () => { root.render(createElement(DeletePanel, props)); });
  await vi.waitFor(() => expect(container.textContent).toContain("문서 상태가 변경"));
  expect(api.queue).not.toHaveBeenCalled(); expect(api.retry).not.toHaveBeenCalled();
  expect(container.querySelector('[data-delete-retry="operation"]')).toBeNull();
  await act(async () => { root.unmount(); });
});

test("서버 결과를 안전하게 확정할 수 없는 삭제는 복구와 대조를 안내한다", async () => {
  const api = ports(); api.status = vi.fn(async () => ({ entries: [{ ...entry, status: "failed" as const, reason: "recovery_conflict" }], next_after_seq: null }));
  const { container, root, props } = mount(api);
  await act(async () => { root.render(createElement(DeletePanel, props)); });
  await vi.waitFor(() => expect(container.textContent).toContain("서버 결과를 대조"));
  expect(api.queue).not.toHaveBeenCalled(); expect(api.retry).not.toHaveBeenCalled();
  await act(async () => { root.unmount(); });
});

test("등록 변경 뒤 늦은 요청 응답은 새 등록에 표시하지 않는다", async () => {
  const api = ports(); let release!: (value: DeleteDisplay) => void;
  api.queue = vi.fn(() => new Promise<DeleteDisplay>((resolve) => { release = resolve; }));
  const { container, root, props } = mount(api);
  await act(async () => { root.render(createElement(DeletePanel, props)); });
  await act(async () => { container.querySelector<HTMLButtonElement>('[data-testid="delete-confirm"]')!.click(); });
  await act(async () => { root.render(createElement(DeletePanel, { ...props, binding: { ...binding, workspace_epoch: "new-epoch" } })); });
  await act(async () => { release(entry); });
  expect(props.onWorkReady).not.toHaveBeenCalled(); expect(container.textContent).not.toContain("삭제 요청을 이 기기에 저장했습니다");
  await act(async () => { root.unmount(); });
});

test("동시 확인 클릭은 하나의 삭제 요청만 만든다", async () => {
  const api = ports(); let release!: (value: DeleteDisplay) => void;
  api.queue = vi.fn(() => new Promise<DeleteDisplay>((resolve) => { release = resolve; }));
  const { container, root, props } = mount(api);
  await act(async () => { root.render(createElement(DeletePanel, props)); });
  await act(async () => { const button = container.querySelector<HTMLButtonElement>('[data-testid="delete-confirm"]')!; button.click(); button.click(); });
  expect(api.queue).toHaveBeenCalledTimes(1);
  await act(async () => { release(entry); });
  await act(async () => { root.unmount(); });
});

test("현재 문서가 없어도 미확정 삭제 요청을 확인하고 취소할 수 있다", async () => {
  const api = ports(); api.status = vi.fn(async () => ({ entries: [{ ...entry, status: "sent" as const }], next_after_seq: null }));
  const { container, root, props } = mount(api, { node: null as unknown as NodeState });
  await act(async () => { root.render(createElement(DeletePanel, props)); });
  await vi.waitFor(() => expect(container.textContent).toContain("서버 결과 확인 중"));
  expect(container.querySelector('[data-testid="delete-confirm"]')).toBeNull();
  await act(async () => { container.querySelector<HTMLButtonElement>('[data-delete-cancel="operation"]')!.click(); });
  expect(api.cancel).toHaveBeenCalledExactlyOnceWith(binding, "operation");
  await act(async () => { root.unmount(); });
});
