import { beforeEach, expect, test, vi } from "vitest";
import * as ipc from "./ipc";
import type { ConflictReviewSnapshot } from "./conflict-review";
import type { WorkspaceContentConflictChooseRemoteResponse, WorkspaceRegistration } from "../../../packages/protocol-types";

const mocked = vi.hoisted(() => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: mocked.invoke }));

const binding: WorkspaceRegistration = { server_origin: "https://example.invalid", workspace_id: "workspace", workspace_epoch: "epoch", device_id: "device", device_stream_id: "stream" };
const documentId = "019a1000-0000-7000-8000-000000000002";
const operationId = "019a1000-0000-7000-8000-000000000003";
const snapshot: ConflictReviewSnapshot = {
  binding, node_id: documentId, operation_id: operationId, operation_seq: "1", generation: "2",
  base_revision: "base-rev", base_bytes: new TextEncoder().encode("base\n"), base_hash: "sha256:f34848ca92665c342abd5816c9e3eda0e82180671195362bcd0080544a3bc2ac",
  local_bytes: new TextEncoder().encode("local\n"), local_hash: "sha256:efb83f2a277e9f49b38efd505f5cbb93885e721b6bd16b788937c9396174c006",
  conflict_revision: "revision-at-412", remote_revision: "current-rev", remote_bytes: new TextEncoder().encode("current\n"), remote_hash: "sha256:48aa6cae8c70abdb28631d22b316e6d9f9d0768ec2911de7090e248b2afe6ca1",
};

function respond(value: unknown) {
  mocked.invoke.mockImplementation(async (command: string) => command === "m0_renderer_session" ? "owner" : value);
}

const choose = (shown: ConflictReviewSnapshot, session: string, expectedVersion: string | null): Promise<WorkspaceContentConflictChooseRemoteResponse> =>
  (ipc as unknown as { chooseWorkspaceRemoteContentConflict(snapshot: ConflictReviewSnapshot, session: string, expectedVersion: string | null): Promise<WorkspaceContentConflictChooseRemoteResponse> })
    .chooseWorkspaceRemoteContentConflict(shown, session, expectedVersion);

beforeEach(() => mocked.invoke.mockReset());

test("명시적 원격판 선택은 표시한 snapshot의 등록과 revision·hash를 그대로 native에 전달한다", async () => {
  respond({ status: "pending", version: "0" });
  await expect(choose(snapshot, "session", null)).resolves.toEqual({ status: "pending", version: "0" });
  expect(mocked.invoke).toHaveBeenCalledWith("workspace_choose_remote_content_conflict", { request: {
    renderer_session_id: "owner", binding, document_id: documentId, edit_session_id: "session", operation_id: operationId, generation: "2", expected_intent_version: null,
    base_revision: "base-rev", conflict_revision: "revision-at-412", remote_revision: "current-rev",
    base_hash: snapshot.base_hash, local_hash: snapshot.local_hash, remote_hash: snapshot.remote_hash,
  } });
});

test("보류 선택 재검토 version과 완료 generation을 보존한다", async () => {
  respond({ status: "completed", generation: "3" });
  await expect(choose(snapshot, "session", "4")).resolves.toEqual({ status: "completed", generation: "3" });
  expect(mocked.invoke).toHaveBeenCalledWith("workspace_choose_remote_content_conflict", { request: expect.objectContaining({ expected_intent_version: "4" }) });
});

test("잘못된 문서·작업 ID, 편집 session과 generation은 native 호출 전에 거절한다", async () => {
  respond({ status: "pending", version: "0" });
  for (const [shown, session] of [
    [{ ...snapshot, node_id: "invalid" }, "session"], [{ ...snapshot, operation_id: "invalid" }, "session"],
    [{ ...snapshot, binding: { ...binding, device_stream_id: null } as unknown as WorkspaceRegistration }, "session"],
    [{ ...snapshot, generation: "-1" }, "session"], [{ ...snapshot, generation: "01" }, "session"],
    [{ ...snapshot, generation: "9223372036854775808" }, "session"], [snapshot, ""],
  ] as const) {
    mocked.invoke.mockClear();
    await expect(choose(shown, session, null)).rejects.toThrow();
    expect(mocked.invoke).not.toHaveBeenCalledWith("workspace_choose_remote_content_conflict", expect.anything());
  }
});

test("현재 서버 revision은 이전 412 조건과 달리 256바이트를 넘을 수 있다", async () => {
  respond({ status: "pending", version: "0" });
  const remoteRevision = "x".repeat(257);
  await expect(choose({ ...snapshot, remote_revision: remoteRevision }, "session", null)).resolves.toEqual({ status: "pending", version: "0" });
  expect(mocked.invoke).toHaveBeenCalledWith("workspace_choose_remote_content_conflict", { request: expect.objectContaining({ remote_revision: remoteRevision }) });
});

test("잘못된 선택 version, revision과 각 원문의 hash는 native 호출 전에 거절한다", async () => {
  respond({ status: "pending", version: "0" });
  for (const bad of ["-1", "1.1", "01", "9223372036854775808"]) {
    mocked.invoke.mockClear();
    await expect(choose(snapshot, "session", bad)).rejects.toThrow();
    expect(mocked.invoke).not.toHaveBeenCalledWith("workspace_choose_remote_content_conflict", expect.anything());
  }
  for (const shown of [
    { ...snapshot, base_revision: "" }, { ...snapshot, conflict_revision: "" }, { ...snapshot, remote_revision: "" },
    { ...snapshot, base_revision: "x".repeat(257) }, { ...snapshot, conflict_revision: "x".repeat(257) },
    { ...snapshot, base_revision: "bad\nrevision" }, { ...snapshot, conflict_revision: "bad\nrevision" }, { ...snapshot, remote_revision: "bad\nrevision" },
    { ...snapshot, base_hash: "sha256:" + "a".repeat(63) }, { ...snapshot, local_hash: "sha256:" + "A".repeat(64) },
    { ...snapshot, remote_hash: "sha256:" + "g".repeat(64) },
  ]) {
    mocked.invoke.mockClear();
    await expect(choose(shown, "session", null)).rejects.toThrow();
    expect(mocked.invoke).not.toHaveBeenCalledWith("workspace_choose_remote_content_conflict", expect.anything());
  }
});

test("알 수 없는 상태, 누락 또는 조작된 decimal과 다른 상태의 필드는 응답으로 받아들이지 않는다", async () => {
  const invalid = [undefined, null, 1, "", "-1", "1.1", "01", "9223372036854775808"];
  for (const result of [
    null, {}, { status: "unknown" }, { status: "pending" }, { status: "completed" },
    { status: "pending", version: "0", generation: "3" }, { status: "completed", generation: "3", version: "0" },
    ...invalid.flatMap((value) => [{ status: "pending", version: value }, { status: "completed", generation: value }]),
  ]) {
    respond(result);
    await expect(choose(snapshot, "session", null)).rejects.toThrow();
  }
});
