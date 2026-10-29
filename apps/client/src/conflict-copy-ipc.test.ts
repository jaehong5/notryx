import { beforeEach, expect, test, vi } from "vitest";
import * as ipc from "./ipc";
import type { ConflictReviewSnapshot } from "./conflict-review";
import type { WorkspaceContentConflictCopyResponse, WorkspaceRegistration } from "../../../packages/protocol-types";

const mocked = vi.hoisted(() => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: mocked.invoke }));

const binding: WorkspaceRegistration = { server_origin: "https://example.invalid", workspace_id: "workspace", workspace_epoch: "epoch", device_id: "device", device_stream_id: "stream" };
const documentId = "019a1000-0000-7000-8000-000000000002";
const operationId = "019a1000-0000-7000-8000-000000000003";
const parentId = "019a1000-0000-7000-8000-000000000004";
const copyId = "019a1000-0000-7000-8000-000000000005";
const createId = "019a1000-0000-7000-8000-000000000006";
const snapshot: ConflictReviewSnapshot = {
  binding, node_id: documentId, operation_id: operationId, operation_seq: "1", generation: "2",
  base_revision: "base-rev", base_bytes: new TextEncoder().encode("base\n"), base_hash: "sha256:f34848ca92665c342abd5816c9e3eda0e82180671195362bcd0080544a3bc2ac",
  local_bytes: new TextEncoder().encode("local\n"), local_hash: "sha256:efb83f2a277e9f49b38efd505f5cbb93885e721b6bd16b788937c9396174c006",
  conflict_revision: "revision-at-412", remote_revision: "current-rev", remote_bytes: new TextEncoder().encode("current\n"), remote_hash: "sha256:48aa6cae8c70abdb28631d22b316e6d9f9d0768ec2911de7090e248b2afe6ca1",
};
const result = { status: "pending", version: "0", node_id: copyId, create_operation_id: createId } as const;

function respond(value: unknown) {
  mocked.invoke.mockImplementation(async (command: string) => command === "m0_renderer_session" ? "owner" : value);
}

const copy = (shown: ConflictReviewSnapshot, session: string, version: string | null, parent: string, name: string): Promise<WorkspaceContentConflictCopyResponse> =>
  (ipc as unknown as { confirmWorkspaceContentConflictCopy(snapshot: ConflictReviewSnapshot, session: string, expectedVersion: string | null, parentId: string, name: string): Promise<WorkspaceContentConflictCopyResponse> })
    .confirmWorkspaceContentConflictCopy(shown, session, version, parent, name);

beforeEach(() => mocked.invoke.mockReset());

test("사본 확인은 표시한 원문 식별값과 대상 parent 및 이름을 그대로 전달한다", async () => {
  respond(result);
  await expect(copy(snapshot, "session", null, parentId, "내 초안.md")).resolves.toEqual(result);
  expect(mocked.invoke).toHaveBeenCalledWith("workspace_confirm_content_conflict_copy", { request: {
    selection: {
      renderer_session_id: "owner", binding, document_id: documentId, edit_session_id: "session", operation_id: operationId,
      generation: "2", expected_intent_version: null,
      base_revision: "base-rev", conflict_revision: "revision-at-412", remote_revision: "current-rev",
      base_hash: snapshot.base_hash, local_hash: snapshot.local_hash, remote_hash: snapshot.remote_hash,
    },
    parent_id: parentId, name: "내 초안.md",
  } });
});

test("명시적 재검토 version과 사본 생성 식별자를 보존한다", async () => {
  respond({ ...result, version: "5" });
  await expect(copy(snapshot, "session", "4", parentId, "Copy.MD")).resolves.toEqual({ ...result, version: "5" });
  expect(mocked.invoke).toHaveBeenCalledWith("workspace_confirm_content_conflict_copy", { request: expect.objectContaining({
    selection: expect.objectContaining({ expected_intent_version: "4" }), parent_id: parentId, name: "Copy.MD",
  }) });
});

test("잘못된 binding, 문서·작업·부모 ID, session, generation과 version을 native 호출 전에 거절한다", async () => {
  respond(result);
  const invalid: [ConflictReviewSnapshot, string, string | null, string, string][] = [
    [{ ...snapshot, binding: { ...binding, device_stream_id: null } as unknown as WorkspaceRegistration }, "session", null, parentId, "copy.md"],
    [{ ...snapshot, node_id: "invalid" }, "session", null, parentId, "copy.md"],
    [{ ...snapshot, operation_id: "invalid" }, "session", null, parentId, "copy.md"],
    [snapshot, "", null, parentId, "copy.md"], [snapshot, "session", null, "invalid", "copy.md"],
    [{ ...snapshot, generation: "01" }, "session", null, parentId, "copy.md"],
    [{ ...snapshot, generation: "9223372036854775808" }, "session", null, parentId, "copy.md"],
    [snapshot, "session", "01", parentId, "copy.md"], [snapshot, "session", "-1", parentId, "copy.md"],
    [snapshot, "session", "9223372036854775808", parentId, "copy.md"],
  ];
  for (const [shown, session, version, parent, name] of invalid) {
    mocked.invoke.mockClear();
    await expect(copy(shown, session, version, parent, name)).rejects.toThrow();
    expect(mocked.invoke).not.toHaveBeenCalledWith("workspace_confirm_content_conflict_copy", expect.anything());
  }
});

test("잘못된 사본 이름과 표시 revision·hash는 native 호출 전에 거절한다", async () => {
  respond(result);
  for (const name of ["", "plain.txt", "../escape.md", "bad\\name.md", "bad\nname.md", "a".repeat(253) + ".md"]) {
    mocked.invoke.mockClear();
    await expect(copy(snapshot, "session", null, parentId, name)).rejects.toThrow();
    expect(mocked.invoke).not.toHaveBeenCalledWith("workspace_confirm_content_conflict_copy", expect.anything());
  }
  for (const shown of [
    { ...snapshot, base_revision: "" }, { ...snapshot, conflict_revision: "bad\nrevision" }, { ...snapshot, remote_revision: "" },
    { ...snapshot, base_hash: "sha256:" + "a".repeat(63) }, { ...snapshot, local_hash: "sha256:" + "A".repeat(64) },
    { ...snapshot, remote_hash: "sha256:" + "g".repeat(64) },
  ]) {
    mocked.invoke.mockClear();
    await expect(copy(shown, "session", null, parentId, "copy.md")).rejects.toThrow();
    expect(mocked.invoke).not.toHaveBeenCalledWith("workspace_confirm_content_conflict_copy", expect.anything());
  }
});

test("사본 생성 응답의 상태, version과 두 식별자가 올바르지 않으면 거절한다", async () => {
  for (const value of [
    null, {}, { ...result, status: "completed" }, { ...result, version: "01" },
    { ...result, version: "9223372036854775808" }, { ...result, version: null },
    { ...result, node_id: "invalid" }, { ...result, create_operation_id: "invalid" },
    { ...result, node_id: documentId }, { ...result, node_id: createId },
    { ...result, create_operation_id: operationId }, { ...result, unexpected: true },
  ]) {
    respond(value);
    await expect(copy(snapshot, "session", null, parentId, "copy.md")).rejects.toThrow();
  }
});
