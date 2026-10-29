import { beforeEach, expect, test, vi } from "vitest";
import * as ipc from "./ipc";
import type { WorkspaceFrozenContentInspection, WorkspaceRegistration } from "../../../packages/protocol-types";

const mocked = vi.hoisted(() => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: mocked.invoke }));

const current: WorkspaceRegistration = { server_origin:"https://example.invalid", workspace_id:"workspace", workspace_epoch:"current-epoch", device_id:"device", device_stream_id:"stream" };
const old: WorkspaceRegistration = { ...current, workspace_epoch:"old-epoch", device_id:"old-device", device_stream_id:"old-stream" };
const documentId = "019a1000-0000-7000-8000-000000000002", operationId = "019a1000-0000-7000-8000-000000000003", otherId = "019a1000-0000-7000-8000-000000000004";
const result: WorkspaceFrozenContentInspection = {
  current_binding:current, old_binding:old, document_id:documentId, operation_id:operationId, operation_seq:"1", operation_status:"sent", abandon_state:"none", operation_generation:"1", local_generation:"2", expected_revision:"draft-base-revision", outcome_unknown:true,
  base_revision:"draft-base-revision", base_bytes_base64:"YmFzZQo=", base_hash:"sha256:f34848ca92665c342abd5816c9e3eda0e82180671195362bcd0080544a3bc2ac",
  operation_bytes_base64:"YmFzZQo=", operation_hash:"sha256:f34848ca92665c342abd5816c9e3eda0e82180671195362bcd0080544a3bc2ac",
  local_bytes_base64:"bG9jYWwK", local_hash:"sha256:efb83f2a277e9f49b38efd505f5cbb93885e721b6bd16b788937c9396174c006",
  remote_revision:"remote-revision", remote_bytes_base64:"Y3VycmVudAo=", remote_hash:"sha256:48aa6cae8c70abdb28631d22b316e6d9f9d0768ec2911de7090e248b2afe6ca1",
};
type Inspection = { current_binding:WorkspaceRegistration; old_binding:WorkspaceRegistration; document_id:string; operation_id:string; operation_seq:string; operation_status:"sent" | "queued"; operation_generation:string; local_generation:string; expected_revision:string | null; outcome_unknown:boolean; base_revision:string; remote_revision:string; operation_bytes:Uint8Array; base_bytes:Uint8Array; local_bytes:Uint8Array; remote_bytes:Uint8Array };
const inspect = (active:WorkspaceRegistration, previous:WorkspaceRegistration, operation:string, document:string): Promise<Inspection> =>
  (ipc as unknown as { workspaceInspectFrozenContent(active:WorkspaceRegistration, previous:WorkspaceRegistration, operation:string, document:string):Promise<Inspection> }).workspaceInspectFrozenContent(active, previous, operation, document);
function respond(value:unknown) { mocked.invoke.mockImplementation(async (command:string) => command === "m0_renderer_session" ? "owner" : value); }
beforeEach(() => mocked.invoke.mockReset());

test("선택한 등록과 작업을 요청하고 문서·순번·별도 세대·revision 및 네 원문을 검증한다", async () => {
  respond(result);
  const shown = await inspect(current, old, operationId, documentId);
  expect(mocked.invoke).toHaveBeenCalledWith("workspace_inspect_frozen_content", { request:{ renderer_session_id:"owner", current_binding:current, old_binding:old, operation_id:operationId } });
  expect(shown).toMatchObject({ current_binding:current, old_binding:old, document_id:documentId, operation_id:operationId, operation_seq:"1", operation_status:"sent", abandon_state:"none", operation_generation:"1", local_generation:"2", expected_revision:"draft-base-revision", base_revision:"draft-base-revision", remote_revision:"remote-revision", outcome_unknown:true });
  expect(new TextDecoder().decode(shown.operation_bytes)).toBe("base\n");
  expect(new TextDecoder().decode(shown.base_bytes)).toBe("base\n");
  expect(new TextDecoder().decode(shown.local_bytes)).toBe("local\n");
  expect(new TextDecoder().decode(shown.remote_bytes)).toBe("current\n");
  respond({ ...result, operation_status:"queued", expected_revision:null, outcome_unknown:false });
  expect(await inspect(current, old, operationId, documentId)).toMatchObject({ operation_status:"queued", expected_revision:null, outcome_unknown:false });
  respond({ ...result, operation_status:"queued", expected_revision:"queued-expected-revision", outcome_unknown:false });
  expect(await inspect(current, old, operationId, documentId)).toMatchObject({ operation_status:"queued", expected_revision:"queued-expected-revision", base_revision:"draft-base-revision" });
});

test("잘못된 등록·문서·작업 입력은 native 호출 전에 거절한다", async () => {
  respond(result);
  for (const [active, previous, operation, document] of [
    [current, current, operationId, documentId], [current, { ...old, workspace_id:"another" }, operationId, documentId],
    [{ ...current, device_stream_id:"" }, old, operationId, documentId], [current, { ...old, device_id:"" }, operationId, documentId],
    [current, old, "invalid", documentId], [current, old, operationId, "invalid"],
  ] as const) {
    mocked.invoke.mockClear(); await expect(inspect(active, previous, operation, document)).rejects.toThrow();
    expect(mocked.invoke).not.toHaveBeenCalledWith("workspace_inspect_frozen_content", expect.anything());
  }
});

