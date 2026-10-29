import { beforeEach, expect, test, vi } from "vitest";
import * as ipc from "./ipc";
import type {
  WorkspaceContentConflictChooseRemoteResponse,
  WorkspaceContentConflictProgressResponse,
  WorkspaceContentConflictReopenResponse,
  WorkspaceRegistration,
} from "../../../packages/protocol-types";

const mocked = vi.hoisted(() => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: mocked.invoke }));

const binding: WorkspaceRegistration = { server_origin: "https://example.invalid", workspace_id: "workspace", workspace_epoch: "epoch", device_id: "device", device_stream_id: "stream" };
const documentId = "019a1000-0000-7000-8000-000000000002";
const operationId = "019a1000-0000-7000-8000-000000000003";
const otherId = "019a1000-0000-7000-8000-000000000004";
const session = "session-before";
const pending = { status: "pending", version: "2", generation: "3", choice: "choose_remote", needs_review: false, frozen: false } as const;
const completed = { status: "completed", generation: "4", frozen: false } as const;
const document = { binding, document_id: documentId, edit_session_id: "session-after", generation: "4", bytes_base64: "cmVtb3RlCg==", readonly: false, local_draft: true, has_session: true, cache_warning: null, open_notice: null };

type Progress = (binding: WorkspaceRegistration, documentId: string, sessionId: string, operationId: string, version: string) => Promise<WorkspaceContentConflictProgressResponse>;
type Complete = (binding: WorkspaceRegistration, documentId: string, sessionId: string, operationId: string, version: string) => Promise<WorkspaceContentConflictChooseRemoteResponse>;
type Reopen = (binding: WorkspaceRegistration, documentId: string, operationId: string, previousSessionId: string) => Promise<WorkspaceContentConflictReopenResponse>;
const typed = ipc as unknown as { continueWorkspaceRemoteContentConflict: Progress; completeWorkspacePendingRemoteContentConflict: Complete; reopenWorkspaceContentConflictCheckpoint: Reopen };
const proceed: Progress = (...args) => typed.continueWorkspaceRemoteContentConflict(...args);
const complete: Complete = (...args) => typed.completeWorkspacePendingRemoteContentConflict(...args);
const reopen: Reopen = (...args) => typed.reopenWorkspaceContentConflictCheckpoint(...args);

function respond(value: unknown) {
  mocked.invoke.mockImplementation(async (command: string) => command === "m0_renderer_session" ? "owner" : value);
}

beforeEach(() => mocked.invoke.mockReset());

test("후속 진행과 완료는 동일한 등록과 세션, 작업 및 선택 version을 native에 전달한다", async () => {
  const request = { renderer_session_id: "owner", binding, document_id: documentId, edit_session_id: session, operation_id: operationId, expected_intent_version: "2" };
  respond({ status: "pending", version: "2", requested_operation_id: otherId });
  await expect(proceed(binding, documentId, session, operationId, "2")).resolves.toEqual({ status: "pending", version: "2", requested_operation_id: otherId });
  expect(mocked.invoke).toHaveBeenCalledWith("workspace_continue_remote_content_conflict", { request });
  respond({ status: "pending", version: "2", requested_operation_id: null });
  await expect(proceed(binding, documentId, session, operationId, "2")).resolves.toEqual({ status: "pending", version: "2", requested_operation_id: null });
  respond({ status: "completed", generation: "4" });
  await expect(complete(binding, documentId, session, operationId, "2")).resolves.toEqual({ status: "completed", generation: "4" });
  expect(mocked.invoke).toHaveBeenCalledWith("workspace_complete_pending_remote_content_conflict", { request });
});

test("후속 진행과 완료는 잘못된 요청 식별자와 version을 호출 전에 거절한다", async () => {
  respond({ status: "pending", version: "2", requested_operation_id: null });
  for (const run of [proceed, complete]) {
    for (const args of [
      [binding, "invalid", session, operationId, "2"], [binding, documentId, session, "invalid", "2"],
      [binding, documentId, "", operationId, "2"], [binding, documentId, session, operationId, "01"],
      [binding, documentId, session, operationId, "-1"], [binding, documentId, session, operationId, "9223372036854775808"],
      [{ ...binding, workspace_epoch: null } as unknown as WorkspaceRegistration, documentId, session, operationId, "2"],
    ] as const) {
      mocked.invoke.mockClear();
      await expect(run(args[0], args[1], args[2], args[3], args[4])).rejects.toThrow();
      expect(mocked.invoke.mock.calls.some(([command]) => command === "workspace_continue_remote_content_conflict" || command === "workspace_complete_pending_remote_content_conflict")).toBe(false);
    }
  }
});

