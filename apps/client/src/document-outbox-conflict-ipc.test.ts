import { beforeEach, expect, test, vi } from "vitest";
import { workspaceDocumentOutbox } from "./ipc";
import type { WorkspaceRegistration } from "../../../packages/protocol-types";

const mocked = vi.hoisted(() => ({ invoke:vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke:mocked.invoke }));

const binding:WorkspaceRegistration = { server_origin:"https://example.invalid", workspace_id:"workspace", workspace_epoch:"epoch", device_id:"device", device_stream_id:"stream" };
const documentId = "019a1000-0000-7000-8000-000000000002";
const conflictId = "019a1000-0000-7000-8000-000000000003";
const followupId = "019a1000-0000-7000-8000-000000000004";
const first = { operation_id:conflictId, operation_seq:"1" };
const latest = { operation_id:followupId, operation_seq:"2", document_id:documentId, generation:"2", status:"queued", frozen:false, abandon_state:"none", failure:null, payload_retired:false };
const response = { binding, document_id:documentId, latest, first_content_conflict:first, has_conflict:true, has_failure:false, confirmed_through:"1" };

function respond(value:unknown) {
  mocked.invoke.mockImplementation(async (command:string) => command === "m0_renderer_session" ? "owner" : value);
}
beforeEach(() => mocked.invoke.mockReset());

test("문서별 전송 상태는 가장 이른 본문 충돌의 operation ID와 순번을 유지한다", async () => {
  respond(response);
  await expect(workspaceDocumentOutbox(binding, documentId)).resolves.toEqual(response);
  expect(mocked.invoke).toHaveBeenCalledWith("workspace_document_outbox", { request:{ renderer_session_id:"owner", binding, document_id:documentId } });
});

test("본문 충돌이 없으면 명시적 null을 유지한다", async () => {
  const clean = { ...response, latest:null, first_content_conflict:null, has_conflict:false, confirmed_through:"0" };
  respond(clean);
  await expect(workspaceDocumentOutbox(binding, documentId)).resolves.toEqual(clean);
});

test("충돌 참조를 누락하거나 잘못된 operation ID 또는 양의 정규 순번이 아니면 거절한다", async () => {
  for (const invalid of [
    undefined, {}, [], "conflict", 1, { operation_id:conflictId }, { operation_seq:"1" },
    { operation_id:"", operation_seq:"1" }, { operation_id:"invalid", operation_seq:"1" },
    { operation_id:conflictId.toUpperCase(), operation_seq:"1" },
    { operation_id:conflictId, operation_seq:null }, { operation_id:conflictId, operation_seq:1 },
    { operation_id:conflictId, operation_seq:"0" }, { operation_id:conflictId, operation_seq:"01" },
    { operation_id:conflictId, operation_seq:"-1" }, { operation_id:conflictId, operation_seq:"1.0" },
    { operation_id:conflictId, operation_seq:"9223372036854775808" },
    { operation_id:conflictId, operation_seq:"1", unexpected:true },
  ]) {
    const malformed = { ...response, first_content_conflict:invalid };
    respond(malformed);
    await expect(workspaceDocumentOutbox(binding, documentId)).rejects.toThrow();
  }
  const missing:Partial<typeof response> = { ...response };
  delete missing.first_content_conflict;
  respond(missing);
  await expect(workspaceDocumentOutbox(binding, documentId)).rejects.toThrow();
});

test("다른 등록이나 문서의 충돌 참조는 반환하지 않는다", async () => {
  for (const invalid of [
    ...Object.keys(binding).map((field) => ({ ...response, binding:{ ...binding, [field]:"other" } })),
    { ...response, binding:null }, { ...response, document_id:followupId }, { ...response, document_id:"invalid" },
    { ...response, unexpected:true },
  ]) {
    respond(invalid);
    await expect(workspaceDocumentOutbox(binding, documentId)).rejects.toThrow();
  }
});
