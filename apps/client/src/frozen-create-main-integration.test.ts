// @vitest-environment jsdom
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import type { WorkspaceRegistration } from "../../../packages/protocol-types";

const fakes = vi.hoisted(() => ({
  root:undefined as { unmount():void } | undefined,
  listeners:new Map<string,()=>void>(),
  active:undefined as WorkspaceRegistration | undefined,
  phase:"none" as "none" | "queued" | "committed",
  now:0,
  list:vi.fn(), decide:vi.fn(), send:vi.fn(), open:vi.fn(), kick:vi.fn(),
}));
vi.mock("react-dom/client", async (original) => {
  const actual=await original<typeof import("react-dom/client")>();
  return { ...actual, createRoot:(...args:Parameters<typeof actual.createRoot>) => { const root=actual.createRoot(...args);fakes.root=root;return root; } };
});
vi.mock("@tauri-apps/api/event",()=>({ listen:async (name:string,callback:()=>void)=>{fakes.listeners.set(name,callback);return ()=>fakes.listeners.delete(name);} }));
vi.mock("@tauri-apps/api/core",()=>({invoke:vi.fn()}));
vi.mock("./migration-recovery",()=>({StartupGate:({ready}:{ready:()=>unknown})=>ready()}));
vi.mock("./auto-transmit",async(original)=>{const actual=await original<typeof import("./auto-transmit")>();return {...actual,AutoTransmit:class extends actual.AutoTransmit { override kick(){fakes.kick();super.kick();} }};});
vi.mock("./ipc",async(original)=>{
  const actual=await original<typeof import("./ipc")>();
  return { ...actual,
    localDrafts:async()=>[], pendingPairStatus:async()=>"none",
    workspaceSyncState:async()=>({binding:fakes.active,status:"ready"}),
    workspaceInventory:async()=>({nodes:[rootNode],next_after_id:null}),
    workspaceCreateParents:async()=>({nodes:[rootNode],next_after_id:null}),
    workspaceFrozenCreates:fakes.list, resolveWorkspaceFrozenCreate:fakes.decide,
    sendNextWorkspaceOperation:fakes.send, openWorkspaceDocument:fakes.open,
  };
});
const binding:WorkspaceRegistration={server_origin:"https://example.invalid",workspace_id:"workspace",workspace_epoch:"epoch",device_id:"device",device_stream_id:"stream"};
const old:WorkspaceRegistration={...binding,device_id:"old-device",device_stream_id:"old-stream"};
const replacement:WorkspaceRegistration={...binding,device_id:"new-device",device_stream_id:"new-stream"};
const rootId="019a1000-0000-7000-8000-000000000001";
const nodeId="019a1000-0000-7000-8000-000000000002";
const operationId="019a1000-0000-7000-8000-000000000003";
const newId="019a1000-0000-7000-8000-000000000004";
const newOperationId="019a1000-0000-7000-8000-000000000005";
const rootNode={node_id:rootId,parent_id:null,name:"",kind:"folder" as const,deleted:false,content_rev:null,content_etag:null,meta_rev:"meta",meta_etag:'"meta"',node_etag:'"node"',content_hash:null,size:null,children_generation:"0",last_change_seq:"1"};
const item={old_binding:old,operation_id:operationId,operation_seq:"1",node_id:nodeId,parent_id:rootId,name:"이전 메모",kind:"folder" as const,status:"sent" as const,current_same_id:null,current_same_name:[]};

beforeEach(()=>{
  vi.resetModules();fakes.listeners.clear();fakes.active=binding;fakes.phase="none";fakes.now=0;
  vi.spyOn(performance,"now").mockImplementation(()=>fakes.now);
  fakes.list.mockReset();fakes.decide.mockReset();fakes.send.mockReset();fakes.open.mockReset();fakes.kick.mockReset();
  fakes.list.mockImplementation(async(current:WorkspaceRegistration)=>({binding:current,entries:current.device_id===binding.device_id?[item]:[],next_after_id:null}));
  fakes.decide.mockImplementation(async()=>{fakes.phase="queued";});
  fakes.send.mockImplementation(async()=>{
    if(fakes.phase==="queued"){fakes.phase="committed";return {binding,status:"committed",operation_id:newOperationId,document_id:newId,generation:null};}
    return {binding:fakes.active,status:"idle",operation_id:null,document_id:null,generation:null};
  });
});
afterEach(async()=>{fakes.root?.unmount();fakes.root=undefined;await new Promise(resolve=>setTimeout(resolve,0));document.body.replaceChildren();vi.restoreAllMocks();});
function change(element:HTMLInputElement|HTMLSelectElement,value:string){
  const prototype=element instanceof HTMLInputElement?HTMLInputElement.prototype:HTMLSelectElement.prototype;
  Object.getOwnPropertyDescriptor(prototype,"value")!.set!.call(element,value);
  element.dispatchEvent(new Event(element instanceof HTMLInputElement?"input":"change",{bubbles:true}));
}
async function showReview(){
  document.body.innerHTML='<div id="root"></div>';await import("./main");
  await vi.waitFor(()=>expect([...document.querySelectorAll("button")].some(button=>button.textContent==="이전 생성 요청 대조")).toBe(true));
  [...document.querySelectorAll("button")].find(button=>button.textContent==="이전 생성 요청 대조")!.click();
  await vi.waitFor(()=>expect(document.querySelector('[aria-label="이전 생성 요청 대조"]')).not.toBeNull());
}
async function selectAndApply(){
  await vi.waitFor(()=>expect(document.querySelector(`[data-review-operation="${operationId}"]`)).not.toBeNull());
  document.querySelector<HTMLButtonElement>(`[data-review-operation="${operationId}"]`)!.click();
  await vi.waitFor(()=>expect(document.querySelector(`[data-testid="review-parent"] option[value="${rootId}"]`)).not.toBeNull());
  change(document.querySelector<HTMLSelectElement>('[data-testid="review-parent"]')!,rootId);
  change(document.querySelector<HTMLInputElement>('[data-testid="review-name"]')!,"새 메모");
  await vi.waitFor(()=>expect(document.querySelector<HTMLButtonElement>('[data-testid="review-apply"]')!.disabled).toBe(false));
  document.querySelector<HTMLButtonElement>('[data-testid="review-apply"]')!.click();
}
async function replaceRegistration(){
  fakes.active=replacement;await vi.waitFor(()=>expect(fakes.listeners.has("workspace-sync-status-changed")).toBe(true));
  fakes.listeners.get("workspace-sync-status-changed")!();
  await vi.waitFor(()=>expect(fakes.list).toHaveBeenCalledWith(replacement,null));
}

