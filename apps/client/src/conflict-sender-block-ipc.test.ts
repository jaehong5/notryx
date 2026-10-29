import { beforeEach, expect, test, vi } from "vitest";
import { sendNextWorkspaceOperation } from "./ipc";
import type { WorkspaceRegistration } from "../../../packages/protocol-types";

const mocked = vi.hoisted(() => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: mocked.invoke }));

const binding: WorkspaceRegistration = { server_origin: "https://example.invalid", workspace_id: "workspace", workspace_epoch: "epoch", device_id: "device", device_stream_id: "stream" };
const documentId = "019a1000-0000-7000-8000-000000000002";
const conflictId = "019a1000-0000-7000-8000-000000000003";
const blockedId = "019a1000-0000-7000-8000-000000000004";
const block = { conflict_operation_id: conflictId, blocked_operation_id: blockedId, blocked_seq: "5" };
const blocked = { binding, status: "needs_review", operation_id: blockedId, document_id: documentId, generation: "2", content_block: block };

function respond(value: unknown) {
  mocked.invoke.mockImplementation(async (command: string) => command === "m0_renderer_session" ? "owner" : value);
}

beforeEach(() => mocked.invoke.mockReset());

test("전송은 충돌 때문에 중단된 작업과 원 412 작업을 함께 반환한다", async () => {
  respond(blocked);
  await expect(sendNextWorkspaceOperation()).resolves.toEqual(blocked);
  expect(mocked.invoke).toHaveBeenCalledWith("workspace_send_next", { request: { renderer_session_id: "owner" } });
});

test("일반 전송 및 다른 검토 사유는 content_block이 null로 유지된다", async () => {
  for (const value of [
    { binding, status: "idle", operation_id: null, document_id: null, generation: null, content_block: null },
    { binding, status: "needs_review", operation_id: blockedId, document_id: documentId, generation: "2", content_block: null },
    { binding, status: "committed", operation_id: blockedId, document_id: documentId, generation: "2", content_block: null },
  ]) {
    respond(value);
    await expect(sendNextWorkspaceOperation()).resolves.toEqual(value);
  }
});

test("충돌 차단 정보의 작업 ID, 순번 및 전송 상태가 맞지 않으면 거절한다", async () => {
  for (const value of [
    { ...blocked, content_block: undefined },
    { ...blocked, content_block: {} },
    { ...blocked, content_block: { ...block, conflict_operation_id: "invalid" } },
    { ...blocked, content_block: { ...block, conflict_operation_id: blockedId } },
    { ...blocked, content_block: { ...block, blocked_operation_id: "invalid" } },
    { ...blocked, content_block: { ...block, blocked_operation_id: conflictId } },
    { ...blocked, content_block: { ...block, blocked_seq: "0" } },
    { ...blocked, content_block: { ...block, blocked_seq: "01" } },
    { ...blocked, content_block: { ...block, blocked_seq: "9223372036854775808" } },
    { ...blocked, content_block: { ...block, extra: true } },
    { ...blocked, status: "committed" },
    { ...blocked, operation_id: null },
    { ...blocked, document_id: null },
    { ...blocked, generation: null },
    { ...blocked, generation: "0" },
  ]) {
    respond(value);
    await expect(sendNextWorkspaceOperation()).rejects.toThrow();
  }
});
