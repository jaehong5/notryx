import { beforeEach, expect, test, vi } from "vitest";
import * as ipc from "./ipc";
import type { WorkspaceFrozenContentInspection, WorkspaceRegistration } from "../../../packages/protocol-types";

const mocked = vi.hoisted(() => ({ invoke:vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke:mocked.invoke }));
const current:WorkspaceRegistration = { server_origin:"https://example.invalid", workspace_id:"workspace", workspace_epoch:"current-epoch", device_id:"device", device_stream_id:"stream" };
const old:WorkspaceRegistration = { ...current, workspace_epoch:"old-epoch", device_id:"old-device", device_stream_id:"old-stream" };
const documentId="019a1000-0000-7000-8000-000000000002", operationId="019a1000-0000-7000-8000-000000000003", otherId="019a1000-0000-7000-8000-000000000004";
const response:WorkspaceFrozenContentInspection = {
  current_binding:current, old_binding:old, document_id:documentId, operation_id:operationId, operation_seq:"1", operation_status:"abandoning", abandon_state:"requested", operation_generation:"1", local_generation:"2", expected_revision:null, outcome_unknown:false,
  base_revision:"base-revision", base_bytes_base64:"YmFzZQo=", base_hash:"sha256:f34848ca92665c342abd5816c9e3eda0e82180671195362bcd0080544a3bc2ac",
  operation_bytes_base64:"YmFzZQo=", operation_hash:"sha256:f34848ca92665c342abd5816c9e3eda0e82180671195362bcd0080544a3bc2ac",
  local_bytes_base64:"bG9jYWwK", local_hash:"sha256:efb83f2a277e9f49b38efd505f5cbb93885e721b6bd16b788937c9396174c006",
  remote_revision:"remote-revision", remote_bytes_base64:"Y3VycmVudAo=", remote_hash:"sha256:48aa6cae8c70abdb28631d22b316e6d9f9d0768ec2911de7090e248b2afe6ca1",
};
type Result = { current_binding:WorkspaceRegistration; old_binding:WorkspaceRegistration; document_id:string; operation_id:string; operation_status:"queued"|"sent"|"abandoning"; abandon_state:"none"|"requested"|"sent"; outcome_unknown:boolean; expected_revision:string|null; operation_bytes:Uint8Array; base_bytes:Uint8Array; local_bytes:Uint8Array; remote_bytes:Uint8Array };
const inspect = ():Promise<Result> => (ipc as unknown as { workspaceInspectFrozenContent(current:WorkspaceRegistration, old:WorkspaceRegistration, operation:string, document:string):Promise<Result> }).workspaceInspectFrozenContent(current,old,operationId,documentId);
function respond(value:unknown) { mocked.invoke.mockImplementation(async (command:string) => command === "m0_renderer_session" ? "owner" : value); }
beforeEach(() => mocked.invoke.mockReset());

test.each(["requested","sent"] as const)("abandoning/%s은 취소 단계와 이전 원문을 보존하고 기존 전송의 결과 불명으로 바꾸지 않는다", async (state) => {
  respond({ ...response, abandon_state:state });
  const value=await inspect();
  expect(value).toMatchObject({ current_binding:current, old_binding:old, document_id:documentId, operation_id:operationId, operation_status:"abandoning", abandon_state:state, outcome_unknown:false, expected_revision:null });
  expect(new TextDecoder().decode(value.operation_bytes)).toBe("base\n");
  expect(new TextDecoder().decode(value.base_bytes)).toBe("base\n");
  expect(new TextDecoder().decode(value.local_bytes)).toBe("local\n");
  expect(new TextDecoder().decode(value.remote_bytes)).toBe("current\n");
  expect(mocked.invoke).toHaveBeenCalledWith("workspace_inspect_frozen_content",{ request:{ renderer_session_id:"owner", current_binding:current, old_binding:old, operation_id:operationId } });
});

test("queued와 sent는 abandoning과 구분하고 원래 전송의 결과 불명은 sent일 때만 표시한다", async () => {
  respond({ ...response, operation_status:"queued", abandon_state:"none", outcome_unknown:false });
  expect(await inspect()).toMatchObject({ operation_status:"queued", abandon_state:"none", outcome_unknown:false });
  respond({ ...response, operation_status:"sent", abandon_state:"none", expected_revision:"base-revision", outcome_unknown:true });
  expect(await inspect()).toMatchObject({ operation_status:"sent", abandon_state:"none", outcome_unknown:true });
  respond({ ...response, operation_status:"abandoning", abandon_state:"sent", expected_revision:"different-revision", outcome_unknown:false });
  expect(await inspect()).toMatchObject({ operation_status:"abandoning", abandon_state:"sent", expected_revision:"different-revision", outcome_unknown:false });
});

test("status와 abandon_state가 부조화이거나 결과 불명을 위조한 응답은 거절한다", async () => {
  for (const invalid of [
    { ...response, abandon_state:"none" }, { ...response, abandon_state:null }, { ...response, abandon_state:undefined }, { ...response, abandon_state:"unknown" },
    { ...response, outcome_unknown:true }, { ...response, outcome_unknown:null },
    { ...response, operation_status:"queued", abandon_state:"requested" }, { ...response, operation_status:"queued", abandon_state:"sent" },
    { ...response, operation_status:"sent", abandon_state:"requested", expected_revision:"base-revision", outcome_unknown:true },
    { ...response, operation_status:"sent", abandon_state:"sent", expected_revision:"base-revision", outcome_unknown:true },
    { ...response, operation_status:"queued", abandon_state:"none", outcome_unknown:true },
    { ...response, operation_status:"sent", abandon_state:"none", expected_revision:"base-revision", outcome_unknown:false },
    { ...response, operation_status:"abandoned", abandon_state:"sent" }, { ...response, operation_status:"committed", abandon_state:"none" },
  ]) { respond(invalid); await expect(inspect()).rejects.toThrow(); }
});

test("abandoning 응답의 등록·문서·작업·원문을 위조하면 거절한다", async () => {
  for (const invalid of [
    { ...response, old_binding:{ ...old, workspace_epoch:"another" } }, { ...response, current_binding:{ ...current, device_id:"another" } },
    { ...response, document_id:otherId }, { ...response, operation_id:otherId }, { ...response, operation_seq:"0" },
    { ...response, local_generation:"0" }, { ...response, operation_bytes_base64:"%%%" }, { ...response, operation_hash:"sha256:" + "0".repeat(64) },
    { ...response, extra:true },
  ]) { respond(invalid); await expect(inspect()).rejects.toThrow(); }
});
