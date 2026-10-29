// @vitest-environment jsdom
import { act, createElement } from "react";
import { createRoot } from "react-dom/client";
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { FrozenMoveReview, type FrozenMoveReviewItem, type FrozenMoveReviewPorts } from "./frozen-move-review";
import type { NodeState, WorkspaceRegistration } from "../../../packages/protocol-types";

const binding: WorkspaceRegistration = { server_origin:"https://example.invalid", workspace_id:"workspace", workspace_epoch:"epoch", device_id:"device", device_stream_id:"stream" };
const old = { ...binding, device_id:"old-device", device_stream_id:"old-stream" };
const node: NodeState = { node_id:"node", parent_id:"current-parent", name:"Source.md", kind:"file", deleted:false, content_rev:"content", content_etag:'"content"', meta_rev:"current", meta_etag:'"current"', node_etag:'"node"', content_hash:"sha256:" + "a".repeat(64), size:"1", children_generation:"0", last_change_seq:"2" };
const destination: NodeState = { node_id:"destination", parent_id:"root", name:"Destination", kind:"folder", deleted:false, content_rev:null, content_etag:null, meta_rev:"folder", meta_etag:'"folder"', node_etag:'"folder-node"', content_hash:null, size:null, children_generation:"0", last_change_seq:"2" };
const item: FrozenMoveReviewItem = { old_binding:old, operation_id:"old-operation", operation_seq:"7", node_id:"node", expected_meta_etag:'"old"', request:{expected_parent_id:"old-parent", parent_id:"destination", name:"Moved.md"}, status:"sent", reason:null, current_node:node, current_destination:destination };
function ports(): FrozenMoveReviewPorts { return { list:vi.fn(async (current) => ({binding:current, entries:[item], next_after_id:null})), decide:vi.fn(async () => {}) }; }
async function show(api:FrozenMoveReviewPorts) {
  const container = document.createElement("div"); document.body.append(container); const root = createRoot(container); const applied = vi.fn();
  const props = {binding, ports:api, onApplied:applied};
  await act(async () => { root.render(createElement(FrozenMoveReview, props)); });
  await vi.waitFor(() => expect(container.querySelector('[data-frozen-move="old-operation"]')).not.toBeNull());
  await act(async () => { container.querySelector<HTMLButtonElement>('[data-frozen-move="old-operation"]')!.click(); });
  return {container, root, props, applied};
}
beforeEach(() => vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true));
afterEach(() => { document.body.replaceChildren(); vi.unstubAllGlobals(); });

test("이전 이동과 현재 상태를 구분하고 현재 상태만으로 이전 결과를 단정하지 않는다", async () => {
  const api = ports(), {container, root} = await show(api);
  expect(container.textContent).toContain("전송 후 결과 미확정");
  expect(container.textContent).toContain("Source.md"); expect(container.textContent).toContain("Moved.md"); expect(container.textContent).toContain("Destination");
  expect(container.textContent).toContain("현재 상태만으로 이전 요청의 결과를 확정할 수 없습니다");
  expect(api.decide).not.toHaveBeenCalled(); await act(async () => { root.unmount(); });
});

test.each(["old-parent", "destination"])("동일 이름 문서의 현재 부모 %s를 구분해 표시한다", async (currentParent) => {
  const api = ports();
  api.list = vi.fn(async (current) => ({binding:current, entries:[{...item, current_node:{...node, name:"Moved.md", parent_id:currentParent}}], next_after_id:null}));
  const {container, root} = await show(api);
  expect(container.textContent).toContain(`현재 부모: ${currentParent}`);
  expect(api.decide).not.toHaveBeenCalled();
  await act(async () => {root.unmount();});
});

