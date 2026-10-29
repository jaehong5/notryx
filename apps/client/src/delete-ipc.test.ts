import { beforeEach, expect, test, vi } from "vitest";
import * as ipc from "./ipc";
import type { WorkspaceRegistration } from "../../../packages/protocol-types";

const mocked = vi.hoisted(() => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: mocked.invoke }));
const binding: WorkspaceRegistration = { server_origin:"https://example.invalid", workspace_id:"workspace", workspace_epoch:"epoch", device_id:"device", device_stream_id:"stream" };
const node = "019a1000-0000-7000-8000-000000000002", operation = "019a1000-0000-7000-8000-000000000003", other = "019a1000-0000-7000-8000-000000000004";
const queued = { operation_id:operation, operation_seq:"1", node_id:node, expected_node_etag:'"node"', status:"queued", receipt:null };
const page = (entry: unknown, value: WorkspaceRegistration = binding) => ({ binding:value, entries:[entry], next_after_seq:null });
function respond(value: unknown) { mocked.invoke.mockImplementation(async (command: string) => command === "m0_renderer_session" ? "owner" : value); }
beforeEach(() => mocked.invoke.mockReset());

test("삭제 요청은 확인한 전체 node ETag와 현재 등록을 그대로 전달하고 다른 응답을 거절한다", async () => {
  respond(queued);
  expect(await ipc.queueWorkspaceDelete(binding, { node_id:node, expected_node_etag:'"node"' })).toMatchObject({ node_id:node, expected_node_etag:'"node"', status:"queued" });
  expect(mocked.invoke).toHaveBeenCalledWith("workspace_queue_delete", { request:{ renderer_session_id:"owner", binding, node_id:node, expected_node_etag:'"node"' } });
  for (const result of [{ ...queued, node_id:other }, { ...queued, expected_node_etag:'"changed"' }, { ...queued, status:"sent" }, { ...queued, receipt:{} }]) {
    respond(result); await expect(ipc.queueWorkspaceDelete(binding, { node_id:node, expected_node_etag:'"node"' })).rejects.toThrow();
  }
});

test("삭제 412는 receipt identity를 검증하고 변경 상태만 표시한다", async () => {
  const receipt = { status:"node_precondition_failed", operation_id:operation, operation_seq:"1", workspace_epoch:binding.workspace_epoch, current_node_etag:'"other"' };
  respond(page({ ...queued, status:"failed", receipt }));
  expect((await ipc.workspaceDeleteStatus(binding,"0")).entries[0]).toMatchObject({ reason:"node_precondition_failed", expected_node_etag:'"node"' });
  for (const bad of [{ ...receipt, operation_id:other }, { ...receipt, operation_seq:"2" }, { ...receipt, workspace_epoch:"other" }, { ...receipt, status:"deleted" }]) {
    respond(page({ ...queued, status:"failed", receipt:bad })); await expect(ipc.workspaceDeleteStatus(binding,"0")).rejects.toThrow();
  }
  for (const current_node_etag of ['"node"', 'W/"new"', '"' + 'x'.repeat(255) + '"']) {
    respond(page({ ...queued, status:"failed", receipt:{ ...receipt, current_node_etag } }));
    await expect(ipc.workspaceDeleteStatus(binding,"0")).rejects.toThrow();
  }
  respond(page({ ...queued, status:"failed", receipt }, { ...binding, device_stream_id:"other" }));
  await expect(ipc.workspaceDeleteStatus(binding,"0")).rejects.toThrow();
});

test("삭제 조건 ETag는 native의 256바이트 한도를 넘기지 않는다", async () => {
  respond(queued);
  await expect(ipc.queueWorkspaceDelete(binding, { node_id:node, expected_node_etag:'"' + 'x'.repeat(255) + '"' })).rejects.toThrow();
  expect(mocked.invoke).not.toHaveBeenCalledWith("workspace_queue_delete", expect.anything());
});

test("삭제 ETag의 비 ASCII 문자는 UTF-8 바이트 기준으로 보존한다", async () => {
  const etag = '"서버😀"';
  respond({ ...queued, expected_node_etag:etag });
  expect((await ipc.queueWorkspaceDelete(binding, { node_id:node, expected_node_etag:etag })).expected_node_etag).toBe(etag);
});

test("삭제 상태의 중복 순번과 잘못된 다음 cursor, 잘못된 취소 식별자를 거절한다", async () => {
  respond({ binding, entries:[queued, queued], next_after_seq:null });
  await expect(ipc.workspaceDeleteStatus(binding,"0")).rejects.toThrow();
  respond({ binding, entries:[queued], next_after_seq:"2" });
  await expect(ipc.workspaceDeleteStatus(binding,"0")).rejects.toThrow();
  mocked.invoke.mockClear();
  await expect(ipc.requestWorkspaceDeleteAbandon(binding,"invalid")).rejects.toThrow();
  expect(mocked.invoke).not.toHaveBeenCalledWith("workspace_request_delete_abandon", expect.anything());
});
