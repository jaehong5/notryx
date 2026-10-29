import { beforeEach, expect, test, vi } from "vitest";
import * as ipc from "./ipc";
import type { ConflictReviewSnapshot } from "./conflict-review";
import type { WorkspaceContentConflictChooseRemoteResponse, WorkspaceRegistration } from "../../../packages/protocol-types";

const mocked = vi.hoisted(() => ({ invoke:vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke:mocked.invoke }));

const binding: WorkspaceRegistration = { server_origin:"https://example.invalid", workspace_id:"workspace", workspace_epoch:"epoch", device_id:"device", device_stream_id:"stream" };
const documentId = "019a1000-0000-7000-8000-000000000002";
const operationId = "019a1000-0000-7000-8000-000000000003";
const snapshot: ConflictReviewSnapshot = {
  binding, node_id:documentId, operation_id:operationId, operation_seq:"1", generation:"2",
  base_revision:"base-rev", base_bytes:new TextEncoder().encode("base\n"), base_hash:"sha256:f34848ca92665c342abd5816c9e3eda0e82180671195362bcd0080544a3bc2ac",
  local_bytes:new TextEncoder().encode("local\n"), local_hash:"sha256:efb83f2a277e9f49b38efd505f5cbb93885e721b6bd16b788937c9396174c006",
  conflict_revision:"revision-at-412", remote_revision:"current-rev", remote_bytes:new TextEncoder().encode("current\n"), remote_hash:"sha256:48aa6cae8c70abdb28631d22b316e6d9f9d0768ec2911de7090e248b2afe6ca1",
};
const confirm = (shown:ConflictReviewSnapshot, session:string, expectedVersion:string | null):Promise<WorkspaceContentConflictChooseRemoteResponse> =>
  (ipc as unknown as { confirmWorkspaceContentConflictMerge(shown:ConflictReviewSnapshot, session:string, expectedVersion:string | null):Promise<WorkspaceContentConflictChooseRemoteResponse> })
    .confirmWorkspaceContentConflictMerge(shown, session, expectedVersion);

function respond(value:unknown) {
  mocked.invoke.mockImplementation(async (command:string) => command === "m0_renderer_session" ? "owner" : value);
}
beforeEach(() => mocked.invoke.mockReset());

test("합본 확인은 표시한 등록과 문서·session·generation 및 revision·hash를 native에 전달한다", async () => {
  respond({ status:"pending", version:"0" });
  await expect(confirm(snapshot, "session", null)).resolves.toEqual({ status:"pending", version:"0" });
  expect(mocked.invoke).toHaveBeenCalledWith("workspace_confirm_content_conflict_merge", { request:{
    renderer_session_id:"owner", binding, document_id:documentId, edit_session_id:"session", operation_id:operationId,
    generation:"2", expected_intent_version:null, base_revision:"base-rev", conflict_revision:"revision-at-412", remote_revision:"current-rev",
    base_hash:snapshot.base_hash, local_hash:snapshot.local_hash, remote_hash:snapshot.remote_hash,
  } });
});

test("명시 재검토는 기존 intent version을 전달하고 현재 remote revision을 길이로 자르지 않는다", async () => {
  respond({ status:"pending", version:"5" });
  const remoteRevision = "x".repeat(257);
  await expect(confirm({ ...snapshot, remote_revision:remoteRevision }, "session", "4")).resolves.toEqual({ status:"pending", version:"5" });
  expect(mocked.invoke).toHaveBeenCalledWith("workspace_confirm_content_conflict_merge", { request:expect.objectContaining({ expected_intent_version:"4", remote_revision:remoteRevision }) });
});

test("동일 확인의 완료 결과는 선택 generation의 다음 세대만 허용한다", async () => {
  respond({ status:"completed", generation:"3" });
  await expect(confirm(snapshot, "session", null)).resolves.toEqual({ status:"completed", generation:"3" });
  expect(mocked.invoke).toHaveBeenCalledWith("workspace_confirm_content_conflict_merge", { request:expect.objectContaining({ generation:"2" }) });
  for (const result of [
    { status:"completed" }, { status:"completed", generation:"0" }, { status:"completed", generation:"2" },
    { status:"completed", generation:"4" }, { status:"completed", generation:"03" },
    { status:"completed", generation:"9223372036854775808" }, { status:"completed", generation:"3", version:"0" },
    { status:"completed", generation:"3", unexpected:true },
  ]) {
    respond(result);
    await expect(confirm(snapshot, "session", null)).rejects.toThrow();
  }
  respond({ status:"completed", generation:"9223372036854775807" });
  await expect(confirm({ ...snapshot, generation:"9223372036854775807" }, "session", null)).rejects.toThrow();
});

