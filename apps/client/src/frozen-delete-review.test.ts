// @vitest-environment jsdom
import { act, createElement } from "react";
import { createRoot } from "react-dom/client";
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { FrozenDeleteReview, type FrozenDeleteReviewItem, type FrozenDeleteReviewPorts } from "./frozen-delete-review";
import type { NodeState, WorkspaceRegistration } from "../../../packages/protocol-types";

const binding: WorkspaceRegistration = { server_origin:"https://example.invalid", workspace_id:"workspace", workspace_epoch:"epoch", device_id:"device", device_stream_id:"stream" };
const old: WorkspaceRegistration = { ...binding, device_id:"old-device", device_stream_id:"old-stream" };
const node: NodeState = { node_id:"node", parent_id:"parent", name:"Synthetic.md", kind:"file", deleted:false, content_rev:"content", content_etag:'"content"', meta_rev:"meta", meta_etag:'"meta"', node_etag:'"current-node"', content_hash:"sha256:" + "a".repeat(64), size:"1", children_generation:"0", last_change_seq:"2" };
const item: FrozenDeleteReviewItem = { old_binding:old, operation_id:"old-operation", operation_seq:"7", node_id:"node", expected_node_etag:'"old-node"', status:"sent", reason:null, current_node:node };
function ports(): FrozenDeleteReviewPorts {
  return { list:vi.fn(async(current)=>({binding:current,entries:[item],next_after_id:null})), decide:vi.fn(async()=>{}) };
}
async function show(api:FrozenDeleteReviewPorts) {
  const container=document.createElement("div");document.body.append(container);const root=createRoot(container);const applied=vi.fn();
  const props={binding,ports:api,onApplied:applied};
  await act(async()=>{root.render(createElement(FrozenDeleteReview,props));});
  await vi.waitFor(()=>expect(container.querySelector('[data-frozen-delete="old-operation"]')).not.toBeNull());
  await act(async()=>{container.querySelector<HTMLButtonElement>('[data-frozen-delete="old-operation"]')!.click();});
  return {container,root,applied,props};
}
beforeEach(()=>vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT",true));
afterEach(()=>{document.body.replaceChildren();vi.unstubAllGlobals();});

test("이전 요청과 현재 문서를 보여 주되 현재 존재로 이전 적용 여부를 추정하지 않는다",async()=>{
  const api=ports();const {container,root}=await show(api);
  expect(container.textContent).toContain("전송 후 결과 미확정");expect(container.textContent).toContain("Synthetic.md");
  expect(container.textContent).toContain("현재 상태만으로 이전 요청의 결과를 확정할 수 없습니다");
  expect(api.decide).not.toHaveBeenCalled();await act(async()=>{root.unmount();});
});

test("새 삭제는 현재 문서를 명시적으로 재확인하고 현재 ETag로만 요청한다",async()=>{
  const api=ports();let release!:()=>void;api.decide=vi.fn(()=>new Promise<void>((resolve)=>{release=resolve;}));
  const {container,root,applied}=await show(api);
  await act(async()=>{container.querySelector<HTMLButtonElement>('[data-testid="frozen-delete-apply"]')!.click();});
  expect(api.decide).not.toHaveBeenCalled();expect(container.textContent).toContain("휴지통");
  await act(async()=>{const button=container.querySelector<HTMLButtonElement>('[data-testid="frozen-delete-apply-confirm"]')!;button.click();button.click();});
  expect(api.decide).toHaveBeenCalledExactlyOnceWith(binding,old,"old-operation",{kind:"apply",expected_node_etag:'"current-node"'});
  expect(applied).not.toHaveBeenCalled();await act(async()=>{release();});expect(applied).toHaveBeenCalledOnce();
  await act(async()=>{root.unmount();});
});

test("미적용도 별도 확인하고 이전 결과를 추정하지 않는다",async()=>{
  const api=ports();const {container,root,applied}=await show(api);
  await act(async()=>{container.querySelector<HTMLButtonElement>('[data-testid="frozen-delete-dismiss"]')!.click();});
  expect(api.decide).not.toHaveBeenCalled();expect(container.textContent).toContain("새 삭제 요청을 만들지 않습니다");
  expect(container.textContent).toContain("이전 요청의 결과는 확인되지 않았을 수 있습니다");
  await act(async()=>{container.querySelector<HTMLButtonElement>('[data-testid="frozen-delete-dismiss-confirm"]')!.click();});
  expect(api.decide).toHaveBeenCalledExactlyOnceWith(binding,old,"old-operation",{kind:"dismiss"});expect(applied).not.toHaveBeenCalled();
  expect(container.textContent).toContain("새 삭제 요청을 만들지 않았습니다");
  expect(container.textContent).not.toContain("서버 문서는 삭제하지 않았습니다");
  await act(async()=>{root.unmount();});
});

test.each([null,{...node,deleted:true},{...node,kind:"folder" as const}])("현재 문서가 없거나 삭제됐거나 파일이 아니면 새 삭제를 막는다",async(current_node)=>{
  const api=ports();api.list=vi.fn(async(current)=>({binding:current,entries:[{...item,current_node}],next_after_id:null}));
  const {container,root}=await show(api);
  expect(container.querySelector<HTMLButtonElement>('[data-testid="frozen-delete-apply"]')!.disabled).toBe(true);
  expect(api.decide).not.toHaveBeenCalled();await act(async()=>{root.unmount();});
});

test("불확정 응답은 다른 선택을 막고 이전과 같은 선택만 재확인한다",async()=>{
  const api=ports();api.decide=vi.fn().mockRejectedValueOnce(new Error("lost response")).mockResolvedValueOnce(undefined);
  const {container,root}=await show(api);
  await act(async()=>{container.querySelector<HTMLButtonElement>('[data-testid="frozen-delete-apply"]')!.click();});
  await act(async()=>{container.querySelector<HTMLButtonElement>('[data-testid="frozen-delete-apply-confirm"]')!.click();});
  expect(container.textContent).toContain("같은 선택을 다시 확인");
  expect(container.querySelector<HTMLButtonElement>('[data-testid="frozen-delete-dismiss"]')!.disabled).toBe(true);
  await act(async()=>{container.querySelector<HTMLButtonElement>('[data-testid="frozen-delete-retry"]')!.click();});
  expect(vi.mocked(api.decide).mock.calls[0]).toEqual(vi.mocked(api.decide).mock.calls[1]);
  await act(async()=>{root.unmount();});
});

test("재확인한 문서의 ETag가 이미 바뀌었다면 현재 상태를 다시 확인한다",async()=>{
  const api=ports();api.decide=vi.fn().mockRejectedValueOnce(new Error("InvalidRequest"));
  const {container,root,applied}=await show(api);
  await act(async()=>{container.querySelector<HTMLButtonElement>('[data-testid="frozen-delete-apply"]')!.click();});
  await act(async()=>{container.querySelector<HTMLButtonElement>('[data-testid="frozen-delete-apply-confirm"]')!.click();});
  expect(container.textContent).toContain("현재 문서 상태가 변경");
  expect(container.querySelector('[data-testid="frozen-delete-retry"]')).toBeNull();
  expect(api.decide).toHaveBeenCalledOnce();expect(applied).not.toHaveBeenCalled();
  await act(async()=>{root.unmount();});
});

test("재등록 뒤 늦은 이전 응답은 새 등록의 화면과 scheduler에 반영하지 않는다",async()=>{
  const api=ports();let release!:()=>void;api.decide=vi.fn(()=>new Promise<void>((resolve)=>{release=resolve;}));
  const {container,root,props,applied}=await show(api);
  await act(async()=>{container.querySelector<HTMLButtonElement>('[data-testid="frozen-delete-apply"]')!.click();});
  await act(async()=>{container.querySelector<HTMLButtonElement>('[data-testid="frozen-delete-apply-confirm"]')!.click();});
  api.list=vi.fn(async(current)=>({binding:current,entries:[],next_after_id:null}));
  await act(async()=>{root.render(createElement(FrozenDeleteReview,{...props,binding:{...binding,device_id:"new-device"}}));});
  await act(async()=>{release();});
  expect(applied).not.toHaveBeenCalled();expect(container.textContent).not.toContain("새 삭제 요청을 이 기기에 저장했습니다");
  await act(async()=>{root.unmount();});
});
