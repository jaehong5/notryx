// @vitest-environment jsdom
import { act, createElement } from "react";
import { createRoot } from "react-dom/client";
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { MovePanel, type MoveDisplay, type MovePanelPorts } from "./move-panel";
import type { NodeState, WorkspaceRegistration } from "../../../packages/protocol-types";

const binding: WorkspaceRegistration = { server_origin: "https://example.invalid", workspace_id: "workspace", workspace_epoch: "epoch", device_id: "device", device_stream_id: "stream" };
function node(node_id: string, parent_id: string | null, name: string, kind: "file" | "folder"): NodeState {
  return { node_id, parent_id, name, kind, deleted: false, content_rev: kind === "file" ? "content" : null, content_etag: kind === "file" ? '"content"' : null, content_hash: kind === "file" ? "sha256:" + "a".repeat(64) : null, size: kind === "file" ? "1" : null, meta_rev: "meta", meta_etag: '"meta"', node_etag: '"node"', children_generation: "0", last_change_seq: "1" };
}
const root = node("root", null, "", "folder"), source = node("source", "root", "source", "folder"), destination = node("destination", "root", "destination", "folder");
const target = node("document", "source", "Synthetic.md", "file");
const request = { expected_parent_id: "source", parent_id: "destination", name: "renamed.md" };
const entry: MoveDisplay = { operation_id: "operation", operation_seq: "1", node_id: "document", expected_meta_etag: '"meta"', request, status: "queued", reason: null };
function ports(): MovePanelPorts {
  return { folders: vi.fn(async () => ({ nodes: [root, source, destination], next_after_id: null })), queue: vi.fn(async () => entry), status: vi.fn(async () => ({ entries: [], next_after_seq: null })), retry: vi.fn(async () => {}), cancel: vi.fn(async () => {}) };
}
function mount(api: MovePanelPorts, extra: Partial<Parameters<typeof MovePanel>[0]> = {}) {
  const container = document.createElement("div"); document.body.append(container);
  const root = createRoot(container);
  const props = { binding, node: target, displayName: "source/Synthetic.md", ports: api, revision: 0, onClose: vi.fn(), onWorkReady: vi.fn(), ...extra };
  return { container, root, props };
}
function change(element: HTMLInputElement | HTMLSelectElement, value: string) {
  const prototype = element instanceof HTMLInputElement ? HTMLInputElement.prototype : HTMLSelectElement.prototype;
  Object.getOwnPropertyDescriptor(prototype, "value")!.set!.call(element, value);
  element.dispatchEvent(new Event(element instanceof HTMLInputElement ? "input" : "change", { bubbles: true }));
}
async function prepare(container: HTMLElement) {
  await vi.waitFor(() => expect(container.querySelector<HTMLSelectElement>('[data-testid="move-parent"]')?.options.length).toBeGreaterThan(1));
  await act(async () => { change(container.querySelector<HTMLSelectElement>('[data-testid="move-parent"]')!, "destination"); change(container.querySelector<HTMLInputElement>('[data-testid="move-name"]')!, "renamed.md"); });
  await act(async () => { container.querySelector<HTMLButtonElement>('[data-testid="move-prepare"]')!.click(); });
}
beforeEach(() => { vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true); });
afterEach(() => { document.body.replaceChildren(); vi.unstubAllGlobals(); });

test("확인 화면에 기존 문서와 목적지 및 미검사 링크 범위를 보여주고 확인 조건을 그대로 보낸다", async () => {
  const api = ports(); const { container, root, props } = mount(api);
  await act(async () => { root.render(createElement(MovePanel, props)); });
  await prepare(container);
  expect(container.textContent).toContain("source/Synthetic.md");
  expect(container.textContent).toContain("destination");
  expect(container.textContent).toContain("renamed.md");
  expect(container.textContent).toContain("링크");
  expect(container.textContent).toContain("검사하지 않았습니다");
  expect(api.queue).not.toHaveBeenCalled();
  await act(async () => { root.render(createElement(MovePanel, { ...props, node: { ...target, parent_id: "other", meta_etag: '"new-meta"' } })); });
  await act(async () => { container.querySelector<HTMLButtonElement>('[data-testid="move-confirm"]')!.click(); });
  expect(api.queue).toHaveBeenCalledExactlyOnceWith(binding, { node_id: "document", expected_meta_etag: '"meta"', request });
  expect(props.onWorkReady).toHaveBeenCalledOnce();
  await act(async () => { root.unmount(); });
});

