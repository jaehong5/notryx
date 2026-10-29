import { beforeEach, expect, test, vi } from "vitest";
import * as ipc from "./ipc";
import type { WorkspaceRegistration } from "../../../packages/protocol-types";

const mocked = vi.hoisted(() => ({ invoke:vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke:mocked.invoke }));
const binding: WorkspaceRegistration = { server_origin:"https://example.invalid", workspace_id:"workspace", workspace_epoch:"epoch", device_id:"device", device_stream_id:"stream" };
const documentId = "019a1000-0000-7000-8000-000000000002", operationId = "019a1000-0000-7000-8000-000000000003";
const result = { binding, document_id:documentId, operation_id:operationId, operation_seq:"1", generation:"2", base_revision:"base-rev", base_bytes_base64:btoa("base\n"), base_hash:"sha256:f34848ca92665c342abd5816c9e3eda0e82180671195362bcd0080544a3bc2ac", local_bytes_base64:btoa("local\n"), local_hash:"sha256:efb83f2a277e9f49b38efd505f5cbb93885e721b6bd16b788937c9396174c006", conflict_revision:"revision-at-412", remote_revision:"current-rev", remote_bytes_base64:btoa("current\n"), remote_hash:"sha256:48aa6cae8c70abdb28631d22b316e6d9f9d0768ec2911de7090e248b2afe6ca1" };
function respond(value:unknown) { mocked.invoke.mockImplementation(async (command:string) => command === "m0_renderer_session" ? "owner" : value); }
beforeEach(() => mocked.invoke.mockReset());

test("충돌 비교 IPC는 기기 checkpoint 세대와 등록을 전달하고 서로 다른 시점의 revision과 bytes를 보존한다", async () => {
  respond(result);
  const snapshot = await ipc.prepareWorkspaceContentConflict(binding, documentId, "session", "2");
  expect(mocked.invoke).toHaveBeenCalledWith("workspace_prepare_content_conflict", { request:{ renderer_session_id:"owner", binding, document_id:documentId, edit_session_id:"session", generation:"2" } });
  expect(snapshot).toMatchObject({ binding, node_id:documentId, operation_id:operationId, operation_seq:"1", generation:"2", base_revision:"base-rev", conflict_revision:"revision-at-412", remote_revision:"current-rev" });
  expect(new TextDecoder().decode(snapshot!.base_bytes)).toBe("base\n");
  expect(new TextDecoder().decode(snapshot!.local_bytes)).toBe("local\n");
  expect(new TextDecoder().decode(snapshot!.remote_bytes)).toBe("current\n");
});

test("현재 세션에 412 충돌이 없으면 빈 원문으로 만들지 않고 null을 반환한다", async () => {
  respond(null);
  expect(await ipc.prepareWorkspaceContentConflict(binding, documentId, "session", "2")).toBeNull();
});

test("generation 0의 충돌 없음은 허용하지만 조작된 충돌 snapshot은 거절한다", async () => {
  respond(null);
  expect(await ipc.prepareWorkspaceContentConflict(binding, documentId, "session", "0")).toBeNull();
  respond({ ...result, generation:"0" });
  await expect(ipc.prepareWorkspaceContentConflict(binding, documentId, "session", "0")).rejects.toThrow();
});

test("다른 등록·문서·generation 또는 잘못된 작업 ID와 revision은 거절한다", async () => {
  for (const wrong of [
    { ...result, binding:{ ...binding, device_stream_id:"other" } }, { ...result, document_id:"019a1000-0000-7000-8000-000000000004" },
    { ...result, generation:"3" }, { ...result, operation_id:"invalid" }, { ...result, operation_seq:"0" },
    { ...result, base_revision:"" }, { ...result, conflict_revision:"" }, { ...result, conflict_revision:"base-rev" }, { ...result, remote_revision:"" },
  ]) {
    respond(wrong); await expect(ipc.prepareWorkspaceContentConflict(binding, documentId, "session", "2")).rejects.toThrow();
  }
});

test("잘못된 base64와 비UTF-8, 편집 한도를 초과한 원문은 비교 결과로 표시하지 않는다", async () => {
  for (const wrong of [
    { ...result, base_bytes_base64:"%%" }, { ...result, local_bytes_base64:"/w==" },
    { ...result, remote_bytes_base64:btoa("a".repeat(5 * 1024 * 1024 + 1)) },
  ]) {
    respond(wrong); await expect(ipc.prepareWorkspaceContentConflict(binding, documentId, "session", "2")).rejects.toThrow();
  }
});

test("잘못된 입력은 native 호출 전에 거절한다", async () => {
  respond(null);
  for (const [id, generation] of [["invalid", "2"], [documentId, "-1"], [documentId, "18446744073709551616"]]) {
    mocked.invoke.mockClear(); await expect(ipc.prepareWorkspaceContentConflict(binding, id, "session", generation)).rejects.toThrow();
    expect(mocked.invoke).not.toHaveBeenCalledWith("workspace_prepare_content_conflict", expect.anything());
  }
});
