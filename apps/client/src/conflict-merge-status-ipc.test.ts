import { beforeEach, expect, test, vi } from "vitest";
import * as ipc from "./ipc";
import type { WorkspaceContentConflictMergeStatusResponse, WorkspaceRegistration } from "../../../packages/protocol-types";

const mocked = vi.hoisted(() => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: mocked.invoke }));

const binding: WorkspaceRegistration = { server_origin:"https://example.invalid", workspace_id:"workspace", workspace_epoch:"epoch", device_id:"device", device_stream_id:"stream" };
const documentId = "019a1000-0000-7000-8000-000000000002";
const conflictId = "019a1000-0000-7000-8000-000000000003";
const mergeId = "019a1000-0000-7000-8000-000000000004";
const otherId = "019a1000-0000-7000-8000-000000000005";
const version = "2";
const initial = { binding, document_id:documentId, operation_id:conflictId, intent_version:version, merge_operation:null };
const queued = { ...initial, merge_operation:{ operation_id:mergeId, operation_seq:"3" } };

type Status = (binding:WorkspaceRegistration, documentId:string, operationId:string, version:string) => Promise<WorkspaceContentConflictMergeStatusResponse>;
const status:Status = (...args) => (ipc as unknown as { workspaceContentConflictMergeStatus:Status }).workspaceContentConflictMergeStatus(...args);

function respond(value:unknown) {
  mocked.invoke.mockImplementation(async (command:string) => command === "m0_renderer_session" ? "owner" : value);
}
beforeEach(() => mocked.invoke.mockReset());

test("합본 상태 조회는 renderer와 등록·문서·원래 충돌 작업·확인 version을 전달한다", async () => {
  respond(initial);
  await expect(status(binding, documentId, conflictId, version)).resolves.toEqual(initial);
  expect(mocked.invoke).toHaveBeenCalledWith("workspace_content_conflict_merge_status", { request:{
    renderer_session_id:"owner", binding, document_id:documentId, operation_id:conflictId, expected_intent_version:version,
  } });
});

test("합본 operation 미생성과 생성된 작업의 동일 ID·순번을 구분한다", async () => {
  respond(initial);
  expect((await status(binding, documentId, conflictId, version)).merge_operation).toBeNull();
  respond(queued);
  const first = await status(binding, documentId, conflictId, version);
  const retry = await status(binding, documentId, conflictId, version);
  expect(first).toEqual(queued);
  expect(retry).toEqual(first);
  const calls = mocked.invoke.mock.calls.filter(([command]) => command === "workspace_content_conflict_merge_status");
  expect(calls).toHaveLength(3);
  expect(calls[1][1]).toEqual(calls[2][1]);
});

test("응답의 등록·문서·원래 충돌 작업 또는 확인 version이 요청과 다르면 거절한다", async () => {
  for (const wrong of [
    ...Object.keys(binding).map((field) => ({ ...queued, binding:{ ...binding, [field]:"other" } })),
    { ...queued, binding:null }, { ...queued, binding:undefined },
    { ...queued, document_id:otherId }, { ...queued, document_id:"invalid" }, { ...queued, document_id:undefined },
    { ...queued, operation_id:otherId }, { ...queued, operation_id:"invalid" }, { ...queued, operation_id:undefined },
    { ...queued, intent_version:"0" }, { ...queued, intent_version:"3" }, { ...queued, intent_version:undefined },
  ]) {
    respond(wrong);
    await expect(status(binding, documentId, conflictId, version)).rejects.toThrow();
  }
});

test("합본 operation ID와 순번, 필수 항목 및 알 수 없는 응답 필드를 검증한다", async () => {
  const invalidDecimal = [undefined, null, 1, "", "-1", "+1", "1.1", "01", "9223372036854775808"];
  for (const value of [
    null, undefined, [], "invalid", {},
    { ...queued, unexpected:true }, { ...queued, merge_operation:undefined },
    { ...initial, merge_operation:undefined, queued_operation:null },
    { ...queued, merge_operation:[] }, { ...queued, merge_operation:{} },
    { ...queued, merge_operation:{ operation_id:conflictId, operation_seq:"3" } },
    { ...queued, merge_operation:{ operation_id:"invalid", operation_seq:"3" } },
    { ...queued, merge_operation:{ operation_id:undefined, operation_seq:"3" } },
    { ...queued, merge_operation:{ operation_id:mergeId, operation_seq:"0" } },
    { ...queued, merge_operation:{ operation_id:mergeId, operation_seq:"3", extra:true } },
    ...invalidDecimal.map((operation_seq) => ({ ...queued, merge_operation:{ operation_id:mergeId, operation_seq } })),
    ...invalidDecimal.map((intent_version) => ({ ...queued, intent_version })),
  ]) {
    respond(value);
    await expect(status(binding, documentId, conflictId, version)).rejects.toThrow();
  }
});

test("잘못된 등록·문서·충돌 작업과 version은 native 조회 전에 거절한다", async () => {
  respond(queued);
  for (const args of [
    [null as unknown as WorkspaceRegistration, documentId, conflictId, version],
    [{ ...binding, workspace_epoch:null } as unknown as WorkspaceRegistration, documentId, conflictId, version],
    [binding, "invalid", conflictId, version], [binding, "", conflictId, version],
    [binding, documentId, "invalid", version], [binding, documentId, "", version],
    ...["", "-1", "+1", "1.1", "01", "9223372036854775808"].map((bad) => [binding, documentId, conflictId, bad] as const),
  ] as const) {
    mocked.invoke.mockClear();
    await expect(status(args[0], args[1], args[2], args[3])).rejects.toThrow();
    expect(mocked.invoke).not.toHaveBeenCalledWith("workspace_content_conflict_merge_status", expect.anything());
  }
});
