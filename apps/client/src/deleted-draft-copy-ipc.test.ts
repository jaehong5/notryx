import { beforeEach, expect, test, vi } from "vitest";
import * as ipc from "./ipc";
import type { WorkspaceDeletedDraftCopyConfirmResult, WorkspaceDeletedDraftCopySnapshot, WorkspaceRegistration } from "../../../packages/protocol-types";

const mocked = vi.hoisted(() => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: mocked.invoke }));

const binding: WorkspaceRegistration = { server_origin: "https://example.invalid", workspace_id: "workspace", workspace_epoch: "epoch", device_id: "device", device_stream_id: "stream" };
const documentId = "019a1000-0000-7000-8000-000000000002";
const parentId = "019a1000-0000-7000-8000-000000000003";
const copyId = "019a1000-0000-7000-8000-000000000004";
const createId = "019a1000-0000-7000-8000-000000000005";
const otherId = "019a1000-0000-7000-8000-000000000006";
const localHash = "sha256:efb83f2a277e9f49b38efd505f5cbb93885e721b6bd16b788937c9396174c006";
const snapshot = { binding, document_id: documentId, edit_session_id: "session", generation: "2", local_hash: localHash, tombstone_seq: "7" };
const result = { status: "pending", node_id: copyId, create_operation_id: createId };

type Snapshot = WorkspaceDeletedDraftCopySnapshot;
type Prepare = (binding: WorkspaceRegistration, documentId: string, sessionId: string, generation: string) => Promise<Snapshot>;
type Confirm = (snapshot: Snapshot, parentId: string, name: string) => Promise<WorkspaceDeletedDraftCopyConfirmResult>;
const typed = ipc as unknown as { prepareWorkspaceDeletedDraftCopy: Prepare; confirmWorkspaceDeletedDraftCopy: Confirm };
const prepare: Prepare = (...args) => typed.prepareWorkspaceDeletedDraftCopy(...args);
const confirm: Confirm = (...args) => typed.confirmWorkspaceDeletedDraftCopy(...args);

function respond(value: unknown) {
  mocked.invoke.mockImplementation(async (command: string) => command === "m0_renderer_session" ? "owner" : value);
}

beforeEach(() => mocked.invoke.mockReset());

test("삭제 사본 snapshot은 현재 등록·문서·session·durable generation의 삭제 증거를 전달한다", async () => {
  respond(snapshot);
  await expect(prepare(binding, documentId, "session", "2")).resolves.toEqual(snapshot);
  expect(mocked.invoke).toHaveBeenCalledWith("workspace_prepare_deleted_draft_copy", { request: {
    renderer_session_id: "owner", binding, document_id: documentId, edit_session_id: "session", generation: "2",
  } });
});

test("삭제 증거의 순번은 양수 u64 범위를 보존한다", async () => {
  const high = { ...snapshot, tombstone_seq: "18446744073709551615" };
  respond(high);
  await expect(prepare(binding, documentId, "session", "2")).resolves.toEqual(high);
  respond(result);
  await expect(confirm(high, parentId, "초안 사본.md")).resolves.toEqual(result);
  expect(mocked.invoke).toHaveBeenCalledWith("workspace_confirm_deleted_draft_copy", { request: expect.objectContaining({ tombstone_seq: high.tombstone_seq }) });
});

test("snapshot 조회는 다른 등록·문서·session·generation 및 잘못된 삭제 증거를 거절한다", async () => {
  for (const value of [
    null, {}, ...Object.keys(binding).map((field) => ({ ...snapshot, binding: { ...binding, [field]: "other" } })),
    { ...snapshot, binding: null }, { ...snapshot, document_id: otherId }, { ...snapshot, edit_session_id: "other-session" },
    { ...snapshot, generation: "3" }, { ...snapshot, generation: "01" }, { ...snapshot, local_hash: "sha256:" + "A".repeat(64) },
    { ...snapshot, tombstone_seq: "0" }, { ...snapshot, tombstone_seq: "01" }, { ...snapshot, tombstone_seq: "18446744073709551616" },
    { ...snapshot, tombstone_seq: null }, { ...snapshot, extra: true },
  ]) {
    respond(value);
    await expect(prepare(binding, documentId, "session", "2")).rejects.toThrow();
  }
});

test("삭제 사본 확인은 사용자가 확인한 durable hash·삭제 순번과 선택한 대상만 전달한다", async () => {
  respond(result);
  await expect(confirm(snapshot, parentId, "초안 사본.md")).resolves.toEqual(result);
  expect(mocked.invoke).toHaveBeenCalledWith("workspace_confirm_deleted_draft_copy", { request: {
    renderer_session_id: "owner", binding, document_id: documentId, edit_session_id: "session", generation: "2", local_hash: localHash,
    tombstone_seq: "7", parent_id: parentId, name: "초안 사본.md",
  } });
});

test("잘못된 조회 요청 및 사본 확인 identity, 이름은 native 호출 전에 거절한다", async () => {
  respond(result);
  const invalidPrepare: [WorkspaceRegistration, string, string, string][] = [
    [binding, "invalid", "session", "2"], [binding, documentId, "", "2"], [binding, documentId, "session", "0"],
    [binding, documentId, "session", "01"], [binding, documentId, "session", "9223372036854775808"],
    [{ ...binding, device_stream_id: null } as unknown as WorkspaceRegistration, documentId, "session", "2"],
  ];
  for (const [scope, doc, session, generation] of invalidPrepare) {
    mocked.invoke.mockClear();
    await expect(prepare(scope, doc, session, generation)).rejects.toThrow();
    expect(mocked.invoke).not.toHaveBeenCalledWith("workspace_prepare_deleted_draft_copy", expect.anything());
  }
  const invalidConfirm: [Snapshot, string, string][] = [
    [{ ...snapshot, document_id: "invalid" }, parentId, "copy.md"], [{ ...snapshot, edit_session_id: "" }, parentId, "copy.md"],
    [{ ...snapshot, generation: "0" }, parentId, "copy.md"], [{ ...snapshot, local_hash: "invalid" }, parentId, "copy.md"],
    [{ ...snapshot, tombstone_seq: "0" }, parentId, "copy.md"], [{ ...snapshot, tombstone_seq: "18446744073709551616" }, parentId, "copy.md"],
    [snapshot, "invalid", "copy.md"], [snapshot, parentId, "../copy.md"], [snapshot, parentId, "copy.txt"],
    [snapshot, parentId, "a".repeat(253) + ".md"],
  ];
  for (const [shown, parent, name] of invalidConfirm) {
    mocked.invoke.mockClear();
    await expect(confirm(shown, parent, name)).rejects.toThrow();
    expect(mocked.invoke).not.toHaveBeenCalledWith("workspace_confirm_deleted_draft_copy", expect.anything());
  }
});

test("확인 결과는 보류 상태와 새 사본 node/create 작업 identity만 허용한다", async () => {
  for (const value of [
    null, {}, { ...result, status: "saved" }, { ...result, status: "unknown" }, { ...result, node_id: "invalid" },
    { ...result, create_operation_id: "invalid" }, { ...result, node_id: documentId },
    { ...result, node_id: createId }, { ...result, extra: true },
  ]) {
    respond(value);
    await expect(confirm(snapshot, parentId, "copy.md")).rejects.toThrow();
  }
});
