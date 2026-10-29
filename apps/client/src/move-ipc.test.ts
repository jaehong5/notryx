import { beforeEach, expect, test, vi } from "vitest";
import * as ipc from "./ipc";
import type { WorkspaceRegistration } from "../../../packages/protocol-types";

const mocked = vi.hoisted(() => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: mocked.invoke }));
const binding: WorkspaceRegistration = { server_origin:"https://example.invalid", workspace_id:"workspace", workspace_epoch:"epoch", device_id:"device", device_stream_id:"stream" };
const node = "019a1000-0000-7000-8000-000000000002";
const operation = "019a1000-0000-7000-8000-000000000003";
const parent = "019a1000-0000-7000-8000-000000000004";
const destination = "019a1000-0000-7000-8000-000000000005";
const other = "019a1000-0000-7000-8000-000000000006";
const request = { expected_parent_id:parent, parent_id:destination, name:"이동.md" };
const input = { node_id:node, expected_meta_etag:'"meta-old"', request };
const queued = { operation_id:operation, operation_seq:"1", ...input, status:"queued", receipt:null };
const page = (entry: unknown, value: WorkspaceRegistration = binding) => ({ binding:value, entries:[entry], next_after_seq:null });
function respond(value: unknown) { mocked.invoke.mockImplementation(async (command: string) => command === "m0_renderer_session" ? "owner" : value); }
beforeEach(() => mocked.invoke.mockReset());

const movedNode = {
  node_id:node, parent_id:destination, name:request.name, kind:"file", deleted:false,
  content_rev:"content", content_etag:'"content"', meta_rev:"meta-new", meta_etag:'"meta-new"', node_etag:'"node-new"',
  content_hash:"sha256:" + "a".repeat(64), size:"1", children_generation:"0", last_change_seq:"8",
};
const moved = { status:"moved", operation_id:operation, operation_seq:"1", workspace_epoch:binding.workspace_epoch, node:movedNode, commit_seq:"8" };
const stale = { status:"metadata_precondition_failed", operation_id:operation, operation_seq:"1", workspace_epoch:binding.workspace_epoch, current_meta_rev:"meta-new", current_meta_etag:'"meta-new"' };

test("이동 요청은 확인한 문서, meta ETag와 기존 부모, 목적지와 이름을 그대로 전송한다", async () => {
  respond(queued);
  expect(await ipc.queueWorkspaceMove(binding, input)).toMatchObject({ ...input, status:"queued", reason:null });
  expect(mocked.invoke).toHaveBeenCalledWith("workspace_queue_move", { request:{ renderer_session_id:"owner", binding, ...input } });
  for (const reply of [
    { ...queued, node_id:other },
    { ...queued, expected_meta_etag:'"changed"' },
    { ...queued, request:{ ...request, expected_parent_id:other } },
    { ...queued, request:{ ...request, parent_id:other } },
    { ...queued, request:{ ...request, name:"changed.md" } },
    { ...queued, operation_seq:"0" }, { ...queued, status:"sent" }, { ...queued, receipt:{} },
  ]) {
    respond(reply);
    await expect(ipc.queueWorkspaceMove(binding, input)).rejects.toThrow();
  }
});

test("이동 입력의 잘못된 ID와 구조 조건을 native 호출 전에 거절한다", async () => {
  respond(queued);
  for (const invalid of [
    { ...input, node_id:"invalid" },
    { ...input, expected_meta_etag:'W/"meta-old"' },
    { ...input, expected_meta_etag:'"' + "x".repeat(255) + '"' },
    { ...input, request:{ ...request, expected_parent_id:"invalid" } },
    { ...input, request:{ ...request, parent_id:"invalid" } },
    { ...input, request:{ ...request, parent_id:node } },
    { ...input, request:{ ...request, expected_parent_id:node } },
  ]) {
    mocked.invoke.mockClear();
    await expect(ipc.queueWorkspaceMove(binding, invalid)).rejects.toThrow();
    expect(mocked.invoke).not.toHaveBeenCalledWith("workspace_queue_move", expect.anything());
  }
  const unicode = '"서버😀"';
  respond({ ...queued, expected_meta_etag:unicode });
  expect((await ipc.queueWorkspaceMove(binding, { ...input, expected_meta_etag:unicode })).expected_meta_etag).toBe(unicode);
});

test("목적지 충돌과 취소 결과는 각 terminal 상태로 구분한다", async () => {
  for (const reason of ["destination_exists", "expected_parent_mismatch", "recovery_conflict", "node_deleted", "parent_not_folder", "not_found", "not_applied"]) {
    const receipt = { status:reason, operation_id:operation, operation_seq:"1", workspace_epoch:binding.workspace_epoch };
    respond(page({ ...queued, status:"failed", receipt }));
    expect((await ipc.workspaceMoveStatus(binding, "0")).entries[0]).toMatchObject({ status:"failed", reason });
  }
  const receipt = { status:"abandoned", operation_id:operation, operation_seq:"1", workspace_epoch:binding.workspace_epoch };
  respond(page({ ...queued, status:"abandoned", receipt }));
  expect((await ipc.workspaceMoveStatus(binding, "0")).entries[0]).toMatchObject({ status:"abandoned", reason:null });
  respond(page({ ...queued, status:"committed", receipt }));
  await expect(ipc.workspaceMoveStatus(binding, "0")).rejects.toThrow();
});

