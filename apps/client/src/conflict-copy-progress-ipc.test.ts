import { beforeEach, expect, test, vi } from "vitest";
import * as ipc from "./ipc";
import type { WorkspaceContentConflictCopyProgressResponse, WorkspaceContentConflictCopyStatusResponse, WorkspaceRegistration } from "../../../packages/protocol-types";

const mocked = vi.hoisted(() => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: mocked.invoke }));

const binding: WorkspaceRegistration = { server_origin: "https://example.invalid", workspace_id: "workspace", workspace_epoch: "epoch", device_id: "device", device_stream_id: "stream" };
const documentId = "019a1000-0000-7000-8000-000000000002";
const conflictId = "019a1000-0000-7000-8000-000000000003";
const parentId = "019a1000-0000-7000-8000-000000000004";
const copyId = "019a1000-0000-7000-8000-000000000005";
const createId = "019a1000-0000-7000-8000-000000000006";
const followupId = "019a1000-0000-7000-8000-000000000007";
const otherId = "019a1000-0000-7000-8000-000000000008";
const copy = { version: "2", parent_id: parentId, name: "내 초안.md", node_id: copyId, create_operation_id: createId,
  create_status: "not_queued", create_failure: null, content_status: "not_queued", followup_operation_id: followupId };
const statusResult = { binding, document_id: documentId, operation_id: conflictId, copy };
type Status = (binding: WorkspaceRegistration, documentId: string, operationId: string) => Promise<WorkspaceContentConflictCopyStatusResponse>;
type Progress = (binding: WorkspaceRegistration, documentId: string, sessionId: string, operationId: string, version: string, approvedFollowupOperationId: string | null) => Promise<WorkspaceContentConflictCopyProgressResponse>;
const typed = ipc as unknown as { workspaceContentConflictCopyStatus: Status; progressWorkspaceContentConflictCopy: Progress };
const status: Status = (...args) => typed.workspaceContentConflictCopyStatus(...args);
const progress: Progress = (...args) => typed.progressWorkspaceContentConflictCopy(...args);

function respond(value: unknown) {
  mocked.invoke.mockImplementation(async (command: string) => command === "m0_renderer_session" ? "owner" : value);
}

beforeEach(() => mocked.invoke.mockReset());

test("사본 상태는 전체 등록과 문서·원 412 작업을 전달하고 승인 대상을 보존한다", async () => {
  respond(statusResult);
  await expect(status(binding, documentId, conflictId)).resolves.toEqual(statusResult);
  expect(mocked.invoke).toHaveBeenCalledWith("workspace_content_conflict_copy_status", { request: {
    renderer_session_id: "owner", binding, document_id: documentId, operation_id: conflictId,
  } });
  respond({ ...statusResult, copy: null });
  await expect(status(binding, documentId, conflictId)).resolves.toEqual({ ...statusResult, copy: null });
});

test("상태 조회는 잘못된 등록, 문서 또는 원 작업을 native 호출 전에 거절한다", async () => {
  respond(statusResult);
  for (const [scope, doc, operation] of [
    [binding, "invalid", conflictId], [binding, documentId, "invalid"],
    [{ ...binding, device_stream_id: null } as unknown as WorkspaceRegistration, documentId, conflictId],
  ] as const) {
    mocked.invoke.mockClear();
    await expect(status(scope, doc, operation)).rejects.toThrow();
    expect(mocked.invoke).not.toHaveBeenCalledWith("workspace_content_conflict_copy_status", expect.anything());
  }
});

test("상태 조회는 생성과 본문 저장 진행을 구분하고 미승인 대상이 없으면 null을 유지한다", async () => {
  for (const [create_status, create_failure, content_status] of [
    ["not_queued", null, "not_queued"], ["pending", null, "not_queued"], ["created", null, "not_queued"],
    ["created", null, "pending"], ["created", null, "saved"], ["needs_review", "destination_exists", "not_queued"],
    ["needs_review", "other", "not_queued"], ["created", null, "needs_review"],
  ]) {
    const expected = { ...statusResult, copy: { ...copy, create_status, create_failure, content_status, followup_operation_id: null } };
    respond(expected);
    await expect(status(binding, documentId, conflictId)).resolves.toEqual(expected);
  }
});

test("상태 응답의 다른 등록·문서·원 작업 또는 잘못된 사본 식별자는 거절한다", async () => {
  for (const value of [
    ...Object.keys(binding).map((field) => ({ ...statusResult, binding: { ...binding, [field]: "other" } })),
    { ...statusResult, binding: null }, { ...statusResult, document_id: otherId }, { ...statusResult, operation_id: otherId },
    { ...statusResult, copy: { ...copy, parent_id: "invalid" } }, { ...statusResult, copy: { ...copy, node_id: documentId } },
    { ...statusResult, copy: { ...copy, node_id: createId } }, { ...statusResult, copy: { ...copy, create_operation_id: conflictId } },
    { ...statusResult, copy: { ...copy, followup_operation_id: conflictId } }, { ...statusResult, copy: { ...copy, followup_operation_id: "invalid" } },
  ]) {
    respond(value);
    await expect(status(binding, documentId, conflictId)).rejects.toThrow();
  }
});

