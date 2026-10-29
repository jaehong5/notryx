import { beforeEach, expect, test, vi } from "vitest";
import { queueWorkspaceRestore, sendNextWorkspaceOperation, workspaceRestoreStatus, workspaceTrash } from "./ipc";
import * as ipc from "./ipc";
import type { WorkspaceRegistration } from "../../../packages/protocol-types";
const mocked = vi.hoisted(() => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: mocked.invoke }));
const binding: WorkspaceRegistration = { server_origin:"https://example.invalid", workspace_id:"workspace", workspace_epoch:"epoch", device_id:"device", device_stream_id:"stream" };
const node = "019a1000-0000-7000-8000-000000000002";
const parent = "019a1000-0000-7000-8000-000000000001";
const operation_id = "019a1000-0000-7000-8000-000000000003";
const request = { expected_meta_rev:"meta-1", parent_id:parent, name:"restored.md" };
const entry = { operation_id, operation_seq:"1", node_id:node, request, status:"queued", receipt:null };
beforeEach(() => mocked.invoke.mockReset());

test("휴지통 페이지의 epoch와 경계를 확인한다", async () => {
  mocked.invoke.mockImplementation(async (command: string) => command === "m0_renderer_session" ? "owner" : { workspace_epoch:"epoch", nodes:[{ node_id:node, parent_id:parent, name:"file.md", kind:"file", meta_rev:"meta-1", restorable:true }], next_after_id:null });
  expect((await workspaceTrash(binding, null)).nodes[0].node_id).toBe(node);
  mocked.invoke.mockImplementation(async (command: string) => command === "m0_renderer_session" ? "owner" : { workspace_epoch:"other", nodes:[], next_after_id:null });
  await expect(workspaceTrash(binding, null)).rejects.toThrow();
});

test("복원 enqueue 결과가 요청과 다르면 거절한다", async () => {
  mocked.invoke.mockImplementation(async (command: string) => command === "m0_renderer_session" ? "owner" : entry);
  expect((await queueWorkspaceRestore(binding, node, request)).status).toBe("queued");
  mocked.invoke.mockImplementation(async (command: string) => command === "m0_renderer_session" ? "owner" : { ...entry, node_id:parent });
  await expect(queueWorkspaceRestore(binding, node, request)).rejects.toThrow();
});

test("복원 상태 페이지는 binding과 실패 receipt identity를 확인한다", async () => {
  const failed = { ...entry, status:"failed", receipt:{ status:"history_expired", operation_id, operation_seq:"1", workspace_epoch:"epoch" } };
  mocked.invoke.mockImplementation(async (command: string) => command === "m0_renderer_session" ? "owner" : { binding, entries:[failed], next_after_seq:null });
  expect((await workspaceRestoreStatus(binding, "0")).entries[0].reason).toBe("history_expired");
  mocked.invoke.mockImplementation(async (command: string) => command === "m0_renderer_session" ? "owner" : { binding, entries:[{...failed, receipt:{...failed.receipt, workspace_epoch:"other"}}], next_after_seq:null });
  await expect(workspaceRestoreStatus(binding, "0")).rejects.toThrow();
});

test("복원 취소 중과 취소 완료의 receipt를 구분하고 identity를 확인한다", async () => {
  const abandoning = { ...entry, status:"abandoning" };
  const receipt = { status:"abandoned", operation_id, operation_seq:"1", workspace_epoch:"epoch" };
  const abandoned = { ...entry, status:"abandoned", receipt };
  const page = (value: unknown) => ({ binding, entries:[value], next_after_seq:null });
  const setResult = (value: unknown) => mocked.invoke.mockImplementation(async (command: string) => command === "m0_renderer_session" ? "owner" : page(value));
  setResult(abandoning);
  expect((await workspaceRestoreStatus(binding, "0")).entries[0].status).toBe("abandoning");
  setResult(abandoned);
  expect((await workspaceRestoreStatus(binding, "0")).entries[0].status).toBe("abandoned");
  setResult({ ...abandoning, receipt });
  await expect(workspaceRestoreStatus(binding, "0")).rejects.toThrow();
  setResult({ ...abandoned, receipt:{ ...receipt, workspace_epoch:"other" } });
  await expect(workspaceRestoreStatus(binding, "0")).rejects.toThrow();
  setResult({ ...abandoned, receipt:{ ...receipt, operation_seq:"2" } });
  await expect(workspaceRestoreStatus(binding, "0")).rejects.toThrow();
  setResult({ ...abandoned, receipt:{ ...receipt, status:"restored" } });
  await expect(workspaceRestoreStatus(binding, "0")).rejects.toThrow();
});

test("복원 권한 거절 응답은 작업을 식별하고 누락된 identity를 거절한다", async () => {
  const denied = { binding, status:"permission_denied", operation_id, document_id:node, generation:null, content_block:null };
  mocked.invoke.mockImplementation(async (command: string) => command === "m0_renderer_session" ? "owner" : denied);
  const result = await sendNextWorkspaceOperation();
  expect(result.status).toBe("permission_denied");
  expect(result.operation_id).toBe(operation_id);
  mocked.invoke.mockImplementation(async (command: string) => command === "m0_renderer_session" ? "owner" : { ...denied, operation_id:null });
  await expect(sendNextWorkspaceOperation()).rejects.toThrow();
  mocked.invoke.mockImplementation(async (command: string) => command === "m0_renderer_session" ? "owner" : { ...denied, operation_id:"invalid" });
  await expect(sendNextWorkspaceOperation()).rejects.toThrow();
  mocked.invoke.mockImplementation(async (command: string) => command === "m0_renderer_session" ? "owner" : { ...denied, document_id:null });
  await expect(sendNextWorkspaceOperation()).rejects.toThrow();
  mocked.invoke.mockImplementation(async (command: string) => command === "m0_renderer_session" ? "owner" : { ...denied, generation:"1" });
  await expect(sendNextWorkspaceOperation()).rejects.toThrow();
});

test("복원 취소는 현재 renderer와 전체 등록을 포함해 명시적으로 요청한다", async () => {
  const target = ipc as typeof ipc & { requestWorkspaceRestoreAbandon(binding: WorkspaceRegistration, operationId: string): Promise<void> };
  mocked.invoke.mockImplementation(async (command: string) => command === "m0_renderer_session" ? "owner" : undefined);
  await target.requestWorkspaceRestoreAbandon(binding, operation_id);
  expect(mocked.invoke).toHaveBeenCalledWith("workspace_request_restore_abandon", { request:{ renderer_session_id:"owner", binding, operation_id } });
  mocked.invoke.mockClear();
  await expect(target.requestWorkspaceRestoreAbandon(binding, "invalid")).rejects.toThrow();
  expect(mocked.invoke).not.toHaveBeenCalledWith("workspace_request_restore_abandon", expect.anything());
});