test("새 이동은 현재 문서와 목적지를 재확인하고 현재 meta ETag와 부모로 한 번 요청한다", async () => {
  const api = ports(); let release!:() => void; api.decide = vi.fn(() => new Promise<void>((resolve) => {release = resolve;}));
  const {container, root, applied} = await show(api);
  await act(async () => { container.querySelector<HTMLButtonElement>('[data-testid="frozen-move-apply"]')!.click(); });
  expect(api.decide).not.toHaveBeenCalled(); expect(container.textContent).toContain("상대 링크");
  await act(async () => { const button = container.querySelector<HTMLButtonElement>('[data-testid="frozen-move-apply-confirm"]')!; button.click(); button.click(); });
  expect(api.decide).toHaveBeenCalledExactlyOnceWith(binding, old, "old-operation", { kind:"apply", expected_meta_etag:'"current"', request:{expected_parent_id:"current-parent", parent_id:"destination", name:"Moved.md"} });
  expect(applied).not.toHaveBeenCalled(); await act(async () => { release(); }); expect(applied).toHaveBeenCalledOnce();
  await act(async () => { root.unmount(); });
});

test("미적용도 별도 확인하고 이전 서버 결과를 단정하지 않는다", async () => {
  const api = ports(), {container, root, applied} = await show(api);
  await act(async () => { container.querySelector<HTMLButtonElement>('[data-testid="frozen-move-dismiss"]')!.click(); });
  expect(api.decide).not.toHaveBeenCalled(); expect(container.textContent).toContain("이전 요청의 결과는 확인되지 않았을 수 있습니다");
  await act(async () => { container.querySelector<HTMLButtonElement>('[data-testid="frozen-move-dismiss-confirm"]')!.click(); });
  expect(api.decide).toHaveBeenCalledExactlyOnceWith(binding, old, "old-operation", {kind:"dismiss"}); expect(applied).not.toHaveBeenCalled();
  await act(async () => { root.unmount(); });
});

test.each([
  {current_node:null, current_destination:destination},
  {current_node:{...node, deleted:true}, current_destination:destination},
  {current_node:node, current_destination:null},
  {current_node:node, current_destination:{...destination, deleted:true}},
  {current_node:node, current_destination:{...destination, kind:"file" as const}},
])("현재 문서나 목적지를 확인하지 못하면 새 이동을 막는다", async (state) => {
  const api = ports(); api.list = vi.fn(async (current) => ({binding:current, entries:[{...item, ...state}], next_after_id:null}));
  const {container, root} = await show(api);
  expect(container.querySelector<HTMLButtonElement>('[data-testid="frozen-move-apply"]')!.disabled).toBe(true);
  expect(api.decide).not.toHaveBeenCalled(); await act(async () => { root.unmount(); });
});

test("불확정 결정은 같은 선택만 다시 확인하며 등록 교체 뒤 늦은 응답은 반영하지 않는다", async () => {
  const api = ports(); let release!:() => void; api.decide = vi.fn().mockRejectedValueOnce(new Error("lost response")).mockImplementationOnce(() => new Promise<void>((resolve) => {release = resolve;}));
  const {container, root, props, applied} = await show(api);
  await act(async () => { container.querySelector<HTMLButtonElement>('[data-testid="frozen-move-apply"]')!.click(); });
  await act(async () => { container.querySelector<HTMLButtonElement>('[data-testid="frozen-move-apply-confirm"]')!.click(); });
  expect(container.querySelector<HTMLButtonElement>('[data-testid="frozen-move-dismiss"]')!.disabled).toBe(true);
  await act(async () => { container.querySelector<HTMLButtonElement>('[data-testid="frozen-move-retry"]')!.click(); });
  expect(vi.mocked(api.decide).mock.calls[0]).toEqual(vi.mocked(api.decide).mock.calls[1]);
  api.list = vi.fn(async (current) => ({binding:current, entries:[], next_after_id:null}));
  await act(async () => { root.render(createElement(FrozenMoveReview, {...props, binding:{...binding, device_id:"new-device"}})); });
  await act(async () => {release();}); expect(applied).not.toHaveBeenCalled();
  await act(async () => {root.unmount();});
});
