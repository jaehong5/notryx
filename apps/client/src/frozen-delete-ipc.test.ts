import { beforeEach, expect, test, vi } from "vitest";
import * as ipc from "./ipc";
import type { WorkspaceRegistration } from "../../../packages/protocol-types";

const mocked=vi.hoisted(()=>({invoke:vi.fn()}));
vi.mock("@tauri-apps/api/core",()=>({invoke:mocked.invoke}));
const binding:WorkspaceRegistration={server_origin:"https://example.invalid",workspace_id:"workspace",workspace_epoch:"epoch",device_id:"device",device_stream_id:"stream"};
const old:WorkspaceRegistration={...binding,device_id:"old-device",device_stream_id:"old-stream"};
const nodeId="019a1000-0000-7000-8000-000000000002", operation="019a1000-0000-7000-8000-000000000003", other="019a1000-0000-7000-8000-000000000004";
const node={node_id:nodeId,parent_id:"019a1000-0000-7000-8000-000000000001",name:"Synthetic.md",kind:"file",deleted:false,content_rev:"content",content_etag:'"content"',meta_rev:"meta",meta_etag:'"meta"',node_etag:'"current"',content_hash:"sha256:"+"a".repeat(64),size:"1",children_generation:"0",last_change_seq:"2"};
const item={old_binding:old,operation_id:operation,operation_seq:"1",node_id:nodeId,expected_node_etag:'"old"',status:"sent",receipt:null,current_node:node};
const page=(value:unknown,current=binding)=>({binding:current,entries:[value],next_after_id:null});
function respond(value:unknown){mocked.invoke.mockImplementation(async(command:string)=>command==="m0_renderer_session"?"owner":value);}
beforeEach(()=>mocked.invoke.mockReset());

test("이전 삭제 목록은 현재 등록과 이전 작업, 현재 같은 node를 검증한다",async()=>{
  respond(page(item));
  expect((await ipc.workspaceFrozenDeletes(binding,null)).entries[0]).toMatchObject({old_binding:old,operation_id:operation,node_id:nodeId,expected_node_etag:'"old"',current_node:{node_id:nodeId}});
  expect(mocked.invoke).toHaveBeenCalledWith("workspace_frozen_deletes",{request:{renderer_session_id:"owner",binding,after_id:null,limit:100}});
  for(const bad of [
    {...item,old_binding:binding}, {...item,old_binding:{...old,workspace_id:"other"}},
    {...item,current_node:{...node,node_id:other}}, {...item,status:"committed"},
    {...item,receipt:{}}, {...item,expected_node_etag:'W/"old"'},
  ]) { respond(page(bad)); await expect(ipc.workspaceFrozenDeletes(binding,null)).rejects.toThrow(); }
  respond(page(item,{...binding,device_stream_id:"other"}));await expect(ipc.workspaceFrozenDeletes(binding,null)).rejects.toThrow();
});

test("이전 412 결과는 이전 등록과 operation identity를 검증한다",async()=>{
  const receipt={status:"node_precondition_failed",operation_id:operation,operation_seq:"1",workspace_epoch:old.workspace_epoch,current_node_etag:'"new"'};
  respond(page({...item,status:"failed",receipt}));
  expect((await ipc.workspaceFrozenDeletes(binding,null)).entries[0].reason).toBe("node_precondition_failed");
  for(const bad of [{...receipt,operation_id:other},{...receipt,operation_seq:"2"},{...receipt,workspace_epoch:"other"},{...receipt,current_node_etag:'"old"'},{...receipt,status:"deleted"}]) {
    respond(page({...item,status:"failed",receipt:bad}));await expect(ipc.workspaceFrozenDeletes(binding,null)).rejects.toThrow();
  }
});

test("새 삭제 결정은 현재 확인한 ETag와 이전 binding을 그대로 넘긴다",async()=>{
  respond(undefined);
  await ipc.resolveWorkspaceFrozenDelete(binding,old,operation,{kind:"apply",expected_node_etag:'"current"'});
  expect(mocked.invoke).toHaveBeenCalledWith("workspace_resolve_frozen_delete",{request:{renderer_session_id:"owner",binding,old_binding:old,operation_id:operation,choice:{kind:"apply",expected_node_etag:'"current"'}}});
  await expect(ipc.resolveWorkspaceFrozenDelete(binding,binding,operation,{kind:"dismiss"})).rejects.toThrow();
  await expect(ipc.resolveWorkspaceFrozenDelete(binding,{...old,workspace_id:"other"},operation,{kind:"dismiss"})).rejects.toThrow();
  await expect(ipc.resolveWorkspaceFrozenDelete(binding,old,operation,{kind:"apply",expected_node_etag:'W/"bad"'})).rejects.toThrow();
});
