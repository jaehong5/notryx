import { beforeEach, expect, test, vi } from "vitest";
import * as ipc from "./ipc";
import type { WorkspaceContentConflictMergeFollowup, WorkspaceContentConflictMergeFollowupStatusResponse, WorkspaceContentConflictProgressResponse, WorkspaceRegistration } from "../../../packages/protocol-types";

const mocked = vi.hoisted(() => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: mocked.invoke }));

const binding: WorkspaceRegistration = { server_origin:"https://example.invalid", workspace_id:"workspace", workspace_epoch:"epoch", device_id:"device", device_stream_id:"stream" };
const documentId = "019a1000-0000-7000-8000-000000000002";
const conflictId = "019a1000-0000-7000-8000-000000000003";
const followupId = "019a1000-0000-7000-8000-000000000004";
const otherId = "019a1000-0000-7000-8000-000000000005";
const version = "2";
const session = "edit-session";
const hash = "sha256:efb83f2a277e9f49b38efd505f5cbb93885e721b6bd16b788937c9396174c006";
const followup: WorkspaceContentConflictMergeFollowup = { operation_id:followupId, operation_seq:"3", generation:"2", payload_hash:hash };
const initial: WorkspaceContentConflictMergeFollowupStatusResponse = { binding, document_id:documentId, operation_id:conflictId, intent_version:version, followup:null };
const blocked: WorkspaceContentConflictMergeFollowupStatusResponse = { ...initial, followup };

type Status = (binding:WorkspaceRegistration, documentId:string, operationId:string, version:string) => Promise<WorkspaceContentConflictMergeFollowupStatusResponse>;
type Approve = (binding:WorkspaceRegistration, documentId:string, sessionId:string, operationId:string, version:string, followup:WorkspaceContentConflictMergeFollowup) => Promise<WorkspaceContentConflictProgressResponse>;
const typed = ipc as unknown as { workspaceContentConflictMergeFollowupStatus:Status; requestWorkspaceContentConflictMergeFollowupAbandon:Approve };
const status:Status = (...args) => typed.workspaceContentConflictMergeFollowupStatus(...args);
const approve:Approve = (...args) => typed.requestWorkspaceContentConflictMergeFollowupAbandon(...args);

function respond(value:unknown) {
  mocked.invoke.mockImplementation(async (command:string) => command === "m0_renderer_session" ? "owner" : value);
}
beforeEach(() => mocked.invoke.mockReset());

test("합본 후속 상태 조회는 등록과 충돌 선택을 전달하고 명시 null 또는 작업 정보를 보존한다", async () => {
  respond(initial);
  await expect(status(binding, documentId, conflictId, version)).resolves.toEqual(initial);
  expect(mocked.invoke).toHaveBeenCalledWith("workspace_content_conflict_merge_followup_status", { request:{
    renderer_session_id:"owner", binding, document_id:documentId, operation_id:conflictId, expected_intent_version:version,
  } });
  respond(blocked);
  await expect(status(binding, documentId, conflictId, version)).resolves.toEqual(blocked);
  await expect(status(binding, documentId, conflictId, version)).resolves.toEqual(blocked);
  expect(mocked.invoke.mock.calls.filter(([command]) => command === "workspace_content_conflict_merge_followup_status")).toHaveLength(3);
  expect(mocked.invoke.mock.calls.every(([command]) => command === "m0_renderer_session" || command === "workspace_content_conflict_merge_followup_status")).toBe(true);
});

test("후속 상태 조회는 다른 등록·문서·충돌 ID·선택 version과 미지 필드를 거절한다", async () => {
  for (const invalid of [
    null, undefined, [], "invalid", {},
    ...Object.keys(binding).map((field) => ({ ...blocked, binding:{ ...binding, [field]:"other" } })),
    { ...blocked, binding:null }, { ...blocked, document_id:otherId }, { ...blocked, document_id:"invalid" },
    { ...blocked, operation_id:otherId }, { ...blocked, operation_id:"invalid" },
    { ...blocked, intent_version:"1" }, { ...blocked, intent_version:"3" }, { ...blocked, intent_version:"02" },
    { ...blocked, followup:undefined }, { ...blocked, unexpected:true },
  ]) {
    respond(invalid);
    await expect(status(binding, documentId, conflictId, version)).rejects.toThrow();
  }
  const missing:Partial<WorkspaceContentConflictMergeFollowupStatusResponse> = { ...blocked };
  delete missing.followup;
  respond(missing);
  await expect(status(binding, documentId, conflictId, version)).rejects.toThrow();
});