test("다른 등록·문서·작업을 가리키거나 모르는 필드를 포함하는 응답을 거절한다", async () => {
  for (const bad of [
    null, {}, { ...result, current_binding:{ ...current, device_stream_id:"replacement" } },
    { ...result, old_binding:{ ...old, workspace_epoch:"other" } }, { ...result, current_binding:undefined }, { ...result, old_binding:undefined },
    { ...result, document_id:otherId }, { ...result, document_id:"invalid" }, { ...result, operation_id:otherId },
    { ...result, operation_id:"invalid" }, { ...result, unexpected:true },
  ]) { respond(bad); await expect(inspect(current, old, operationId, documentId)).rejects.toThrow(); }
});

test("작업 상태와 불명 여부, 순번, 양쪽 generation 및 revision을 검증한다", async () => {
  for (const bad of [
    ...["0", "-1", "01", "1.0", "9223372036854775808", undefined].map((operation_seq) => ({ ...result, operation_seq })),
    ...["0", "-1", "01", "9223372036854775808", undefined].map((operation_generation) => ({ ...result, operation_generation })),
    ...["0", "-1", "01", "9223372036854775808", undefined].map((local_generation) => ({ ...result, local_generation })),
    { ...result, operation_generation:"2", local_generation:"1" },
    { ...result, operation_status:"sent", expected_revision:"different-revision" },
    { ...result, operation_status:"sent", expected_revision:null },
    { ...result, operation_generation:"2", local_generation:"2" },
    { ...result, operation_status:"queued", outcome_unknown:false, operation_generation:"2", local_generation:"2" },
    { ...result, operation_status:"committed" }, { ...result, operation_status:"failed" }, { ...result, operation_status:"unknown" },
    { ...result, operation_status:"sent", outcome_unknown:false }, { ...result, operation_status:"queued", outcome_unknown:true }, { ...result, outcome_unknown:null },
    { ...result, expected_revision:"" }, { ...result, expected_revision:undefined }, { ...result, expected_revision:"bad\nrevision" },
    { ...result, base_revision:"" }, { ...result, base_revision:undefined }, { ...result, base_revision:"bad\nrevision" },
    { ...result, remote_revision:"" }, { ...result, remote_revision:undefined }, { ...result, remote_revision:"bad\nrevision" },
  ]) { respond(bad); await expect(inspect(current, old, operationId, documentId)).rejects.toThrow(); }
});

const fields = [
  ["operation_bytes_base64", "operation_hash", "operation_bytes"], ["base_bytes_base64", "base_hash", "base_bytes"],
  ["local_bytes_base64", "local_hash", "local_bytes"], ["remote_bytes_base64", "remote_hash", "remote_bytes"],
] as const;

test.each(fields)("%s 원문의 base64 및 %s hash 형식과 실제 bytes 일치를 검증한다", async (encodedField, hashField) => {
  for (const invalid of [undefined, null, 42, "%%%", "a", "Zm9v=", "AA=A"]) {
    respond({ ...result, [encodedField]:invalid }); await expect(inspect(current, old, operationId, documentId)).rejects.toThrow();
  }
  for (const invalid of [undefined, null, "", "sha256:" + "a".repeat(63), "sha256:" + "A".repeat(64), "sha256:" + "g".repeat(64), "sha256:" + "0".repeat(64)]) {
    respond({ ...result, [hashField]:invalid }); await expect(inspect(current, old, operationId, documentId)).rejects.toThrow();
  }
  respond({ ...result, [encodedField]:"/w==", [hashField]:"sha256:a8100ae6aa1940d0b663bb31cd466142ebbdbd5187131b92d93818987832eb89" });
  const shown = await inspect(current, old, operationId, documentId);
  expect(shown).toBeDefined();
});

test("동일 세대의 payload와 local 원문이 같으면 조회 결과를 허용한다", async () => {
  respond({ ...result, operation_generation:"2", local_generation:"2", operation_bytes_base64:result.local_bytes_base64, operation_hash:result.local_hash });
  const shown = await inspect(current, old, operationId, documentId);
  expect(shown.operation_bytes).toEqual(shown.local_bytes);
});

test.each(fields)("%s 원문은 저장 경계까지 허용하고 1 byte 초과를 거절한다", async (encodedField, hashField, decodedField) => {
  const remote = decodedField === "remote_bytes";
  const maximum = (remote ? 25 : 5) * 1024 * 1024;
  const atLimit = btoa("a".repeat(maximum));
  const atHash = remote ? "e24e1deb1466614496ddfc6af6316e5c0432849cce7205d46e2d18230e2a83f3" : "a29968fad2e782aa9f2040a35f05adb97ed8979eb1f572c8c8ea78637e275f3c";
  const overHash = remote ? "333c085aa86d99197c17caa2a911eff4d8abe8632034389be18974a0a8436038" : "0e353bd000756ce37c9349f47d51efd8f250556700fe35ac213a7ab62db44dbb";
  respond({ ...result, [encodedField]:atLimit, [hashField]:`sha256:${atHash}` });
  const shown = await inspect(current, old, operationId, documentId);
  expect(shown[decodedField].length).toBe(maximum);
  expect(shown[decodedField][0]).toBe(97); expect(shown[decodedField][maximum - 1]).toBe(97);
  respond({ ...result, [encodedField]:btoa("a".repeat(maximum + 1)), [hashField]:`sha256:${overHash}` });
  await expect(inspect(current, old, operationId, documentId)).rejects.toThrow();
}, 30000);