test("후속 진행은 요청한 선택 version의 pending만, 완료는 completed만 반환한다", async () => {
  for (const value of [
    null, {}, { status: "unknown" }, { status: "pending" }, { status: "pending", version: "01", requested_operation_id: null },
    { status: "pending", version: "3", requested_operation_id: otherId }, { status: "pending", version: "2" },
    { status: "pending", version: "2", requested_operation_id: "invalid" }, { status: "pending", version: "2", requested_operation_id: "" },
    { status: "pending", version: "2", requested_operation_id: operationId },
    { status: "pending", version: "2", requested_operation_id: 1 }, { status: "pending", version: "2", requested_operation_id: null, generation: "4" },
    { status: "completed", generation: "4" },
  ]) {
    respond(value);
    await expect(proceed(binding, documentId, session, operationId, "2")).rejects.toThrow();
  }
  for (const value of [
    null, {}, { status: "unknown" }, { status: "completed" }, { status: "completed", generation: "01" },
    { status: "completed", generation: "9223372036854775808" }, { status: "completed", generation: "4", version: "2" },
    { status: "pending", version: "2" },
  ]) {
    respond(value);
    await expect(complete(binding, documentId, session, operationId, "2")).rejects.toThrow();
  }
});

test("재열기는 이전 세션과 등록, 문서 및 작업을 전달하고 durable 문서와 상태를 보존한다", async () => {
  for (const state of [{ status: "not_found" }, pending, completed]) {
    respond({ state, document });
    await expect(reopen(binding, documentId, operationId, session)).resolves.toEqual({ state, document });
    expect(mocked.invoke).toHaveBeenCalledWith("workspace_reopen_content_conflict_checkpoint", { request: {
      renderer_session_id: "owner", binding, document_id: documentId, operation_id: operationId, previous_edit_session_id: session,
    } });
  }
});

test("재열기는 다른 등록, 문서 또는 이전 세션을 반환하면 거절한다", async () => {
  for (const badDocument of [
    ...Object.keys(binding).map((field) => ({ ...document, binding: { ...binding, [field]: "other" } })),
    { ...document, binding: null }, { ...document, document_id: otherId }, { ...document, document_id: "invalid" },
    { ...document, edit_session_id: session }, { ...document, edit_session_id: "" }, { ...document, edit_session_id: null },
  ]) {
    respond({ state: completed, document: badDocument });
    await expect(reopen(binding, documentId, operationId, session)).rejects.toThrow();
  }
});

test("재열기는 잘못된 요청과 누락되거나 조작된 문서 및 상태를 거절한다", async () => {
  respond({ state: completed, document });
  for (const args of [[binding, "invalid", operationId, session], [binding, documentId, "invalid", session], [binding, documentId, operationId, ""]] as const) {
    mocked.invoke.mockClear();
    await expect(reopen(args[0], args[1], args[2], args[3])).rejects.toThrow();
    expect(mocked.invoke).not.toHaveBeenCalledWith("workspace_reopen_content_conflict_checkpoint", expect.anything());
  }
  for (const response of [
    null, {}, { state: completed }, { document }, { state: null, document }, { state: { status: "unknown" }, document },
    { state: { ...pending, version: "01" }, document }, { state: { ...pending, needs_review: "false" }, document },
    { state: { ...completed, frozen: undefined }, document }, { state: { status: "not_found", version: "0" }, document },
    { state: completed, document: { ...document, generation: "01" } }, { state: completed, document: { ...document, local_draft: "true" } },
    { state: completed, document: { ...document, readonly: undefined } }, { state: completed, document, unexpected: true },
  ]) {
    respond(response);
    await expect(reopen(binding, documentId, operationId, session)).rejects.toThrow();
  }
});