test("상태 조회는 누락된 상태, 잘못된 이름·version·enum 및 예상 밖 필드를 거절한다", async () => {
  for (const value of [
    null, {}, { ...statusResult, copy: undefined }, { ...statusResult, extra: true },
    { ...statusResult, copy: { ...copy, version: "01" } }, { ...statusResult, copy: { ...copy, version: "9223372036854775808" } },
    { ...statusResult, copy: { ...copy, name: "../bad.md" } }, { ...statusResult, copy: { ...copy, name: "note.txt" } },
    { ...statusResult, copy: { ...copy, create_status: "unknown" } }, { ...statusResult, copy: { ...copy, content_status: "unknown" } },
    { ...statusResult, copy: { ...copy, create_status: "needs_review", create_failure: null } },
    { ...statusResult, copy: { ...copy, create_status: "needs_review", create_failure: "unknown" } },
    { ...statusResult, copy: { ...copy, create_status: "pending", create_failure: "destination_exists" } },
    { ...statusResult, copy: { ...copy, create_status: "created", create_failure: "other" } },
    { ...statusResult, copy: { ...copy, create_failure: undefined } },
    { ...statusResult, copy: { ...copy, followup_operation_id: undefined } }, { ...statusResult, copy: { ...copy, extra: true } },
  ]) {
    respond(value);
    await expect(status(binding, documentId, conflictId)).rejects.toThrow();
  }
});

test("진행 요청은 승인된 후속 작업 ID 또는 승인 없음 null을 정확히 전달한다", async () => {
  for (const approved of [null, followupId]) {
    respond({ status: "pending", requested_operation_id: approved });
    await expect(progress(binding, documentId, "session", conflictId, "2", approved)).resolves.toEqual({ status: "pending", requested_operation_id: approved });
    expect(mocked.invoke).toHaveBeenCalledWith("workspace_progress_content_conflict_copy", { request: {
      progress: { renderer_session_id: "owner", binding, document_id: documentId, edit_session_id: "session", operation_id: conflictId, expected_intent_version: "2" },
      approved_followup_operation_id: approved,
    } });
  }
});

test("진행 응답은 승인 필요, 요청 대기, 저장 완료와 검토 필요를 구분한다", async () => {
  for (const value of [
    { status: "approval_required", followup_operation_id: followupId }, { status: "pending", requested_operation_id: null },
    { status: "saved" }, { status: "needs_review" },
  ]) {
    respond(value);
    await expect(progress(binding, documentId, "session", conflictId, "2", null)).resolves.toEqual(value);
  }
  respond({ status: "pending", requested_operation_id: followupId });
  await expect(progress(binding, documentId, "session", conflictId, "2", followupId)).resolves.toEqual({ status: "pending", requested_operation_id: followupId });
});

test("잘못된 진행 요청과 응답의 승인·후속 작업 식별자 및 상태를 거절한다", async () => {
  respond({ status: "pending", requested_operation_id: null });
  const invalidRequests: [WorkspaceRegistration, string, string, string, string, string | null][] = [
    [binding, "invalid", "session", conflictId, "2", null], [binding, documentId, "", conflictId, "2", null],
    [binding, documentId, "session", "invalid", "2", null], [binding, documentId, "session", conflictId, "01", null],
    [binding, documentId, "session", conflictId, "9223372036854775808", null],
    [binding, documentId, "session", conflictId, "2", "invalid"], [binding, documentId, "session", conflictId, "2", conflictId],
  ];
  for (const [scope, doc, session, operation, version, approved] of invalidRequests) {
    mocked.invoke.mockClear();
    await expect(progress(scope, doc, session, operation, version, approved)).rejects.toThrow();
    expect(mocked.invoke).not.toHaveBeenCalledWith("workspace_progress_content_conflict_copy", expect.anything());
  }
  for (const value of [
    null, {}, { status: "unknown" }, { status: "approval_required" }, { status: "approval_required", followup_operation_id: "invalid" },
    { status: "approval_required", followup_operation_id: conflictId }, { status: "pending" },
    { status: "pending", requested_operation_id: "invalid" }, { status: "pending", requested_operation_id: conflictId },
    { status: "pending", requested_operation_id: followupId },
    { status: "saved", requested_operation_id: followupId }, { status: "needs_review", extra: true },
  ]) {
    respond(value);
    await expect(progress(binding, documentId, "session", conflictId, "2", null)).rejects.toThrow();
  }
  respond({ status: "pending", requested_operation_id: otherId });
  await expect(progress(binding, documentId, "session", conflictId, "2", followupId)).rejects.toThrow();
});
