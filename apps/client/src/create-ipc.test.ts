import { beforeEach, expect, test, vi } from "vitest";
import { queueWorkspaceCreate, requestWorkspaceCreateAbandon, workspaceCreateStatus } from "./ipc";
import type { WorkspaceRegistration } from "../../../packages/protocol-types";

const mocked = vi.hoisted(() => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: mocked.invoke }));

const binding: WorkspaceRegistration = { server_origin:"https://example.invalid", workspace_id:"workspace", workspace_epoch:"epoch", device_id:"device", device_stream_id:"stream" };
const parent = "019a1000-0000-7000-8000-000000000001";
const node = "019a1000-0000-7000-8000-000000000002";
const operation = "019a1000-0000-7000-8000-000000000003";
const other = "019a1000-0000-7000-8000-000000000004";
const request = { node_id:node, parent_id:parent, name:"새 폴더", kind:"folder", upload_id:null };
const queued = { operation_id:operation, operation_seq:"1", node_id:node, request, status:"queued", receipt:null };
const createdNode = { node_id:node, parent_id:parent, name:request.name, kind:"folder", deleted:false, content_rev:null, content_etag:null, meta_rev:"meta", meta_etag:'"meta"', node_etag:'"node"', content_hash:null, size:null, children_generation:"0", last_change_seq:"9" };
const receipt = { status:"created", operation_id:operation, operation_seq:"1", workspace_epoch:binding.workspace_epoch, node:createdNode, commit_seq:"9" };
const page = (entry: unknown, pageBinding: WorkspaceRegistration = binding) => ({ binding:pageBinding, entries:[entry], next_after_seq:null });
function respond(value: unknown) {
  mocked.invoke.mockImplementation(async (command: string) => command === "m0_renderer_session" ? "owner" : value);
}
beforeEach(() => mocked.invoke.mockReset());

test("생성 요청은 renderer와 등록을 포함하고 응답의 부모와 이름을 확인한다", async () => {
  respond(queued);
  expect(await queueWorkspaceCreate(binding, { parent_id:parent, name:request.name, kind:"folder" })).toMatchObject({ operation_id:operation, node_id:node, parent_id:parent, name:request.name, status:"queued" });
  expect(mocked.invoke).toHaveBeenCalledWith("workspace_queue_create", { request:{ renderer_session_id:"owner", binding, parent_id:parent, name:request.name, kind:"folder" } });
  for (const result of [
    { ...queued, node_id:other },
    { ...queued, request:{ ...request, parent_id:other } },
    { ...queued, request:{ ...request, name:"다른 이름" } },
    { ...queued, request:{ ...request, kind:"file" } },
    { ...queued, status:"sent" },
    { ...queued, receipt },
  ]) {
    respond(result);
    await expect(queueWorkspaceCreate(binding, { parent_id:parent, name:request.name, kind:"folder" })).rejects.toThrow();
  }
});

test("생성 완료 receipt는 등록과 node, 부모, 이름, 순번과 commit을 확인한다", async () => {
  const committed = { ...queued, status:"committed", receipt };
  respond(page(committed));
  expect((await workspaceCreateStatus(binding,"0")).entries[0]).toMatchObject({ node_id:node, status:"committed", reason:null });
  expect(mocked.invoke).toHaveBeenCalledWith("workspace_create_status", { request:{ renderer_session_id:"owner", binding, after_seq:"0", limit:100 } });
  respond(page(committed, { ...binding, device_stream_id:"other" }));
  await expect(workspaceCreateStatus(binding,"0")).rejects.toThrow();
  const invalid = [
    { ...receipt, operation_id:other },
    { ...receipt, operation_seq:"2" },
    { ...receipt, workspace_epoch:"other" },
    { ...receipt, node:{ ...createdNode, node_id:other } },
    { ...receipt, node:{ ...createdNode, parent_id:other } },
    { ...receipt, node:{ ...createdNode, name:"다른 이름" } },
    { ...receipt, node:{ ...createdNode, deleted:true } },
    { ...receipt, commit_seq:"10" },
    { ...receipt, status:"destination_exists" },
  ];
  for (const value of invalid) {
    respond(page({ ...committed, receipt:value }));
    await expect(workspaceCreateStatus(binding,"0")).rejects.toThrow();
  }
});

test("생성 충돌과 취소는 receipt identity와 상태가 일치해야 하며 페이지 순서를 확인한다", async () => {
  const failure = { status:"destination_exists", operation_id:operation, operation_seq:"1", workspace_epoch:binding.workspace_epoch };
  const failed = { ...queued, status:"failed", receipt:failure };
  respond(page(failed)); expect((await workspaceCreateStatus(binding,"0")).entries[0].reason).toBe("destination_exists");
  const abandoned = { ...queued, status:"abandoned", receipt:{ ...failure, status:"abandoned" } };
  respond(page(abandoned)); expect((await workspaceCreateStatus(binding,"0")).entries[0].status).toBe("abandoned");
  for (const entry of [
    { ...failed, receipt:{ ...failure, workspace_epoch:"other" } },
    { ...failed, receipt:{ ...failure, operation_seq:"2" } },
    { ...failed, receipt:{ ...failure, status:"abandoned" } },
    { ...abandoned, receipt:failure },
    { ...queued, status:"abandoning", receipt:failure },
  ]) {
    respond(page(entry)); await expect(workspaceCreateStatus(binding,"0")).rejects.toThrow();
  }
  respond({ binding, entries:[queued, queued], next_after_seq:null });
  await expect(workspaceCreateStatus(binding,"0")).rejects.toThrow();
  respond({ binding, entries:[queued], next_after_seq:"2" });
  await expect(workspaceCreateStatus(binding,"0")).rejects.toThrow();
});

test("생성 취소는 현재 renderer와 전체 등록을 전달하고 잘못된 식별자를 거절한다", async () => {
  respond(undefined);
  await requestWorkspaceCreateAbandon(binding,operation);
  expect(mocked.invoke).toHaveBeenCalledWith("workspace_request_create_abandon", { request:{ renderer_session_id:"owner", binding, operation_id:operation } });
  mocked.invoke.mockClear();
  await expect(requestWorkspaceCreateAbandon(binding,"invalid")).rejects.toThrow();
  expect(mocked.invoke).not.toHaveBeenCalledWith("workspace_request_create_abandon", expect.anything());
});
