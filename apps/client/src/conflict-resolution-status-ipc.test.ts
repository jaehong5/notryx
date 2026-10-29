import { beforeEach, expect, test, vi } from "vitest";
import * as ipc from "./ipc";
import type { WorkspaceConflictResolutionResponse, WorkspaceRegistration } from "../../../packages/protocol-types";

const mocked = vi.hoisted(() => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: mocked.invoke }));

const binding: WorkspaceRegistration = { server_origin: "https://example.invalid", workspace_id: "workspace", workspace_epoch: "epoch", device_id: "device", device_stream_id: "stream" };
const documentId = "019a1000-0000-7000-8000-000000000002";
const operationId = "019a1000-0000-7000-8000-000000000003";
const otherId = "019a1000-0000-7000-8000-000000000004";
const pending = { status: "pending", version: "0", generation: "2", choice: "choose_remote", needs_review: false, frozen: false } as const;
const response = { binding, document_id: documentId, operation_id: operationId, state: pending };

function respond(value: unknown) {
  mocked.invoke.mockImplementation(async (command: string) => command === "m0_renderer_session" ? "owner" : value);
}

const status = (bindingValue: WorkspaceRegistration, doc: string, operation: string): Promise<WorkspaceConflictResolutionResponse> =>
  (ipc as unknown as { workspaceContentConflictResolutionStatus(binding: WorkspaceRegistration, documentId: string, operationId: string): Promise<WorkspaceConflictResolutionResponse> })
    .workspaceContentConflictResolutionStatus(bindingValue, doc, operation);

beforeEach(() => mocked.invoke.mockReset());

test("충돌 해결 상태 조회는 renderer와 정확한 등록·문서·작업을 전달한다", async () => {
  respond(response);
  await expect(status(binding, documentId, operationId)).resolves.toEqual(response);
  expect(mocked.invoke).toHaveBeenCalledWith("workspace_content_conflict_resolution_status", { request: { renderer_session_id: "owner", binding, document_id: documentId, operation_id: operationId } });
});

test("조회는 기록 없음, 세 선택의 보류 및 완료를 구분하고 0 version을 유지한다", async () => {
  const states = [
    { status: "not_found" },
    ...["keep_local_copy", "choose_remote", "save_merged"].map((choice) => ({ ...pending, choice, needs_review: true, frozen: true })),
    { status: "completed", generation: "3", frozen: false },
    { status: "completed", generation: "3", frozen: true },
  ];
  for (const state of states) {
    respond({ ...response, state });
    expect((await status(binding, documentId, operationId)).state).toEqual(state);
  }
});

test("응답의 등록 구성 요소, 문서 또는 작업 ID가 요청과 다르면 거절한다", async () => {
  for (const wrong of [
    ...Object.keys(binding).map((field) => ({ ...response, binding: { ...binding, [field]: "other" } })),
    { ...response, binding: null },
    { ...response, document_id: otherId }, { ...response, document_id: "invalid" }, { ...response, document_id: undefined },
    { ...response, operation_id: otherId }, { ...response, operation_id: "invalid" }, { ...response, operation_id: undefined },
  ]) {
    respond(wrong);
    await expect(status(binding, documentId, operationId)).rejects.toThrow();
  }
});

test("상태 종류 및 필수 choice·boolean을 확인하고 다른 variant의 필드나 누락을 거절한다", async () => {
  for (const state of [
    null, [], "pending", {}, { status: "unknown" }, { status: "pending" },
    { ...pending, choice: "unknown" }, { ...pending, choice: undefined }, { ...pending, needs_review: "false" },
    { ...pending, needs_review: undefined }, { ...pending, frozen: 0 }, { ...pending, frozen: undefined },
    { status: "completed" }, { status: "completed", generation: "2", frozen: "false" },
    { status: "not_found", generation: "2" },
  ]) {
    respond({ ...response, state });
    await expect(status(binding, documentId, operationId)).rejects.toThrow();
  }
});

test("version과 generation은 정규 decimal 문자열이며 SQLite 범위를 넘지 않아야 한다", async () => {
  const invalid = [undefined, null, 1, "", "-1", "+1", "1.1", "01", "9223372036854775808"];
  for (const value of invalid) {
    for (const state of [{ ...pending, version: value }, { ...pending, generation: value }, { status: "completed", generation: value, frozen: false }]) {
      respond({ ...response, state });
      await expect(status(binding, documentId, operationId)).rejects.toThrow();
    }
  }
});

test("잘못된 문서나 작업 ID는 native 조회 전에 거절한다", async () => {
  respond(response);
  for (const [doc, operation] of [["invalid", operationId], [documentId, "invalid"], ["", operationId], [documentId, ""]]) {
    mocked.invoke.mockClear();
    await expect(status(binding, doc, operation)).rejects.toThrow();
    expect(mocked.invoke).not.toHaveBeenCalledWith("workspace_content_conflict_resolution_status", expect.anything());
  }
});
