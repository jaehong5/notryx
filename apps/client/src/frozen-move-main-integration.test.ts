// @vitest-environment jsdom
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import type { WorkspaceRegistration } from "../../../packages/protocol-types";
const fakes=vi.hoisted(()=>({root:undefined as {unmount():void}|undefined,active:undefined as WorkspaceRegistration|undefined,listeners:new Map<string,()=>void>(),list:vi.fn(),decide:vi.fn(),send:vi.fn(),kick:vi.fn()}));
vi.mock("react-dom/client",async(original)=>{const actual=await original<typeof import("react-dom/client")>();return {...actual,createRoot:(...args:Parameters<typeof actual.createRoot>)=>{const root=actual.createRoot(...args);fakes.root=root;return root;}};});
vi.mock("@tauri-apps/api/event",()=>({listen:async(name:string,callback:()=>void)=>{fakes.listeners.set(name,callback);return()=>fakes.listeners.delete(name);}}));vi.mock("@tauri-apps/api/core",()=>({invoke:vi.fn()}));
vi.mock("./migration-recovery",()=>({StartupGate:({ready}:{ready:()=>unknown})=>ready()}));
vi.mock("./auto-transmit",async(original)=>{const actual=await original<typeof import("./auto-transmit")>();return {...actual,AutoTransmit:class extends actual.AutoTransmit { override kick(){fakes.kick();super.kick();} }};});
vi.mock("./ipc",async(original)=>{const actual=await original<typeof import("./ipc")>();return {...actual,localDrafts:async()=>[],pendingPairStatus:async()=>"none",workspaceSyncState:async()=>({binding:fakes.active,status:"ready"}),workspaceInventory:async()=>({nodes:[rootNode],next_after_id:null}),workspaceFrozenMoves:fakes.list,resolveWorkspaceFrozenMove:fakes.decide,sendNextWorkspaceOperation:fakes.send};});
const binding:WorkspaceRegistration={server_origin:"https://example.invalid",workspace_id:"workspace",workspace_epoch:"epoch",device_id:"device",device_stream_id:"stream"};
const old={...binding,device_id:"old-device",device_stream_id:"old-stream"};
const rootNode={node_id:"019a1000-0000-7000-8000-000000000001",parent_id:null,name:"",kind:"folder" as const,deleted:false,content_rev:null,content_etag:null,meta_rev:"meta",meta_etag:'"meta"',node_etag:'"root"',content_hash:null,size:null,children_generation:"0",last_change_seq:"1"};
const node={...rootNode,node_id:"019a1000-0000-7000-8000-000000000002",parent_id:rootNode.node_id,name:"Synthetic.md",kind:"file" as const,content_rev:"content",content_etag:'"content"',node_etag:'"current"',content_hash:"sha256:"+"a".repeat(64),size:"1"};
const item={old_binding:old,operation_id:"019a1000-0000-7000-8000-000000000003",operation_seq:"1",node_id:node.node_id,expected_meta_etag:'"old"',request:{expected_parent_id:rootNode.node_id,parent_id:rootNode.node_id,name:"Moved.md"},status:"sent" as const,reason:null,current_node:node,current_destination:rootNode};
beforeEach(()=>{vi.resetModules();fakes.listeners.clear();fakes.active=binding;fakes.kick.mockReset();fakes.list.mockReset().mockResolvedValue({binding,entries:[item],next_after_id:null});fakes.decide.mockReset().mockResolvedValue(undefined);fakes.send.mockReset().mockResolvedValue({binding,status:"idle",operation_id:null,document_id:null,generation:null});});
afterEach(async()=>{fakes.root?.unmount();fakes.root=undefined;await new Promise(resolve=>setTimeout(resolve,0));document.body.replaceChildren();vi.restoreAllMocks();});
async function show(){document.body.innerHTML='<div id="root"></div>';await import("./main");await vi.waitFor(()=>expect([...document.querySelectorAll("button")].some(button=>button.textContent==="이전 이동 요청 대조")).toBe(true));[...document.querySelectorAll("button")].find(button=>button.textContent==="이전 이동 요청 대조")!.click();await vi.waitFor(()=>expect(document.querySelector(`[data-frozen-move="${item.operation_id}"]`)).not.toBeNull());document.querySelector<HTMLButtonElement>(`[data-frozen-move="${item.operation_id}"]`)!.click();}