test("확인 취소는 이동 요청을 만들지 않는다", async () => {
  const api = ports(); const { container, root, props } = mount(api);
  await act(async () => { root.render(createElement(MovePanel, props)); });
  await prepare(container);
  await act(async () => { container.querySelector<HTMLButtonElement>('[data-testid="move-confirm-cancel"]')!.click(); });
  expect(api.queue).not.toHaveBeenCalled();
  expect(props.onWorkReady).not.toHaveBeenCalled();
  await act(async () => { root.unmount(); });
});

test("이동 요청 결과가 불확정이면 같은 확인으로 새 작업을 만들지 않고 상태를 확인한다", async () => {
  const api = ports(); api.queue = vi.fn(async () => { throw new Error("lost response"); });
  const { container, root, props } = mount(api);
  await act(async () => { root.render(createElement(MovePanel, props)); });
  await prepare(container);
  const before = vi.mocked(api.status).mock.calls.length;
  await act(async () => { container.querySelector<HTMLButtonElement>('[data-testid="move-confirm"]')!.click(); });
  expect(container.textContent).toContain("이동 요청 결과를 확인할 수 없습니다");
  expect(container.querySelector('[data-testid="move-confirm"]')).toBeNull();
  expect(api.queue).toHaveBeenCalledOnce();
  expect(vi.mocked(api.status).mock.calls.length).toBeGreaterThan(before);
  expect(props.onWorkReady).not.toHaveBeenCalled();
  await act(async () => { root.unmount(); });
});

test("선택한 문서가 없어도 권한 거절된 동일 작업을 명시적으로 재시도하거나 취소한다", async () => {
  const api = ports(); api.status = vi.fn(async () => ({ entries: [{ ...entry, status: "sent" as const }], next_after_seq: null }));
  const { container, root, props } = mount(api, { node: null, permissionDeniedOperationId: "operation" });
  await act(async () => { root.render(createElement(MovePanel, props)); });
  await vi.waitFor(() => expect(container.textContent).toContain("이동 권한이 없습니다"));
  expect(api.retry).not.toHaveBeenCalled(); expect(api.cancel).not.toHaveBeenCalled();
  await act(async () => { container.querySelector<HTMLButtonElement>('[data-move-retry="operation"]')!.click(); });
  expect(api.retry).toHaveBeenCalledExactlyOnceWith(binding, "operation");
  await act(async () => { container.querySelector<HTMLButtonElement>('[data-move-cancel="operation"]')!.click(); });
  expect(api.cancel).toHaveBeenCalledExactlyOnceWith(binding, "operation");
  expect(api.queue).not.toHaveBeenCalled();
  await act(async () => { root.unmount(); });
});

test("서버 412는 변경을 안내하며 새 조건으로 자동 재발급하지 않는다", async () => {
  const api = ports(); api.status = vi.fn(async () => ({ entries: [{ ...entry, status: "failed" as const, reason: "metadata_precondition_failed" }], next_after_seq: null }));
  const { container, root, props } = mount(api, { node: null });
  await act(async () => { root.render(createElement(MovePanel, props)); });
  await vi.waitFor(() => expect(container.textContent).toContain("문서 상태가 변경"));
  expect(api.queue).not.toHaveBeenCalled(); expect(api.retry).not.toHaveBeenCalled();
  await act(async () => { root.unmount(); });
});

test("등록 변경 뒤 늦은 응답은 새 등록에 표시하거나 전송을 시작하지 않는다", async () => {
  const api = ports(); let release!: (value: MoveDisplay) => void;
  api.queue = vi.fn(() => new Promise<MoveDisplay>((resolve) => { release = resolve; }));
  const { container, root, props } = mount(api);
  await act(async () => { root.render(createElement(MovePanel, props)); });
  await prepare(container);
  await act(async () => { container.querySelector<HTMLButtonElement>('[data-testid="move-confirm"]')!.click(); });
  await act(async () => { root.render(createElement(MovePanel, { ...props, binding: { ...binding, workspace_epoch: "new-epoch" } })); });
  await act(async () => { release(entry); });
  expect(props.onWorkReady).not.toHaveBeenCalled();
  expect(container.textContent).not.toContain("이동 요청을 이 기기에 저장했습니다");
  await act(async () => { root.unmount(); });
});

