import { beforeEach, expect, test, vi } from "vitest";
import { prepareWorkspaceContentConflict } from "./ipc";
import type { WorkspaceRegistration } from "../../../packages/protocol-types";

const mocked = vi.hoisted(() => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: mocked.invoke }));

const binding: WorkspaceRegistration = { server_origin: "https://example.invalid", workspace_id: "workspace", workspace_epoch: "epoch", device_id: "device", device_stream_id: "stream" };
const documentId = "019a1000-0000-7000-8000-000000000002";
const operationId = "019a1000-0000-7000-8000-000000000003";
const baseHash = "sha256:b934e87225fc7f342fe3256003befdfed041748599ba0344837b2c9266d2b10e";
const localHash = "sha256:efb83f2a277e9f49b38efd505f5cbb93885e721b6bd16b788937c9396174c006";
const remoteHash = "sha256:218843cc490e758cf3569e13443acb86b2f6b9d9893485ff4d422bf3ec75ef59";
const result = { binding, document_id: documentId, operation_id: operationId, operation_seq: "1", generation: "2", base_revision: "base", base_bytes_base64: "77u/YmFzZQ0K", base_hash: baseHash, local_bytes_base64: "bG9jYWwK", local_hash: localHash, conflict_revision: "failed", remote_revision: "current", remote_bytes_base64: "cmVtb3RlDQo=", remote_hash: remoteHash };

function respond(value: unknown) {
  mocked.invoke.mockImplementation(async (command: string) => command === "m0_renderer_session" ? "owner" : value);
}
beforeEach(() => mocked.invoke.mockReset());

test("충돌 비교 IPC는 BOM과 CRLF가 포함된 원문과 세 원문의 hash를 보존한다", async () => {
  respond(result);
  const snapshot = await prepareWorkspaceContentConflict(binding, documentId, "session", "2");
  expect(snapshot).toMatchObject({ base_hash: baseHash, local_hash: localHash, remote_hash: remoteHash });
  expect(Array.from(snapshot!.base_bytes)).toEqual([0xef, 0xbb, 0xbf, 0x62, 0x61, 0x73, 0x65, 0x0d, 0x0a]);
  expect(Array.from(snapshot!.remote_bytes)).toEqual([0x72, 0x65, 0x6d, 0x6f, 0x74, 0x65, 0x0d, 0x0a]);
});

test("충돌 비교 IPC는 각 원문의 누락되거나 올바르지 않은 hash를 거절한다", async () => {
  for (const field of ["base_hash", "local_hash", "remote_hash"] as const) {
    for (const bad of [undefined, "", "sha256:" + "a".repeat(63), "sha256:" + "A".repeat(64), "sha256:" + "g".repeat(64), 42]) {
      respond({ ...result, [field]: bad });
      await expect(prepareWorkspaceContentConflict(binding, documentId, "session", "2")).rejects.toThrow();
    }
  }
});