test("이동 412의 작업 ID, 순번, epoch와 meta revision을 확인하고 새 요청을 만들지 않는다", async () => {
  respond(page({ ...queued, status:"failed", receipt:stale }));
  expect((await ipc.workspaceMoveStatus(binding, "0")).entries[0]).toMatchObject({ ...input, status:"failed", reason:"metadata_precondition_failed" });
  for (const receipt of [
    { ...stale, operation_id:other }, { ...stale, operation_seq:"2" }, { ...stale, workspace_epoch:"other" },
    { ...stale, current_meta_rev:"meta-old", current_meta_etag:'"meta-old"' },
    { ...stale, current_meta_rev:"different", current_meta_etag:'"meta-new"' },
    { ...stale, current_meta_etag:'W/"meta-new"' }, { ...stale, status:"deleted" },
  ]) {
    respond(page({ ...queued, status:"failed", receipt }));
    await expect(ipc.workspaceMoveStatus(binding, "0")).rejects.toThrow();
  }
  expect(mocked.invoke).not.toHaveBeenCalledWith("workspace_queue_move", expect.anything());
});

test("서버가 저장한 이동 결과의 대상, 경로와 revision이 요청과 일치할 때만 완료로 표시한다", async () => {
  respond(page({ ...queued, status:"committed", receipt:moved }));
  expect((await ipc.workspaceMoveStatus(binding, "0")).entries[0]).toMatchObject({ ...input, status:"committed", reason:null });
  for (const receipt of [
    { ...moved, operation_id:other }, { ...moved, operation_seq:"2" }, { ...moved, workspace_epoch:"other" },
    { ...moved, node:{ ...movedNode, node_id:other } },
    { ...moved, node:{ ...movedNode, parent_id:other } },
    { ...moved, node:{ ...movedNode, name:"other.md" } },
    { ...moved, node:{ ...movedNode, kind:"folder" } },
    { ...moved, node:{ ...movedNode, deleted:true } },
    { ...moved, node:{ ...movedNode, meta_rev:"meta-old", meta_etag:'"meta-old"' } },
    { ...moved, commit_seq:"9" },
  ]) {
    respond(page({ ...queued, status:"committed", receipt }));
    await expect(ipc.workspaceMoveStatus(binding, "0")).rejects.toThrow();
  }
});

test("서버 journal의 u64 순번과 선행 0이 있는 문서 크기를 수용한다", async () => {
  for (const seq of ["9223372036854775808", "18446744073709551615"]) {
    const receipt = { ...moved, commit_seq:seq, node:{ ...movedNode, last_change_seq:seq, size:"00" } };
    respond(page({ ...queued, status:"committed", receipt }));
    expect((await ipc.workspaceMoveStatus(binding, "0")).entries[0].status).toBe("committed");
  }
  const seq = "18446744073709551616";
  respond(page({ ...queued, status:"committed", receipt:{ ...moved, commit_seq:seq, node:{ ...movedNode, last_change_seq:seq } } }));
  await expect(ipc.workspaceMoveStatus(binding, "0")).rejects.toThrow();
});

test("이동 상태 페이지의 등록, 순서와 cursor 및 취소 ID를 검증한다", async () => {
  respond(page(queued, { ...binding, device_stream_id:"other" }));
  await expect(ipc.workspaceMoveStatus(binding, "0")).rejects.toThrow();
  respond({ binding, entries:[queued, queued], next_after_seq:null });
  await expect(ipc.workspaceMoveStatus(binding, "0")).rejects.toThrow();
  respond({ binding, entries:[queued], next_after_seq:"2" });
  await expect(ipc.workspaceMoveStatus(binding, "0")).rejects.toThrow();
  respond({ binding, entries:[], next_after_seq:"1" });
  await expect(ipc.workspaceMoveStatus(binding, "0")).rejects.toThrow();
  mocked.invoke.mockClear();
  await expect(ipc.requestWorkspaceMoveAbandon(binding, "invalid")).rejects.toThrow();
  expect(mocked.invoke).not.toHaveBeenCalledWith("workspace_request_move_abandon", expect.anything());
  await ipc.requestWorkspaceMoveAbandon(binding, operation);
  expect(mocked.invoke).toHaveBeenCalledWith("workspace_request_move_abandon", { request:{ renderer_session_id:"owner", binding, operation_id:operation } });
});
