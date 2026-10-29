import { expect, test, vi } from "vitest";
import { prepareWorkspaceContentConflict } from "./ipc";
import type { WorkspaceRegistration } from "../../../packages/protocol-types";

const mocked = vi.hoisted(() => ({ invoke:vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke:mocked.invoke }));
const binding: WorkspaceRegistration = { server_origin:"https://example.invalid", workspace_id:"workspace", workspace_epoch:"epoch", device_id:"device", device_stream_id:"stream" };
const documentId = "019a1000-0000-7000-8000-000000000002", operationId = "019a1000-0000-7000-8000-000000000003";

test("현재 서버 revision의 유효한 긴 strong ETag 값은 과거 412 조건의 크기 한도와 구분한다", async () => {
  const longRevision = "x".repeat(257);
  const value = { binding, document_id:documentId, operation_id:operationId, operation_seq:"1", generation:"2", base_revision:"base", base_bytes_base64:btoa("base"), base_hash:"sha256:cae662172fd450bb0cd710a769079c05bfc5d8e35efa6576edc7d0377afdd4a2", local_bytes_base64:btoa("local"), local_hash:"sha256:25bf8e1a2393f1108d37029b3df5593236c755742ec93465bbafa9b290bddcf6", conflict_revision:"failed", remote_revision:longRevision, remote_bytes_base64:btoa("remote"), remote_hash:"sha256:b71199ebd070b36beab7317920c2c2f1d777df8d05e5527d8458fda57cb17a7a" };
  mocked.invoke.mockImplementation(async (command:string) => command === "m0_renderer_session" ? "owner" : value);
  const snapshot = await prepareWorkspaceContentConflict(binding, documentId, "session", "2");
  expect(snapshot?.remote_revision).toBe(longRevision);
});