test("선택 요청에 맞지 않는 pending version을 거절한다", async () => {
  for (const version of ["1", "2", "9223372036854775807"]) {
    respond({ status:"pending", version });
    await expect(confirm(snapshot, "session", null)).rejects.toThrow();
  }
  for (const version of ["4", "5"]) {
    respond({ status:"pending", version });
    await expect(confirm(snapshot, "session", "4")).resolves.toEqual({ status:"pending", version });
  }
  for (const version of ["0", "3", "6", "9223372036854775807"]) {
    respond({ status:"pending", version });
    await expect(confirm(snapshot, "session", "4")).rejects.toThrow();
  }
});

test("잘못된 등록·문서·작업 ID, session과 generation은 합본 확인 전에 거절한다", async () => {
  respond({ status:"pending", version:"0" });
  for (const [shown, session] of [
    [{ ...snapshot, node_id:"invalid" }, "session"], [{ ...snapshot, operation_id:"invalid" }, "session"],
    [{ ...snapshot, binding:{ ...binding, device_stream_id:null } as unknown as WorkspaceRegistration }, "session"],
    [{ ...snapshot, generation:"-1" }, "session"], [{ ...snapshot, generation:"01" }, "session"],
    [{ ...snapshot, generation:"9223372036854775808" }, "session"], [snapshot, ""],
  ] as const) {
    mocked.invoke.mockClear();
    await expect(confirm(shown, session, null)).rejects.toThrow();
    expect(mocked.invoke).not.toHaveBeenCalledWith("workspace_confirm_content_conflict_merge", expect.anything());
  }
});

test("잘못된 intent version, revision과 각 원문 hash는 native 호출 전에 거절한다", async () => {
  respond({ status:"pending", version:"0" });
  for (const bad of ["-1", "1.1", "01", "9223372036854775808"]) {
    mocked.invoke.mockClear();
    await expect(confirm(snapshot, "session", bad)).rejects.toThrow();
    expect(mocked.invoke).not.toHaveBeenCalledWith("workspace_confirm_content_conflict_merge", expect.anything());
  }
  for (const shown of [
    { ...snapshot, base_revision:"" }, { ...snapshot, conflict_revision:"" }, { ...snapshot, remote_revision:"" },
    { ...snapshot, base_revision:"x".repeat(257) }, { ...snapshot, conflict_revision:"x".repeat(257) },
    { ...snapshot, base_revision:"bad\nrevision" }, { ...snapshot, conflict_revision:"bad\nrevision" }, { ...snapshot, remote_revision:"bad\nrevision" },
    { ...snapshot, base_hash:"sha256:" + "a".repeat(63) }, { ...snapshot, local_hash:"sha256:" + "A".repeat(64) }, { ...snapshot, remote_hash:"sha256:" + "g".repeat(64) },
  ]) {
    mocked.invoke.mockClear();
    await expect(confirm(shown, "session", null)).rejects.toThrow();
    expect(mocked.invoke).not.toHaveBeenCalledWith("workspace_confirm_content_conflict_merge", expect.anything());
  }
});

test("알 수 없는 선택 상태와 누락된 version 또는 다른 상태의 필드는 거절한다", async () => {
  const invalid = [undefined, null, 1, "", "-1", "1.1", "01", "9223372036854775808"];
  for (const result of [
    null, {}, { status:"unknown" }, { status:"pending" },
    { status:"completed", generation:"3", version:"0" },
    { status:"pending", version:"0", generation:"3" }, { status:"pending", version:"0", unexpected:true },
    ...invalid.map((version) => ({ status:"pending", version })),
  ]) {
    respond(result);
    await expect(confirm(snapshot, "session", null)).rejects.toThrow();
  }
});