test("이전 이동 대조에서 현재 상태를 확인한 뒤에만 새 요청을 적용한다",async()=>{
  await show();
  expect(fakes.decide).not.toHaveBeenCalled();
  await vi.waitFor(()=>expect(document.querySelector('[data-testid="frozen-move-apply"]')).not.toBeNull());
  document.querySelector<HTMLButtonElement>('[data-testid="frozen-move-apply"]')!.click();
  expect(fakes.decide).not.toHaveBeenCalled();
  await vi.waitFor(()=>expect(document.querySelector('[data-testid="frozen-move-apply-confirm"]')).not.toBeNull());
  document.querySelector<HTMLButtonElement>('[data-testid="frozen-move-apply-confirm"]')!.click();
  await vi.waitFor(()=>expect(fakes.decide).toHaveBeenCalledExactlyOnceWith(binding,old,item.operation_id,{kind:"apply",expected_meta_etag:'"meta"',request:{expected_parent_id:rootNode.node_id,parent_id:rootNode.node_id,name:"Moved.md"}}));
});

test("패널을 닫은 뒤 현재 등록의 새 요청이 저장되면 scheduler를 깨운다",async()=>{
  let release!:()=>void;fakes.decide.mockImplementation(()=>new Promise<void>(resolve=>{release=resolve;}));
  await show();
  await vi.waitFor(()=>expect(document.querySelector('[data-testid="frozen-move-apply"]')).not.toBeNull());
  document.querySelector<HTMLButtonElement>('[data-testid="frozen-move-apply"]')!.click();
  await vi.waitFor(()=>expect(document.querySelector('[data-testid="frozen-move-apply-confirm"]')).not.toBeNull());
  document.querySelector<HTMLButtonElement>('[data-testid="frozen-move-apply-confirm"]')!.click();
  await vi.waitFor(()=>expect(fakes.decide).toHaveBeenCalledOnce());
  [...document.querySelectorAll('button')].find(button=>button.textContent==='이전 이동 요청 대조')!.click();
  await vi.waitFor(()=>expect(document.querySelector('[aria-label="이전 이동 요청 대조"]')).toBeNull());
  fakes.kick.mockClear();
  release();
  await vi.waitFor(()=>expect(fakes.kick).toHaveBeenCalledOnce(),{timeout:3000});
},12000);

test("패널을 닫은 뒤 이전 등록 응답은 새 등록 scheduler를 깨우지 않는다",async()=>{
  let release!:()=>void;fakes.decide.mockImplementation(()=>new Promise<void>(resolve=>{release=resolve;}));
  await show();
  await vi.waitFor(()=>expect(document.querySelector('[data-testid="frozen-move-apply"]')).not.toBeNull());
  document.querySelector<HTMLButtonElement>('[data-testid="frozen-move-apply"]')!.click();
  await vi.waitFor(()=>expect(document.querySelector('[data-testid="frozen-move-apply-confirm"]')).not.toBeNull());
  document.querySelector<HTMLButtonElement>('[data-testid="frozen-move-apply-confirm"]')!.click();
  await vi.waitFor(()=>expect(fakes.decide).toHaveBeenCalledOnce());
  [...document.querySelectorAll('button')].find(button=>button.textContent==='이전 이동 요청 대조')!.click();
  await vi.waitFor(()=>expect(document.querySelector('[aria-label="이전 이동 요청 대조"]')).toBeNull());
  fakes.active={...binding,device_id:"replacement",device_stream_id:"new-stream"};
  fakes.list.mockImplementation(async(current:WorkspaceRegistration)=>({binding:current,entries:[],next_after_id:null}));
  await vi.waitFor(()=>expect(fakes.listeners.has("workspace-sync-status-changed")).toBe(true));
  fakes.listeners.get("workspace-sync-status-changed")!();
  [...document.querySelectorAll('button')].find(button=>button.textContent==='이전 이동 요청 대조')!.click();
  await vi.waitFor(()=>expect(fakes.list).toHaveBeenCalledWith(fakes.active,null));
  [...document.querySelectorAll('button')].find(button=>button.textContent==='이전 이동 요청 대조')!.click();
  await vi.waitFor(()=>expect(document.querySelector('[aria-label="이전 이동 요청 대조"]')).toBeNull());
  fakes.kick.mockClear();release();
  await new Promise(resolve=>setTimeout(resolve,200));expect(fakes.kick).not.toHaveBeenCalled();
},12000);