test("후속 상태의 ID·순번·generation·hash를 모두 검증한다", async () => {
  const invalidDecimal = [undefined, null, 1, "", "-1", "+1", "1.1", "01", "9223372036854775808"];
  for (const invalid of [
    {}, [], "invalid", { ...followup, operation_id:"invalid" }, { ...followup, operation_id:conflictId },
    { ...followup, operation_id:followupId.toUpperCase() }, { ...followup, operation_id:undefined },
    { ...followup, operation_seq:"0" }, { ...followup, generation:"0" },
    ...invalidDecimal.map((operation_seq) => ({ ...followup, operation_seq })),
    ...invalidDecimal.map((generation) => ({ ...followup, generation })),
    { ...followup, payload_hash:undefined }, { ...followup, payload_hash:null }, { ...followup, payload_hash:"invalid" },
    { ...followup, payload_hash:"sha256:" + "A".repeat(64) }, { ...followup, payload_hash:"sha256:" + "a".repeat(63) },
    { ...followup, payload_hash:"sha256:" + "g".repeat(64) }, { ...followup, unexpected:true },
  ]) {
    respond({ ...initial, followup:invalid });
    await expect(status(binding, documentId, conflictId, version)).rejects.toThrow();
  }
});

test("잘못된 상태 조회 요청은 native 호출 전에 거절한다", async () => {
  respond(blocked);
  for (const args of [
    [null as unknown as WorkspaceRegistration, documentId, conflictId, version],
    [{ ...binding, workspace_epoch:null } as unknown as WorkspaceRegistration, documentId, conflictId, version],
    [binding, "invalid", conflictId, version], [binding, documentId, "invalid", version],
    ...["", "-1", "+1", "1.1", "01", "9223372036854775808"].map((invalid) => [binding, documentId, conflictId, invalid] as const),
  ] as const) {
    mocked.invoke.mockClear();
    await expect(status(args[0], args[1], args[2], args[3])).rejects.toThrow();
    expect(mocked.invoke).not.toHaveBeenCalledWith("workspace_content_conflict_merge_followup_status", expect.anything());
  }
});

test("별도 승인은 확인한 후속 작업 ID·순번·generation·hash를 그대로 전달한다", async () => {
  const result = { status:"pending", version, requested_operation_id:followupId };
  respond(result);
  await expect(approve(binding, documentId, session, conflictId, version, followup)).resolves.toEqual(result);
  expect(mocked.invoke).toHaveBeenCalledWith("workspace_request_content_conflict_merge_followup_abandon", { request:{
    progress:{ renderer_session_id:"owner", binding, document_id:documentId, edit_session_id:session, operation_id:conflictId, expected_intent_version:version },
    followup,
  } });
  await expect(approve(binding, documentId, session, conflictId, version, followup)).resolves.toEqual(result);
  const requests = mocked.invoke.mock.calls.filter(([command]) => command === "workspace_request_content_conflict_merge_followup_abandon");
  expect(requests).toHaveLength(2);
  expect(requests[0][1]).toEqual(requests[1][1]);
});

test("잘못된 승인 요청은 native 호출 전에 거절한다", async () => {
  respond({ status:"pending", version, requested_operation_id:followupId });
  const invalidFollowups = [
    null, undefined, {}, [], { ...followup, operation_id:"invalid" }, { ...followup, operation_id:conflictId },
    { ...followup, operation_seq:"0" }, { ...followup, operation_seq:"01" }, { ...followup, operation_seq:"9223372036854775808" },
    { ...followup, generation:"0" }, { ...followup, generation:"01" }, { ...followup, generation:"9223372036854775808" },
    { ...followup, payload_hash:undefined }, { ...followup, payload_hash:"sha256:" + "A".repeat(64) }, { ...followup, unexpected:true },
  ];
  for (const invalid of invalidFollowups) {
    mocked.invoke.mockClear();
    await expect(approve(binding, documentId, session, conflictId, version, invalid as WorkspaceContentConflictMergeFollowup)).rejects.toThrow();
    expect(mocked.invoke).not.toHaveBeenCalledWith("workspace_request_content_conflict_merge_followup_abandon", expect.anything());
  }
  for (const args of [
    [null as unknown as WorkspaceRegistration, documentId, session, conflictId, version],
    [binding, "invalid", session, conflictId, version], [binding, documentId, "", conflictId, version],
    [binding, documentId, session, "invalid", version], [binding, documentId, session, conflictId, "01"],
  ] as const) {
    mocked.invoke.mockClear();
    await expect(approve(args[0], args[1], args[2], args[3], args[4], followup)).rejects.toThrow();
    expect(mocked.invoke).not.toHaveBeenCalledWith("workspace_request_content_conflict_merge_followup_abandon", expect.anything());
  }
});

test("승인 결과는 확인한 작업과 version의 pending 응답만 허용한다", async () => {
  for (const invalid of [
    null, undefined, [], {}, { status:"unknown" }, { status:"completed", generation:"3" },
    { status:"pending", version }, { status:"pending", version, requested_operation_id:null },
    { status:"pending", version, requested_operation_id:conflictId }, { status:"pending", version, requested_operation_id:otherId },
    { status:"pending", version:"3", requested_operation_id:followupId }, { status:"pending", version:"02", requested_operation_id:followupId },
    { status:"pending", version, requested_operation_id:followupId, unexpected:true },
  ]) {
    respond(invalid);
    await expect(approve(binding, documentId, session, conflictId, version, followup)).rejects.toThrow();
  }
});
