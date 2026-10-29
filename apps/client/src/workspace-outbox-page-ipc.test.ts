import { beforeEach, expect, test, vi } from "vitest";
import * as ipc from "./ipc";
import type { WorkspaceOutboxEntry, WorkspaceOutboxPage, WorkspaceRegistration } from "../../../packages/protocol-types";

const mocked = vi.hoisted(() => ({ invoke:vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke:mocked.invoke }));
const binding:WorkspaceRegistration = { server_origin:"https://example.invalid", workspace_id:"workspace", workspace_epoch:"epoch", device_id:"device", device_stream_id:"stream" };
const documentId="019a1000-0000-7000-8000-000000000002";
const id = (number:number) => `019a1000-0000-7000-8000-${number.toString(16).padStart(12,"0")}`;
const entry = (sequence:number):WorkspaceOutboxEntry => ({ operation_id:id(sequence+100), operation_seq:String(sequence), document_id:documentId, generation:"1", status:"sent", frozen:true, abandon_state:"none", failure:null, payload_retired:false });
const read = (registration:WorkspaceRegistration, cursor:string):Promise<WorkspaceOutboxPage> =>
  (ipc as unknown as { workspaceOutbox(registration:WorkspaceRegistration, cursor:string):Promise<WorkspaceOutboxPage> }).workspaceOutbox(registration,cursor);
function respond(value:unknown) { mocked.invoke.mockImplementation(async (command:string) => command === "m0_renderer_session" ? "owner" : value); }
beforeEach(() => mocked.invoke.mockReset());

test("등록과 cursor를 전달하고 한 번에 요청한 100개 페이지 하나만 읽는다", async () => {
  const entries = Array.from({length:100},(_,index) => entry(index+1));
  respond({ entries, next_after_seq:"100" });
  await expect(read(binding,"0")).resolves.toEqual({ entries, next_after_seq:"100" });
  expect(mocked.invoke).toHaveBeenCalledWith("workspace_outbox", { request:{ renderer_session_id:"owner", binding, after_seq:"0", limit:100 } });
  expect(mocked.invoke.mock.calls.filter(([command]) => command === "workspace_outbox")).toHaveLength(1);
  respond({ entries:[entry(101)], next_after_seq:null });
  await expect(read(binding,"100")).resolves.toEqual({ entries:[entry(101)], next_after_seq:null });
  expect(mocked.invoke).toHaveBeenCalledWith("workspace_outbox", { request:{ renderer_session_id:"owner", binding, after_seq:"100", limit:100 } });
});

test("빈 마지막 페이지와 frozen이 아닌 현재 등록 작업도 읽을 수 있다", async () => {
  respond({ entries:[], next_after_seq:null }); await expect(read(binding,"7")).resolves.toEqual({ entries:[], next_after_seq:null });
  const item = { ...entry(8), frozen:false, status:"queued" as const };
  respond({ entries:[item], next_after_seq:null }); await expect(read(binding,"7")).resolves.toEqual({ entries:[item], next_after_seq:null });
});

test("잘못된 등록과 정규 cursor는 native 호출 전에 거절한다", async () => {
  respond({ entries:[], next_after_seq:null });
  for (const [registration,cursor] of [
    [{ ...binding, server_origin:"" },"0"], [{ ...binding, workspace_id:"" },"0"], [{ ...binding, workspace_epoch:"" },"0"],
    [{ ...binding, device_id:"" },"0"], [{ ...binding, device_stream_id:"" },"0"],
    [binding,""], [binding,"-1"], [binding,"01"], [binding,"1.0"], [binding,"9223372036854775808"],
  ] as const) {
    mocked.invoke.mockClear(); await expect(read(registration,cursor)).rejects.toThrow();
    expect(mocked.invoke).not.toHaveBeenCalledWith("workspace_outbox",expect.anything());
  }
});

test("항목 ID, 양의 정규 seq와 generation, status 및 frozen 형식을 검사한다", async () => {
  const valid=entry(2);
  for (const wrong of [
    { ...valid, operation_id:"invalid" }, { ...valid, document_id:"invalid" },
    ...["0","01","-1","1.0","9223372036854775808",undefined].map((operation_seq) => ({ ...valid, operation_seq })),
    ...["0","01","-1","9223372036854775808",undefined].map((generation) => ({ ...valid, generation })),
    { ...valid, status:"unknown" }, { ...valid, status:null }, { ...valid, frozen:"true" }, { ...valid, frozen:undefined },
    { ...valid, abandon_state:"unknown" }, { ...valid, failure:"unknown" }, { ...valid, payload_retired:"false" },
    { ...valid, unexpected:true },
  ]) { respond({ entries:[wrong], next_after_seq:null }); await expect(read(binding,"0")).rejects.toThrow(); }
});

test("cursor 이하 항목과 역순·중복 순번·중복 operation ID를 거절한다", async () => {
  const first=entry(2), second=entry(3);
  for (const entries of [
    [entry(1)], [first,entry(1)], [second,first], [first,first], [first,{ ...second, operation_id:first.operation_id }],
  ]) { respond({ entries, next_after_seq:null }); await expect(read(binding,"1")).rejects.toThrow(); }
});

test("next cursor는 요청 cursor보다 크고 마지막 항목 순번과 일치하는 정규 값이어야 한다", async () => {
  const full=Array.from({length:100},(_,index) => entry(index+2));
  for (const next_after_seq of ["0","1","2","100","102","0101","-1","1.0","9223372036854775808",undefined]) {
    respond({ entries:full, next_after_seq }); await expect(read(binding,"1")).rejects.toThrow();
  }
  respond({ entries:[], next_after_seq:"2" }); await expect(read(binding,"1")).rejects.toThrow();
  respond({ entries:full, next_after_seq:"101" }); await expect(read(binding,"1")).resolves.toEqual({ entries:full, next_after_seq:"101" });
});

test("잘못된 페이지 형태와 100개 초과 또는 알 수 없는 필드를 거절한다", async () => {
  for (const value of [null, {}, { entries:null, next_after_seq:null }, { entries:[], next_after_seq:undefined }, { entries:[], next_after_seq:null, unexpected:true }, { entries:Array.from({length:101},(_,index) => entry(index+1)), next_after_seq:null }]) {
    respond(value); await expect(read(binding,"0")).rejects.toThrow();
  }
});
