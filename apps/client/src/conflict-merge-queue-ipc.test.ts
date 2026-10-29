import { beforeEach, expect, test, vi } from "vitest";
import * as ipc from "./ipc";
import type { WorkspaceContentConflictProgressResponse, WorkspaceRegistration } from "../../../packages/protocol-types";

const mocked = vi.hoisted(() => ({ invoke:vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke:mocked.invoke }));

const binding:WorkspaceRegistration = { server_origin:"https://example.invalid", workspace_id:"workspace", workspace_epoch:"epoch", device_id:"device", device_stream_id:"stream" };
const documentId = "019a1000-0000-7000-8000-000000000002";
const conflictId = "019a1000-0000-7000-8000-000000000003";
const mergeId = "019a1000-0000-7000-8000-000000000004";
const session = "edit-session";
type Queue = (binding:WorkspaceRegistration, documentId:string, sessionId:string, operationId:string, version:string) => Promise<WorkspaceContentConflictProgressResponse>;
const queue:Queue = (...args) => (ipc as unknown as { queueWorkspaceContentConflictMerge:Queue }).queueWorkspaceContentConflictMerge(...args);

function respond(value:unknown) {
  mocked.invoke.mockImplementation(async (command:string) => command === "m0_renderer_session" ? "owner" : value);
}
beforeEach(() => mocked.invoke.mockReset());

test("합본 전송 준비는 등록과 문서, session, 충돌 작업 및 확인 version을 전달한다", async () => {
  const value = { status:"pending", version:"2", requested_operation_id:mergeId };
  respond(value);
  await expect(queue(binding, documentId, session, conflictId, "2")).resolves.toEqual(value);
  expect(mocked.invoke).toHaveBeenCalledWith("workspace_queue_content_conflict_merge", { request:{
    renderer_session_id:"owner", binding, document_id:documentId, edit_session_id:session, operation_id:conflictId, expected_intent_version:"2",
  } });
});

test("동일한 전송 준비 재조회는 동일 요청과 응답의 합본 operation ID를 보존한다", async () => {
  const value = { status:"pending", version:"2", requested_operation_id:mergeId };
  respond(value);
  const first = await queue(binding, documentId, session, conflictId, "2");
  const retry = await queue(binding, documentId, session, conflictId, "2");
  expect(first).toEqual(value);
  expect(retry).toEqual(first);
  const requests = mocked.invoke.mock.calls.filter(([command]) => command === "workspace_queue_content_conflict_merge");
  expect(requests).toHaveLength(2);
  expect(requests[0][1]).toEqual(requests[1][1]);
});

test("잘못된 등록·문서·충돌 작업·session·version은 native 호출 전에 거절한다", async () => {
  respond({ status:"pending", version:"2", requested_operation_id:mergeId });
  for (const args of [
    [null as unknown as WorkspaceRegistration, documentId, session, conflictId, "2"],
    [{ ...binding, workspace_epoch:null } as unknown as WorkspaceRegistration, documentId, session, conflictId, "2"],
    [binding, "invalid", session, conflictId, "2"], [binding, documentId, session, "invalid", "2"],
    [binding, documentId, "", conflictId, "2"], [binding, documentId, session, conflictId, ""],
    [binding, documentId, session, conflictId, "-1"], [binding, documentId, session, conflictId, "01"],
    [binding, documentId, session, conflictId, "1.1"], [binding, documentId, session, conflictId, "9223372036854775808"],
  ] as const) {
    mocked.invoke.mockClear();
    await expect(queue(args[0], args[1], args[2], args[3], args[4])).rejects.toThrow();
    expect(mocked.invoke).not.toHaveBeenCalledWith("workspace_queue_content_conflict_merge", expect.anything());
  }
});

test("요청 version과 다르거나 합본 operation ID가 없는 pending 응답을 거절한다", async () => {
  for (const value of [
    null, {}, { status:"unknown" }, { status:"completed", generation:"3" }, { status:"pending" },
    { status:"pending", version:"2" }, { status:"pending", version:"2", requested_operation_id:null },
    { status:"pending", version:"2", requested_operation_id:"" }, { status:"pending", version:"2", requested_operation_id:"invalid" },
    { status:"pending", version:"2", requested_operation_id:1 }, { status:"pending", version:"2", requested_operation_id:conflictId },
    { status:"pending", version:"1", requested_operation_id:mergeId }, { status:"pending", version:"3", requested_operation_id:mergeId },
    { status:"pending", version:"02", requested_operation_id:mergeId }, { status:"pending", version:"9223372036854775808", requested_operation_id:mergeId },
    { status:"pending", version:"2", requested_operation_id:mergeId, unexpected:true },
    { status:"pending", version:"2", requested_operation_id:mergeId, generation:"3" },
  ]) {
    respond(value);
    await expect(queue(binding, documentId, session, conflictId, "2")).rejects.toThrow();
  }
});
