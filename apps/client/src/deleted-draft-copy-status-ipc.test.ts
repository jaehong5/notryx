import { beforeEach, expect, test, vi } from "vitest";
import * as ipc from "./ipc";
import type { WorkspaceDeletedDraftCopyProgressResult, WorkspaceDeletedDraftCopyStatus, WorkspaceRegistration } from "../../../packages/protocol-types";

const mocked = vi.hoisted(() => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: mocked.invoke }));

const binding: WorkspaceRegistration = { server_origin: "https://example.invalid", workspace_id: "workspace", workspace_epoch: "epoch", device_id: "device", device_stream_id: "stream" };
const documentId = "019a1000-0000-7000-8000-000000000002";
const parentId = "019a1000-0000-7000-8000-000000000003";
const nodeId = "019a1000-0000-7000-8000-000000000004";
const createId = "019a1000-0000-7000-8000-000000000005";
const contentId = "019a1000-0000-7000-8000-000000000006";
const terminalId = "019a1000-0000-7000-8000-000000000007";
const otherId = "019a1000-0000-7000-8000-000000000008";
const statusResult = {
  binding, document_id: documentId, generation: "2", local_hash: "sha256:efb83f2a277e9f49b38efd505f5cbb93885e721b6bd16b788937c9396174c006",
  tombstone_seq: "7", terminal_operation_id: null, parent_id: parentId, name: "내 초안.md", node_id: nodeId,
  create_operation_id: createId, create_status: "not_queued", create_failure: null, content_status: "not_queued", content_operation_id: null, blocking_followup: null,
};

type Status = (binding: WorkspaceRegistration, documentId: string) => Promise<WorkspaceDeletedDraftCopyStatus | null>;
type Progress = (binding: WorkspaceRegistration, documentId: string, nodeId: string, createOperationId: string) => Promise<WorkspaceDeletedDraftCopyProgressResult>;
const typed = ipc as unknown as { workspaceDeletedDraftCopyStatus: Status; progressWorkspaceDeletedDraftCopy: Progress };
const status: Status = (...args) => typed.workspaceDeletedDraftCopyStatus(...args);
const progress: Progress = (...args) => typed.progressWorkspaceDeletedDraftCopy(...args);

function respond(value: unknown) {
  mocked.invoke.mockImplementation(async (command: string) => command === "m0_renderer_session" ? "owner" : value);
}

beforeEach(() => mocked.invoke.mockReset());

test("삭제 사본 상태는 등록과 원본 문서를 지정하며 이전 선택이 없을 때 null을 반환한다", async () => {
  respond(statusResult);
  await expect(status(binding, documentId)).resolves.toEqual(statusResult);
  expect(mocked.invoke).toHaveBeenCalledWith("workspace_deleted_draft_copy_status", { request: { renderer_session_id: "owner", binding, document_id: documentId } });
  respond(null);
  await expect(status(binding, documentId)).resolves.toBeNull();
});

test("삭제 증거 두 종류와 생성·본문 저장 상태를 구분한다", async () => {
  for (const evidence of [
    { tombstone_seq: "18446744073709551615", terminal_operation_id: null },
    { tombstone_seq: null, terminal_operation_id: terminalId },
  ]) {
    for (const [create_status, create_failure, content_status, content_operation_id] of [
      ["not_queued", null, "not_queued", null], ["pending", null, "not_queued", null], ["created", null, "not_queued", null],
      ["created", null, "pending", contentId], ["created", null, "saved", contentId], ["created", null, "needs_review", contentId],
      ["needs_review", "destination_exists", "not_queued", null], ["needs_review", "other", "not_queued", null],
    ]) {
      const expected = { ...statusResult, ...evidence, create_status, create_failure, content_status, content_operation_id };
      respond(expected);
      await expect(status(binding, documentId)).resolves.toEqual(expected);
    }
  }
});

test("상태 조회는 다른 등록·문서와 조작된 사본 식별자 또는 확인 자료를 거절한다", async () => {
  for (const value of [
    ...Object.keys(binding).map((field) => ({ ...statusResult, binding: { ...binding, [field]: "other" } })),
    { ...statusResult, binding: null }, { ...statusResult, document_id: otherId },
    { ...statusResult, generation: "0" }, { ...statusResult, generation: "01" }, { ...statusResult, local_hash: "invalid" },
    { ...statusResult, parent_id: "invalid" }, { ...statusResult, name: "../copy.md" }, { ...statusResult, name: "copy.txt" },
    { ...statusResult, node_id: documentId }, { ...statusResult, node_id: parentId }, { ...statusResult, create_operation_id: "invalid" },
    { ...statusResult, tombstone_seq: null, terminal_operation_id: null }, { ...statusResult, terminal_operation_id: terminalId },
    { ...statusResult, tombstone_seq: "0" }, { ...statusResult, tombstone_seq: "18446744073709551616" },
    { ...statusResult, tombstone_seq: null, terminal_operation_id: "invalid" }, { ...statusResult, extra: true },
  ]) {
    respond(value);
    await expect(status(binding, documentId)).rejects.toThrow();
  }
});

test("생성 실패와 본문 작업 ID의 상태별 누락 또는 모순은 거절한다", async () => {
  for (const value of [
    { ...statusResult, create_status: "unknown" }, { ...statusResult, content_status: "unknown" },
    { ...statusResult, create_status: "needs_review", create_failure: null }, { ...statusResult, create_failure: "other" },
    { ...statusResult, create_status: "needs_review", create_failure: "unknown" }, { ...statusResult, create_failure: undefined },
    { ...statusResult, content_status: "saved", content_operation_id: null },
    { ...statusResult, content_status: "pending", content_operation_id: "invalid" },
    { ...statusResult, content_status: "not_queued", content_operation_id: contentId },
  ]) {
    respond(value);
    await expect(status(binding, documentId)).rejects.toThrow();
  }
});

test("진행 요청은 확인된 등록·원본·사본 및 create 작업을 그대로 전달한다", async () => {
  for (const operation_id of [createId, contentId]) {
    respond({ operation_id });
    await expect(progress(binding, documentId, nodeId, createId)).resolves.toEqual({ operation_id });
    expect(mocked.invoke).toHaveBeenCalledWith("workspace_progress_deleted_draft_copy", { request: {
      renderer_session_id: "owner", binding, document_id: documentId, node_id: nodeId, create_operation_id: createId,
    } });
  }
});

test("status/progress는 잘못된 요청과 진행 작업 응답을 거절한다", async () => {
  respond({ operation_id: createId });
  for (const [scope, doc] of [[binding, "invalid"], [{ ...binding, device_id: null } as unknown as WorkspaceRegistration, documentId]] as const) {
    mocked.invoke.mockClear();
    await expect(status(scope, doc)).rejects.toThrow();
    expect(mocked.invoke).not.toHaveBeenCalledWith("workspace_deleted_draft_copy_status", expect.anything());
  }
  for (const [doc, node, create] of [["invalid", nodeId, createId], [documentId, "invalid", createId], [documentId, documentId, createId], [documentId, nodeId, "invalid"]] as const) {
    mocked.invoke.mockClear();
    await expect(progress(binding, doc, node, create)).rejects.toThrow();
    expect(mocked.invoke).not.toHaveBeenCalledWith("workspace_progress_deleted_draft_copy", expect.anything());
  }
  for (const value of [null, {}, { operation_id: "invalid" }, { operation_id: createId, extra: true }]) {
    respond(value);
    await expect(progress(binding, documentId, nodeId, createId)).rejects.toThrow();
  }
});
