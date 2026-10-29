import { beforeEach, expect, test, vi } from "vitest";
import { resolveWorkspaceFrozenCreate, workspaceFrozenCreates } from "./ipc";
import type { WorkspaceRegistration } from "../../../packages/protocol-types";
import type { FrozenCreateReviewChoice } from "./frozen-create-review";

const mocked = vi.hoisted(() => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: mocked.invoke }));
const binding: WorkspaceRegistration = { server_origin:"https://example.invalid", workspace_id:"workspace", workspace_epoch:"epoch", device_id:"device", device_stream_id:"stream" };
const old: WorkspaceRegistration = { ...binding, device_id:"old-device", device_stream_id:"old-stream" };
const root = "019a1000-0000-7000-8000-000000000001";
const node = "019a1000-0000-7000-8000-000000000002";
const operation = "019a1000-0000-7000-8000-000000000003";
const other = "019a1000-0000-7000-8000-000000000004";
const request = { node_id:node, parent_id:root, name:"메모", kind:"folder", upload_id:null };
const observed = { node_id:node, parent_id:root, name:"메모", kind:"folder", deleted:false, content_rev:null, content_etag:null, meta_rev:"meta", meta_etag:'"meta"', node_etag:'"node"', content_hash:null, size:null, children_generation:"0", last_change_seq:"1" };
const sameName = { ...observed, node_id:other };
const entry = { old_binding:old, operation_id:operation, operation_seq:"1", request, status:"sent", receipt:null, current_same_id:observed, current_same_name:[sameName] };
function respond(value: unknown) { mocked.invoke.mockImplementation(async (command: string) => command === "m0_renderer_session" ? "owner" : value); }
function page(item: unknown = entry, current: WorkspaceRegistration = binding) { return { binding:current, entries:[item], next_after_id:null }; }
beforeEach(() => mocked.invoke.mockReset());

test("이전 생성 페이지는 renderer와 binding을 전달하고 현재 상태와 이전 요청을 구분한다", async () => {
  respond(page());
  const result = await workspaceFrozenCreates(binding,null);
  expect(mocked.invoke).toHaveBeenCalledWith("workspace_frozen_creates", { request:{ renderer_session_id:"owner", binding, after_id:null, limit:100 } });
  expect(result.entries[0]).toMatchObject({ old_binding:old, operation_id:operation, operation_seq:"1", node_id:node, parent_id:root, name:"메모", status:"sent", current_same_id:observed, current_same_name:[sameName] });
  respond(page({ ...entry, status:"queued", current_same_id:null, current_same_name:[] }));
  expect((await workspaceFrozenCreates(binding,null)).entries[0].status).toBe("queued");
  respond(page({ ...entry, status:"abandoning", current_same_id:null, current_same_name:[] }));
  expect((await workspaceFrozenCreates(binding,null)).entries[0].status).toBe("abandoning");
});

test("이전 생성 페이지는 현재 등록과 이전 workspace를 확인한다", async () => {
  for (const result of [
    page(entry,{ ...binding, device_stream_id:"other" }),
    page({ ...entry, old_binding:{ ...old, workspace_id:"foreign" } }),
    page({ ...entry, old_binding:binding }),
  ]) { respond(result); await expect(workspaceFrozenCreates(binding,null)).rejects.toThrow(); }
});

test("같은 workspace의 이전 서버 주소를 보존하고 명시적 선택에 전달한다", async () => {
  const previous = { ...old, server_origin:"https://other.invalid" };
  respond(page({ ...entry, old_binding: previous }));
  expect((await workspaceFrozenCreates(binding,null)).entries[0].old_binding).toEqual(previous);
  respond(undefined);
  await resolveWorkspaceFrozenCreate(binding,previous,operation,{kind:"dismiss"});
  expect(mocked.invoke).toHaveBeenCalledWith("workspace_resolve_frozen_create", { request:{ renderer_session_id:"owner", binding, old_binding:previous, operation_id:operation, choice:{kind:"dismiss"} } });
});

test("손상된 생성 요청, 현재 node와 상태는 잘못된 목록으로 거절한다", async () => {
  const variants = [
    { ...entry, operation_id:"invalid" },
    { ...entry, operation_seq:"0" },
    { ...entry, request:{ ...request, node_id:"invalid" } },
    { ...entry, request:{ ...request, parent_id:node } },
    { ...entry, request:{ ...request, kind:"invalid" } },
    { ...entry, status:"committed" },
    { ...entry, current_same_id:{ ...observed, node_id:other } },
    { ...entry, current_same_id:{ ...observed, size:"5" } },
    { ...entry, current_same_name:[{ ...sameName, parent_id:other }] },
    { ...entry, current_same_name:[{ ...sameName, deleted:true }] },
    { ...entry, current_same_name:[observed] },
  ];
  for (const invalid of variants) { respond(page(invalid)); await expect(workspaceFrozenCreates(binding,null)).rejects.toThrow(); }
});