test("대조 패널에서 적용한 새 요청은 scheduler를 깨워 전송한다",async()=>{
  await showReview();expect(fakes.list).toHaveBeenCalledWith(binding,null);
  fakes.now=1000;await selectAndApply();
  await vi.waitFor(()=>expect(fakes.decide).toHaveBeenCalledExactlyOnceWith(binding,old,operationId,{kind:"apply",parent_id:rootId,name:"새 메모"}));
  await vi.waitFor(()=>expect(fakes.phase).toBe("committed"),{timeout:5000});
  expect(fakes.send).toHaveBeenCalled();expect(fakes.open).not.toHaveBeenCalled();
  expect(document.body.textContent).toContain("결과가 미확정인 요청은 현재 목록만으로 이전 처리 여부를 판단할 수 없습니다.");
});

test("등록 교체 뒤 이전 결정의 늦은 완료는 화면이나 scheduler를 갱신하지 않는다",async()=>{
  let release!:()=>void;fakes.decide.mockImplementation(()=>new Promise<void>(resolve=>{release=resolve;}));
  await showReview();await selectAndApply();await vi.waitFor(()=>expect(fakes.decide).toHaveBeenCalledOnce());
  await replaceRegistration();const sendsBefore=fakes.send.mock.calls.length;
  release();await new Promise(resolve=>setTimeout(resolve,0));
  expect(document.body.textContent).not.toContain("새 생성 요청을 이 기기에 저장했습니다");
  expect(fakes.send.mock.calls.length).toBe(sendsBefore);expect(fakes.open).not.toHaveBeenCalled();
});

test("등록 교체 뒤 이전 목록의 지연 응답은 새 등록에 노출되지 않는다",async()=>{
  let release!:(value:{binding:WorkspaceRegistration;entries:(typeof item)[];next_after_id:null})=>void;
  fakes.list.mockImplementation((current:WorkspaceRegistration)=>current.device_id===binding.device_id ? new Promise(resolve=>{release=resolve;}) : Promise.resolve({binding:current,entries:[],next_after_id:null}));
  await showReview();await vi.waitFor(()=>expect(fakes.list).toHaveBeenCalledWith(binding,null));
  await replaceRegistration();release({binding,entries:[item],next_after_id:null});
  await new Promise(resolve=>setTimeout(resolve,0));
  expect(document.querySelector(`[data-review-operation="${operationId}"]`)).toBeNull();
  expect(fakes.decide).not.toHaveBeenCalled();
});

test("패널을 닫은 뒤 생성 대조의 새 요청이 저장되면 scheduler를 깨운다",async()=>{
  let release!:()=>void;fakes.decide.mockImplementation(()=>new Promise<void>(resolve=>{release=resolve;}));
  await showReview();await selectAndApply();await vi.waitFor(()=>expect(fakes.decide).toHaveBeenCalledOnce());
  [...document.querySelectorAll("button")].find(button=>button.textContent==="이전 생성 요청 대조")!.click();
  await vi.waitFor(()=>expect(document.querySelector('[aria-label="이전 생성 요청 대조"]')).toBeNull());
  fakes.kick.mockClear();release();
  await vi.waitFor(()=>expect(fakes.kick).toHaveBeenCalledOnce(),{timeout:3000});
},12000);

test("생성 대조를 닫고 등록을 교체한 뒤 이전 응답은 새 scheduler를 깨우지 않는다",async()=>{
  let release!:()=>void;fakes.decide.mockImplementation(()=>new Promise<void>(resolve=>{release=resolve;}));
  await showReview();await selectAndApply();await vi.waitFor(()=>expect(fakes.decide).toHaveBeenCalledOnce());
  [...document.querySelectorAll("button")].find(button=>button.textContent==="이전 생성 요청 대조")!.click();
  await vi.waitFor(()=>expect(document.querySelector('[aria-label="이전 생성 요청 대조"]')).toBeNull());
  fakes.active=replacement;await vi.waitFor(()=>expect(fakes.listeners.has("workspace-sync-status-changed")).toBe(true));
  fakes.listeners.get("workspace-sync-status-changed")!();
  [...document.querySelectorAll("button")].find(button=>button.textContent==="이전 생성 요청 대조")!.click();
  await vi.waitFor(()=>expect(fakes.list).toHaveBeenCalledWith(replacement,null));
  [...document.querySelectorAll("button")].find(button=>button.textContent==="이전 생성 요청 대조")!.click();
  await vi.waitFor(()=>expect(document.querySelector('[aria-label="이전 생성 요청 대조"]')).toBeNull());
  fakes.kick.mockClear();release();await new Promise(resolve=>setTimeout(resolve,200));expect(fakes.kick).not.toHaveBeenCalled();
},12000);