test("확인 뒤 목적지 경로가 바뀌면 이전 확인으로 전송하지 않고 새 경로를 다시 확인한다", async () => {
  const api = ports();
  const changed = { ...destination, name: "new-destination" };
  let reads = 0;
  api.folders = vi.fn(async () => ({ nodes: [root, source, ++reads === 1 ? destination : changed], next_after_id: null }));
  const { container, root: mounted, props } = mount(api);
  await act(async () => { mounted.render(createElement(MovePanel, props)); });
  await prepare(container);
  expect(container.textContent).toContain("destination");
  await act(async () => { container.querySelector<HTMLButtonElement>('[data-testid="move-confirm"]')!.click(); });
  expect(api.queue).not.toHaveBeenCalled();
  expect(container.textContent).toContain("목적지 경로가 변경");
  expect(container.querySelector('[data-testid="move-confirm"]')).toBeNull();
  await prepare(container);
  expect(container.textContent).toContain("new-destination");
  await act(async () => { container.querySelector<HTMLButtonElement>('[data-testid="move-confirm"]')!.click(); });
  expect(api.queue).toHaveBeenCalledExactlyOnceWith(binding, { node_id: "document", expected_meta_etag: '"meta"', request });
  await act(async () => { mounted.unmount(); });
});

test("목적지 조상이 목록에 없어 경로를 확인할 수 없으면 이동을 요청하지 않는다", async () => {
  const api = ports(); api.folders = vi.fn(async () => ({ nodes: [root, source, { ...destination, parent_id: "unknown" }], next_after_id: null }));
  const { container, root: mounted, props } = mount(api);
  await act(async () => { mounted.render(createElement(MovePanel, props)); });
  await prepare(container);
  expect(container.textContent).toContain("경로를 확인할 수 없습니다");
  expect(container.querySelector('[data-testid="move-confirm"]')).toBeNull();
  expect(api.queue).not.toHaveBeenCalled();
  await act(async () => { mounted.unmount(); });
});

test("목적지의 조상이 다른 폴더 페이지에 있어도 전체 경로를 확인하고 이동한다", async () => {
  const api = ports();
  api.folders = vi.fn(async (_binding, after) => after === null
    ? { nodes: [destination, source], next_after_id: "source" }
    : { nodes: [root], next_after_id: null });
  const { container, root: mounted, props } = mount(api);
  await act(async () => { mounted.render(createElement(MovePanel, props)); });
  await prepare(container);
  await vi.waitFor(() => expect(container.querySelector('[data-testid="move-confirm"]')).not.toBeNull());
  expect(container.textContent).toContain("destination");
  await act(async () => { container.querySelector<HTMLButtonElement>('[data-testid="move-confirm"]')!.click(); });
  expect(api.queue).toHaveBeenCalledExactlyOnceWith(binding, { node_id: "document", expected_meta_etag: '"meta"', request });
  expect(api.folders).toHaveBeenCalledWith(binding, "source");
  await act(async () => { mounted.unmount(); });
});

test("실제 폴더 이름이 경로 확인 안내문과 같아도 정상적인 전체 경로로 처리한다", async () => {
  const api = ports();
  const sameAsHint = node("hint-parent", "root", "[경로 확인 필요]", "folder");
  const nested = { ...destination, parent_id: "hint-parent" };
  api.folders = vi.fn(async () => ({ nodes: [root, source, sameAsHint, nested], next_after_id: null }));
  const { container, root: mounted, props } = mount(api);
  await act(async () => { mounted.render(createElement(MovePanel, props)); });
  await prepare(container);
  await vi.waitFor(() => expect(container.querySelector('[data-testid="move-confirm"]')).not.toBeNull());
  expect(container.textContent).toContain("[경로 확인 필요]/destination");
  await act(async () => { container.querySelector<HTMLButtonElement>('[data-testid="move-confirm"]')!.click(); });
  expect(api.queue).toHaveBeenCalledExactlyOnceWith(binding, { node_id: "document", expected_meta_etag: '"meta"', request });
  await act(async () => { mounted.unmount(); });
});

test("이 기기에 동기화된 목적지 경로를 표시하고 metadata 갱신 시 기존 확인을 무효화한다", async () => {
  const api = ports(); const { container, root: mounted, props } = mount(api);
  await act(async () => { mounted.render(createElement(MovePanel, { ...props, treeRevision: 0 })); });
  await prepare(container);
  expect(container.textContent).toContain("이 기기에 동기화된 경로");
  expect(container.querySelector('[data-testid="move-confirm"]')).not.toBeNull();
  await act(async () => { mounted.render(createElement(MovePanel, { ...props, treeRevision: 1 })); });
  expect(container.querySelector('[data-testid="move-confirm"]')).toBeNull();
  expect(container.textContent).toMatch(/변경|다시 확인/);
  expect(api.queue).not.toHaveBeenCalled();
  await act(async () => { mounted.unmount(); });
});
