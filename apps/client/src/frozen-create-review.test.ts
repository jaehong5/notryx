// @vitest-environment jsdom
import { act, createElement } from "react";
import { createRoot } from "react-dom/client";
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { FrozenCreateReview, type FrozenCreateReviewItem, type FrozenCreateReviewPorts } from "./frozen-create-review";
import type { WorkspaceRegistration } from "../../../packages/protocol-types";

const binding: WorkspaceRegistration = { server_origin:"https://example.invalid", workspace_id:"workspace", workspace_epoch:"epoch", device_id:"device", device_stream_id:"stream" };
const old: WorkspaceRegistration = { ...binding, device_id:"old-device", device_stream_id:"old-stream" };
const folder = { node_id:"parent", parent_id:null, name:"", kind:"folder" as const, deleted:false };
const sameId = { node_id:"old-node", parent_id:"parent", name:"original", kind:"folder" as const, deleted:false };
const sameName = { node_id:"other-node", parent_id:"parent", name:"original", kind:"folder" as const, deleted:false };
const item: FrozenCreateReviewItem = { old_binding:old, operation_id:"old-operation", operation_seq:"7", node_id:"old-node", parent_id:"old-parent", name:"original", kind:"folder", status:"sent", current_same_id:sameId, current_same_name:[sameName] };
function ports(): FrozenCreateReviewPorts {
  return {
    list: vi.fn(async (current) => ({ binding:current, entries:[item], next_after_id:null })),
    folders: vi.fn(async () => ({ nodes:[folder], next_after_id:null })),
    decide: vi.fn(async () => {}),
  };
}
function change(element: HTMLInputElement | HTMLSelectElement, value: string) {
  const prototype=element instanceof HTMLInputElement ? HTMLInputElement.prototype : HTMLSelectElement.prototype;
  Object.getOwnPropertyDescriptor(prototype,"value")!.set!.call(element,value);
  element.dispatchEvent(new Event(element instanceof HTMLInputElement ? "input" : "change",{bubbles:true}));
}
async function show(api:FrozenCreateReviewPorts) {
  const container=document.createElement("div"); document.body.append(container);const root=createRoot(container);const applied=vi.fn();
  const props={binding,ports:api,onApplied:applied};
  await act(async()=>{root.render(createElement(FrozenCreateReview,props));});
  await vi.waitFor(()=>expect(container.querySelector('[data-review-operation="old-operation"]')).not.toBeNull());
  await act(async()=>{container.querySelector<HTMLButtonElement>('[data-review-operation="old-operation"]')!.click();});
  return {container,root,applied,props};
}
async function setDestination(container:HTMLElement) {
  await act(async()=>{change(container.querySelector<HTMLSelectElement>('[data-testid="review-parent"]')!,"parent");change(container.querySelector<HTMLInputElement>('[data-testid="review-name"]')!,"new name");});
}
beforeEach(()=>vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT",true));
afterEach(()=>{document.body.replaceChildren();vi.unstubAllGlobals();});

test("이전 요청과 현재 같은 ID·이름을 별도로 보여 주고 성공을 추정하지 않는다",async()=>{
  const api=ports(); const {container,root}=await show(api);
  expect(container.textContent).toContain("이전 node ID: old-node");expect(container.textContent).toContain("이전 부모 ID: old-parent");expect(container.textContent).toContain("이전 이름: original");
  expect(container.textContent).toContain("이전 서버 주소: https://example.invalid");expect(container.textContent).toContain("현재 서버 주소: https://example.invalid");
  expect(container.textContent).toContain("전송 후 결과 미확정");expect(container.textContent).toContain("현재 같은 ID: original (old-node");expect(container.textContent).toContain("현재 같은 이름: original (other-node");
  expect(container.textContent).toContain("이전 요청의 서버 결과는 확정하지 않습니다");
  expect(container.textContent).not.toContain("생성 완료");expect(api.decide).not.toHaveBeenCalled();await act(async()=>{root.unmount();});
});

test("같은 workspace의 이전 서버 주소가 달라도 사용자가 대조할 수 있다",async()=>{
  const api=ports(); const previous={...old,server_origin:"https://old.example"};
  api.list=vi.fn(async(current)=>({binding:current,entries:[{...item,old_binding:previous}],next_after_id:null}));
  const {container,root}=await show(api);
  expect(container.textContent).toContain("이전 서버 주소: https://old.example");
  expect(container.textContent).toContain("현재 서버 주소: https://example.invalid");
  expect(api.decide).not.toHaveBeenCalled();await act(async()=>{root.unmount();});
});

test("현재 부모와 이름을 명시한 새 요청만 적용하고 빠른 중복 클릭은 한 번 처리한다",async()=>{
  const api=ports();let release!:()=>void;api.decide=vi.fn(()=>new Promise<void>((resolve)=>{release=resolve;}));
  const {container,root,applied}=await show(api);
  expect(container.querySelector<HTMLButtonElement>('[data-testid="review-apply"]')!.disabled).toBe(true);
  await setDestination(container);
  await act(async()=>{const button=container.querySelector<HTMLButtonElement>('[data-testid="review-apply"]')!;button.click();button.click();});
  expect(api.decide).toHaveBeenCalledExactlyOnceWith(binding,old,"old-operation",{kind:"apply",parent_id:"parent",name:"new name"});expect(applied).not.toHaveBeenCalled();
  await act(async()=>{release();});expect(applied).toHaveBeenCalledOnce();expect(container.textContent).toContain("서버 반영 상태는 새 폴더 또는 문서에서 확인하세요");await act(async()=>{root.unmount();});
});

test("미적용은 별도 확인해야 기록하며 서버 삭제로 표시하지 않는다",async()=>{
  const api=ports();const {container,root,applied}=await show(api);
  await act(async()=>{container.querySelector<HTMLButtonElement>('[data-testid="review-dismiss"]')!.click();});
  expect(api.decide).not.toHaveBeenCalled();expect(container.textContent).toContain("서버 항목은 삭제하지 않습니다");
  await act(async()=>{container.querySelector<HTMLButtonElement>('[data-testid="review-dismiss-confirm"]')!.click();});
  expect(api.decide).toHaveBeenCalledExactlyOnceWith(binding,old,"old-operation",{kind:"dismiss"});expect(applied).not.toHaveBeenCalled();expect(container.textContent).toContain("서버 항목은 삭제하지 않았습니다");await act(async()=>{root.unmount();});
});

test("불확정 응답은 같은 선택만 재시도하고 dependency 오류는 자료를 유지한다",async()=>{
  const api=ports();const decide=vi.fn().mockRejectedValueOnce(new Error("network_lost")).mockRejectedValueOnce(new Error("dependent_operations"));api.decide=decide;
  const {container,root,applied}=await show(api);await setDestination(container);
  await act(async()=>{container.querySelector<HTMLButtonElement>('[data-testid="review-apply"]')!.click();});
  expect(container.textContent).toContain("같은 선택을 다시 확인");expect(container.querySelector<HTMLButtonElement>('[data-testid="review-dismiss"]')!.disabled).toBe(true);
  await act(async()=>{container.querySelector<HTMLButtonElement>('[data-testid="review-retry"]')!.click();});
  expect(decide).toHaveBeenCalledTimes(2);expect(decide.mock.calls[0]).toEqual(decide.mock.calls[1]);
  expect(container.textContent).toContain("연결된 작업이 있어");expect(container.querySelector('[data-review-operation="old-operation"]')).not.toBeNull();expect(applied).not.toHaveBeenCalled();await act(async()=>{root.unmount();});
});

test("다른 workspace 기록을 표시하지 않고 늦은 이전 등록 결과도 적용하지 않는다",async()=>{
  const api=ports();let release!:()=>void;api.decide=vi.fn(()=>new Promise<void>((resolve)=>{release=resolve;}));
  const foreign={...item,old_binding:{...old,workspace_id:"another"},operation_id:"foreign-operation",name:"다른 공간 기록"};
  api.list=vi.fn(async(current)=>({binding:current,entries:[item,foreign],next_after_id:null}));
  const {container,root,props,applied}=await show(api);
  expect(container.textContent).not.toContain("다른 공간 기록");await setDestination(container);
  await act(async()=>{container.querySelector<HTMLButtonElement>('[data-testid="review-apply"]')!.click();});
  const replacement={...binding,device_id:"replacement",device_stream_id:"replacement-stream"};
  api.list=vi.fn(async(current)=>({binding:current,entries:[],next_after_id:null}));
  await act(async()=>{root.render(createElement(FrozenCreateReview,{...props,binding:replacement}));});
  await act(async()=>{release();});
  expect(applied).not.toHaveBeenCalled();expect(container.textContent).not.toContain("새 생성 요청을 이 기기에 저장했습니다");expect(container.textContent).not.toContain("다른 공간 기록");await act(async()=>{root.unmount();});
});

test("등록 변경 뒤 도착한 이전 목록은 새 등록에 노출되지 않는다",async()=>{
  const api=ports();let release!:(page:{binding:WorkspaceRegistration;entries:FrozenCreateReviewItem[];next_after_id:null})=>void;
  api.list=vi.fn((current)=>current.device_id===binding.device_id ? new Promise<{binding:WorkspaceRegistration;entries:FrozenCreateReviewItem[];next_after_id:null}>((resolve)=>{release=resolve;}) : Promise.resolve({binding:current,entries:[],next_after_id:null}));
  const container=document.createElement("div");document.body.append(container);const root=createRoot(container);const applied=vi.fn();
  await act(async()=>{root.render(createElement(FrozenCreateReview,{binding,ports:api,onApplied:applied}));});
  await act(async()=>{root.render(createElement(FrozenCreateReview,{binding:{...binding,device_id:"new-device"},ports:api,onApplied:applied}));});
  await act(async()=>{release({binding,entries:[item],next_after_id:null});});
  expect(container.querySelector('[data-review-operation="old-operation"]')).toBeNull();expect(api.decide).not.toHaveBeenCalled();await act(async()=>{root.unmount();});
});

test("이전 전송 대기 요청과 현재 미발견 상태를 표시한다",async()=>{
  const api=ports();api.list=vi.fn(async(current)=>({binding:current,entries:[{...item,status:"queued" as const,current_same_id:null,current_same_name:[]}],next_after_id:null}));
  const {container,root}=await show(api);
  expect(container.textContent).toContain("전송 대기");expect(container.textContent).toContain("현재 같은 ID: 목록에서 확인되지 않음");expect(container.textContent).toContain("현재 같은 이름: 목록에서 확인되지 않음");
  expect(api.decide).not.toHaveBeenCalled();await act(async()=>{root.unmount();});
});
// Append to apps/client/src/frozen-create-review.test.ts.
// Proposed failed display shape: status:'failed', reason:<validated receipt status>.

test.each([
  ["destination_exists", /이름|목적지|충돌/],
  ["node_id_conflict", /식별|ID|충돌/i],
] as const)("확정 실패 %s는 같은 목록의 미확정 요청과 구분해 보여 준다", async (reason, reasonText) => {
  const api = ports();
  const failed = { ...item, status:"failed", reason } as unknown as FrozenCreateReviewItem;
  const uncertain = { ...item, operation_id:"pending-operation", node_id:"pending-node", name:"pending", status:"sent" as const };
  api.list = vi.fn(async (current) => ({ binding:current, entries:[failed,uncertain], next_after_id:null }));
  const { container, root, applied } = await show(api);
  const failedButton = container.querySelector<HTMLButtonElement>('[data-review-operation="old-operation"]')!;
  const pendingButton = container.querySelector<HTMLButtonElement>('[data-review-operation="pending-operation"]')!;
  expect(failedButton.textContent).toMatch(/실패|거절/);
  expect(failedButton.textContent).not.toContain("미확정");
  expect(pendingButton.textContent).toContain("미확정");
  expect(container.textContent).toMatch(reasonText);
  expect(api.decide).not.toHaveBeenCalled();
  await act(async () => { container.querySelector<HTMLButtonElement>('[data-testid="review-dismiss"]')!.click(); });
  expect(api.decide).not.toHaveBeenCalled();
  expect(container.textContent).toContain("서버 항목은 삭제하지 않습니다");
  await act(async () => { container.querySelector<HTMLButtonElement>('[data-testid="review-dismiss-confirm"]')!.click(); });
  expect(api.decide).toHaveBeenCalledExactlyOnceWith(binding, old, "old-operation", {kind:"dismiss"});
  expect(applied).not.toHaveBeenCalled();
  await act(async () => { root.unmount(); });
});

test("확정 실패만 선택했을 때 이전 결과가 미확정이라고 안내하지 않는다", async () => {
  const api = ports();
  const failed = { ...item, status:"failed", reason:"destination_exists" } as unknown as FrozenCreateReviewItem;
  api.list = vi.fn(async (current) => ({ binding:current, entries:[failed], next_after_id:null }));
  const { container, root } = await show(api);
  expect(container.textContent).toMatch(/실패|거절/);
  expect(container.textContent).not.toContain("서버가 이전 요청을 처리했는지는 현재 상태만으로 확정할 수 없습니다");
  expect(container.textContent).not.toContain("이전 요청의 서버 결과는 확정하지 않습니다");
  expect(api.decide).not.toHaveBeenCalled();
  await act(async () => { root.unmount(); });
});

test("복구 충돌은 이전 요청의 적용 여부를 추정하지 않는다", async () => {
  const api=ports();
  const unresolved={...item,status:"failed",reason:"recovery_conflict"} as unknown as FrozenCreateReviewItem;
  api.list=vi.fn(async(current)=>({binding:current,entries:[unresolved],next_after_id:null}));
  const {container,root}=await show(api);
  expect(container.textContent).toContain("이전 요청의 적용 여부를 확인할 수 없습니다");
  expect(container.textContent).not.toContain("서버 처리 실패");
  expect(container.textContent).not.toContain("서버에서 이전 요청을 처리하지 못했습니다");
  expect(api.decide).not.toHaveBeenCalled();
  await act(async()=>{root.unmount();});
});
