import { beforeEach, expect, test, vi } from "vitest";
import * as ipc from "./ipc";
import type { WorkspaceRegistration } from "../../../packages/protocol-types";

const mocked = vi.hoisted(() => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: mocked.invoke }));
const binding: WorkspaceRegistration = { server_origin:"https://example.invalid", workspace_id:"workspace", workspace_epoch:"epoch", device_id:"device", device_stream_id:"stream" };
const old = { ...binding, device_id:"old-device", device_stream_id:"old-stream" };
const nodeId = "019a1000-0000-7000-8000-000000000002", operation = "019a1000-0000-7000-8000-000000000003", parent = "019a1000-0000-7000-8000-000000000004", destination = "019a1000-0000-7000-8000-000000000005", other = "019a1000-0000-7000-8000-000000000006";
const request = { expected_parent_id:parent, parent_id:destination, name:"Moved.md" };
const node = { node_id:nodeId, parent_id:parent, name:"Source.md", kind:"file", deleted:false, content_rev:"content", content_etag:'"content"', meta_rev:"current", meta_etag:'"current"', node_etag:'"node"', content_hash:"sha256:" + "a".repeat(64), size:"1", children_generation:"0", last_change_seq:"2" };
const folder = { node_id:destination, parent_id:parent, name:"Destination", kind:"folder", deleted:false, content_rev:null, content_etag:null, meta_rev:"folder", meta_etag:'"folder"', node_etag:'"folder-node"', content_hash:null, size:null, children_generation:"0", last_change_seq:"2" };
const item = { old_binding:old, operation_id:operation, operation_seq:"1", node_id:nodeId, expected_meta_etag:'"old"', request, status:"sent", receipt:null, current_node:node, current_destination:folder };
const page = (entry:unknown, current = binding) => ({ binding:current, entries:[entry], next_after_id:null });
function respond(value:unknown) { mocked.invoke.mockImplementation(async (command:string) => command === "m0_renderer_session" ? "owner" : value); }
beforeEach(() => mocked.invoke.mockReset());

test("동결 이동 목록은 이전 요청과 현재 등록·문서·목적지를 검증한다", async () => {
  respond(page(item));
  expect((await ipc.workspaceFrozenMoves(binding, null)).entries[0]).toMatchObject({ old_binding:old, operation_id:operation, node_id:nodeId, request, current_node:{node_id:nodeId}, current_destination:{node_id:destination} });
  expect(mocked.invoke).toHaveBeenCalledWith("workspace_frozen_moves", { request:{ renderer_session_id:"owner", binding, after_id:null, limit:100 } });
  for (const bad of [
    {...item, old_binding:binding}, {...item, old_binding:{...old, workspace_id:"other"}},
    {...item, current_node:{...node, node_id:other}}, {...item, current_destination:{...folder, node_id:other}},
    {...item, request:{...request, parent_id:nodeId}}, {...item, expected_meta_etag:'W/"old"'},
    {...item, status:"committed"}, {...item, receipt:{}},
  ]) { respond(page(bad)); await expect(ipc.workspaceFrozenMoves(binding, null)).rejects.toThrow(); }
  respond(page(item, {...binding, device_stream_id:"other"}));
  await expect(ipc.workspaceFrozenMoves(binding, null)).rejects.toThrow();
});

test("동결 이동의 412는 이전 binding과 작업 identity 및 구조 revision을 확인한다", async () => {
  const receipt = { status:"metadata_precondition_failed", operation_id:operation, operation_seq:"1", workspace_epoch:old.workspace_epoch, current_meta_rev:"new", current_meta_etag:'"new"' };
  respond(page({...item, status:"failed", receipt}));
  expect((await ipc.workspaceFrozenMoves(binding, null)).entries[0].reason).toBe("metadata_precondition_failed");
  for (const bad of [ {...receipt, operation_id:other}, {...receipt, operation_seq:"2"}, {...receipt, workspace_epoch:"other"}, {...receipt, current_meta_rev:"old", current_meta_etag:'"old"'}, {...receipt, current_meta_rev:"mismatch"}, {...receipt, status:"moved"} ]) {
    respond(page({...item, status:"failed", receipt:bad}));
    await expect(ipc.workspaceFrozenMoves(binding, null)).rejects.toThrow();
  }
});

test("새 이동 결정은 현재 확인한 metadata와 부모 및 목적지를 그대로 전달한다", async () => {
  respond(undefined);
  const choice = { kind:"apply" as const, expected_meta_etag:'"current"', request };
  await ipc.resolveWorkspaceFrozenMove(binding, old, operation, choice);
  expect(mocked.invoke).toHaveBeenCalledWith("workspace_resolve_frozen_move", { request:{ renderer_session_id:"owner", binding, old_binding:old, operation_id:operation, choice } });
  for (const invalid of [ {...choice, expected_meta_etag:'W/"bad"'}, {...choice, request:{...request, parent_id:"invalid"}} ]) {
    mocked.invoke.mockClear();
    await expect(ipc.resolveWorkspaceFrozenMove(binding, old, operation, invalid)).rejects.toThrow();
    expect(mocked.invoke).not.toHaveBeenCalledWith("workspace_resolve_frozen_move", expect.anything());
  }
  await expect(ipc.resolveWorkspaceFrozenMove(binding, binding, operation, {kind:"dismiss"})).rejects.toThrow();
  await expect(ipc.resolveWorkspaceFrozenMove(binding, {...old, workspace_id:"other"}, operation, {kind:"dismiss"})).rejects.toThrow();
});
