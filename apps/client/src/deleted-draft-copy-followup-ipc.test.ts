import { beforeEach, expect, test, vi } from "vitest";
import * as ipc from "./ipc";
import type { WorkspaceDeletedDraftCopyFollowupAbandonResult, WorkspaceDeletedDraftCopyStatus, WorkspaceRegistration } from "../../../packages/protocol-types";

const mocked = vi.hoisted(() => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: mocked.invoke }));

const binding: WorkspaceRegistration = { server_origin: "https://example.invalid", workspace_id: "workspace", workspace_epoch: "epoch", device_id: "device", device_stream_id: "stream" };
const documentId = "019a1000-0000-7000-8000-000000000002";
const parentId = "019a1000-0000-7000-8000-000000000003";
const nodeId = "019a1000-0000-7000-8000-000000000004";
const createId = "019a1000-0000-7000-8000-000000000005";
const blockerId = "019a1000-0000-7000-8000-000000000006";
const followupId = "019a1000-0000-7000-8000-000000000007";
const otherId = "019a1000-0000-7000-8000-000000000008";
const followupHash = "sha256:efb83f2a277e9f49b38efd505f5cbb93885e721b6bd16b788937c9396174c006";
const block = { blocker_operation_id: blockerId, followup_operation_id: followupId, followup_operation_seq: "5", followup_generation: "3", followup_hash: followupHash };
const state = { binding, document_id: documentId, generation: "2", local_hash: "sha256:efb83f2a277e9f49b38efd505f5cbb93885e721b6bd16b788937c9396174c006", tombstone_seq: "7", terminal_operation_id: null, parent_id: parentId, name: "초안 사본.md", node_id: nodeId, create_operation_id: createId, create_status: "not_queued", create_failure: null, content_status: "not_queued", content_operation_id: null, blocking_followup: block };

type Status = (binding: WorkspaceRegistration, documentId: string) => Promise<WorkspaceDeletedDraftCopyStatus | null>;
type Approve = (binding: WorkspaceRegistration, documentId: string, nodeId: string, createOperationId: string, blockerOperationId: string, followupOperationId: string, followupOperationSeq: string, followupGeneration: string, followupHash: string) => Promise<WorkspaceDeletedDraftCopyFollowupAbandonResult>;
const typed = ipc as unknown as { workspaceDeletedDraftCopyStatus: Status; requestWorkspaceDeletedDraftCopyFollowupAbandon: Approve };
const status: Status = (...args) => typed.workspaceDeletedDraftCopyStatus(...args);
const approve: Approve = (...args) => typed.requestWorkspaceDeletedDraftCopyFollowupAbandon(...args);

function respond(value: unknown) {
  mocked.invoke.mockImplementation(async (command: string) => command === "m0_renderer_session" ? "owner" : value);
}

beforeEach(() => mocked.invoke.mockReset());

test("상태 조회는 명시 승인이 필요한 blocker와 후속 작업의 순번을 보존한다", async () => {
  respond(state);
  await expect(status(binding, documentId)).resolves.toEqual(state);
  respond({ ...state, blocking_followup: null });
  await expect(status(binding, documentId)).resolves.toEqual({ ...state, blocking_followup: null });
});

test("상태 조회는 잘못된 차단 작업 identity·순번 및 누락된 정보를 거절한다", async () => {
  for (const blocking_followup of [
    undefined, {}, { ...block, blocker_operation_id: "invalid" }, { ...block, followup_operation_id: "invalid" },
    { ...block, blocker_operation_id: followupId }, { ...block, followup_operation_seq: "0" },
    { ...block, followup_operation_seq: "01" }, { ...block, followup_operation_seq: "9223372036854775808" },
    { ...block, followup_generation: "0" }, { ...block, followup_generation: "01" },
    { ...block, followup_generation: "9223372036854775808" }, { ...block, followup_generation: undefined },
    { ...block, followup_hash: "sha256:" + "A".repeat(64) }, { ...block, followup_hash: "invalid" }, { ...block, followup_hash: undefined },
    { ...block, extra: true },
  ]) {
    respond({ ...state, blocking_followup });
    await expect(status(binding, documentId)).rejects.toThrow();
  }
});

test("명시 승인은 표시한 등록·문서·사본과 blocker/followup ID를 전달한다", async () => {
  respond({ operation_id: followupId });
  await expect(approve(binding, documentId, nodeId, createId, blockerId, followupId, "5", "3", followupHash)).resolves.toEqual({ operation_id: followupId });
  expect(mocked.invoke).toHaveBeenCalledWith("workspace_request_deleted_draft_copy_followup_abandon", { request: {
    renderer_session_id: "owner", binding, document_id: documentId, node_id: nodeId, create_operation_id: createId,
    blocker_operation_id: blockerId, followup_operation_id: followupId, followup_operation_seq: "5", followup_generation: "3", followup_hash: followupHash,
  } });
});

test("잘못된 요청 identity와 승인한 작업이 아닌 결과는 거절한다", async () => {
  respond({ operation_id: followupId });
  const invalid: [WorkspaceRegistration, string, string, string, string, string, string, string, string][] = [
    [{ ...binding, device_stream_id: null } as unknown as WorkspaceRegistration, documentId, nodeId, createId, blockerId, followupId, "5", "3", followupHash],
    [binding, "invalid", nodeId, createId, blockerId, followupId, "5", "3", followupHash], [binding, documentId, "invalid", createId, blockerId, followupId, "5", "3", followupHash],
    [binding, documentId, documentId, createId, blockerId, followupId, "5", "3", followupHash], [binding, documentId, nodeId, "invalid", blockerId, followupId, "5", "3", followupHash],
    [binding, documentId, nodeId, createId, "invalid", followupId, "5", "3", followupHash], [binding, documentId, nodeId, createId, blockerId, "invalid", "5", "3", followupHash],
    [binding, documentId, nodeId, createId, blockerId, blockerId, "5", "3", followupHash],
    [binding, documentId, nodeId, createId, blockerId, followupId, "0", "3", followupHash],
    [binding, documentId, nodeId, createId, blockerId, followupId, undefined as unknown as string, "3", followupHash],
    [binding, documentId, nodeId, createId, blockerId, followupId, "01", "3", followupHash],
    [binding, documentId, nodeId, createId, blockerId, followupId, "9223372036854775808", "3", followupHash],
    [binding, documentId, nodeId, createId, blockerId, followupId, "5", "0", followupHash],
    [binding, documentId, nodeId, createId, blockerId, followupId, "5", "01", followupHash],
    [binding, documentId, nodeId, createId, blockerId, followupId, "5", "3", "sha256:" + "A".repeat(64)],
  ];
  for (const [scope, doc, node, create, blocker, followup, sequence, nextGeneration, hash] of invalid) {
    mocked.invoke.mockClear();
    await expect(approve(scope, doc, node, create, blocker, followup, sequence, nextGeneration, hash)).rejects.toThrow();
    expect(mocked.invoke).not.toHaveBeenCalledWith("workspace_request_deleted_draft_copy_followup_abandon", expect.anything());
  }
  for (const value of [null, {}, { operation_id: "invalid" }, { operation_id: otherId }, { operation_id: followupId, extra: true }]) {
    respond(value);
    await expect(approve(binding, documentId, nodeId, createId, blockerId, followupId, "5", "3", followupHash)).rejects.toThrow();
  }
});