test("Rust가 판정한 Unicode casefold 일치 항목은 JS 소문자 비교로 거절하지 않는다", async () => {
  respond(page({ ...entry, request:{ ...request, name:"Straße" }, current_same_name:[{ ...sameName, name:"STRASSE" }] }));
  const result = await workspaceFrozenCreates(binding, null);
  expect(result.entries[0].current_same_name[0].name).toBe("STRASSE");
});

test("페이지 cursor와 목록 순서는 operation ID와 일치해야 한다", async () => {
  respond({ binding, entries:[entry], next_after_id:operation });
  expect((await workspaceFrozenCreates(binding,null)).next_after_id).toBe(operation);
  respond({ binding, entries:[entry], next_after_id:other }); await expect(workspaceFrozenCreates(binding,null)).rejects.toThrow();
  respond({ binding, entries:[entry,entry], next_after_id:null }); await expect(workspaceFrozenCreates(binding,null)).rejects.toThrow();
  respond(page()); await expect(workspaceFrozenCreates(binding,operation)).rejects.toThrow();
  await expect(workspaceFrozenCreates(binding,"invalid")).rejects.toThrow();
});

test("명시 결정은 전체 binding과 renderer를 전달하고 잘못된 payload를 호출 전에 거절한다", async () => {
  respond(undefined);
  await resolveWorkspaceFrozenCreate(binding,old,operation,{ kind:"apply", parent_id:root, name:"새 이름" });
  expect(mocked.invoke).toHaveBeenCalledWith("workspace_resolve_frozen_create", { request:{ renderer_session_id:"owner", binding, old_binding:old, operation_id:operation, choice:{ kind:"apply", parent_id:root, name:"새 이름" } } });
  await resolveWorkspaceFrozenCreate(binding,old,operation,{kind:"dismiss"});
  mocked.invoke.mockClear();
  const invalid: Array<[WorkspaceRegistration,string,FrozenCreateReviewChoice]> = [
    [{ ...old, workspace_id:"foreign" },operation,{kind:"dismiss"}],
    [binding,operation,{kind:"dismiss"}],
    [old,"invalid",{kind:"dismiss"}],
    [old,operation,{kind:"apply",parent_id:"invalid",name:"이름"}],
    [old,operation,{kind:"apply",parent_id:root,name:""}],
    [old,operation,{kind:"apply",parent_id:root,name:"가".repeat(86)}],
    [old,operation,{kind:"wrong"} as unknown as FrozenCreateReviewChoice],
  ];
  for (const [previous,id,choice] of invalid) await expect(resolveWorkspaceFrozenCreate(binding,previous,id,choice)).rejects.toThrow();
  expect(mocked.invoke).not.toHaveBeenCalledWith("workspace_resolve_frozen_create",expect.anything());
});
// Append to apps/client/src/frozen-create-ipc.test.ts.
// Proposed contract: failed entry includes a validated receipt with status and
// identity, and the display exposes the same terminal reason as `reason`.

test("확정 실패 receipt는 이전 등록의 epoch와 operation identity를 검증해 실패 사유를 반환한다", async () => {
  const previous = { ...old, workspace_epoch:"old-epoch" };
  for (const status of ["destination_exists", "node_id_conflict", "parent_not_folder", "node_deleted", "not_found", "not_applied", "recovery_conflict"]) {
    const receipt = { status, operation_id:operation, operation_seq:"1", workspace_epoch:previous.workspace_epoch };
    respond(page({ ...entry, old_binding:previous, status:"failed", receipt }));
    const result = await workspaceFrozenCreates(binding,null);
    expect(result.entries[0]).toMatchObject({ old_binding:previous, operation_id:operation, operation_seq:"1", status:"failed", reason:status });
  }
});

test("확정 실패의 손상·불일치 receipt는 이전 성공이나 미확정으로 표시하지 않는다", async () => {
  const previous = { ...old, workspace_epoch:"old-epoch" };
  const receipt = { status:"destination_exists", operation_id:operation, operation_seq:"1", workspace_epoch:previous.workspace_epoch };
  const invalid = [
    null,
    {},
    { ...receipt, operation_id:other },
    { ...receipt, operation_seq:"2" },
    { ...receipt, workspace_epoch:binding.workspace_epoch },
    { ...receipt, status:"created" },
    { ...receipt, status:"abandoned" },
    { ...receipt, status:"unknown" },
    { ...receipt, unexpected:true },
  ];
  for (const value of invalid) {
    respond(page({ ...entry, old_binding:previous, status:"failed", receipt:value }));
    await expect(workspaceFrozenCreates(binding,null)).rejects.toThrow();
  }
  for (const status of ["queued", "sent", "abandoning"]) {
    respond(page({ ...entry, old_binding:previous, status, receipt }));
    await expect(workspaceFrozenCreates(binding,null)).rejects.toThrow();
  }
});
