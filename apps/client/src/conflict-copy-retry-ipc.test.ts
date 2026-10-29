import { beforeEach, expect, test, vi } from "vitest";
import * as ipc from "./ipc";
import type { WorkspaceContentConflictCopyResponse, WorkspaceRegistration } from "../../../packages/protocol-types";

const mocked = vi.hoisted(() => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: mocked.invoke }));

const binding: WorkspaceRegistration = { server_origin: "https://example.invalid", workspace_id: "workspace", workspace_epoch: "epoch", device_id: "device", device_stream_id: "stream" };
const documentId = "019a1000-0000-7000-8000-000000000002";
const operationId = "019a1000-0000-7000-8000-000000000003";
const parentId = "019a1000-0000-7000-8000-000000000004";
const oldCreateId = "019a1000-0000-7000-8000-000000000005";
const newCopyId = "019a1000-0000-7000-8000-000000000006";
const newCreateId = "019a1000-0000-7000-8000-000000000007";
const result = { status: "pending", version: "2", node_id: newCopyId, create_operation_id: newCreateId } as const;

type Retry = (binding: WorkspaceRegistration, documentId: string, sessionId: string, operationId: string, version: string, expectedCreateOperationId: string, parentId: string, name: string) => Promise<WorkspaceContentConflictCopyResponse>;
const retry: Retry = (...args) => (ipc as unknown as { retryWorkspaceContentConflictCopy: Retry }).retryWorkspaceContentConflictCopy(...args);

function respond(value: unknown) {
  mocked.invoke.mockImplementation(async (command: string) => command === "m0_renderer_session" ? "owner" : value);
}

beforeEach(() => mocked.invoke.mockReset());

test("사본 재승인은 이전 create 작업과 새 대상, 기존 선택 version을 전달한다", async () => {
  respond(result);
  await expect(retry(binding, documentId, "session", operationId, "2", oldCreateId, parentId, "수정한 사본.md")).resolves.toEqual(result);
  expect(mocked.invoke).toHaveBeenCalledWith("workspace_retry_content_conflict_copy", { request: {
    progress: { renderer_session_id: "owner", binding, document_id: documentId, edit_session_id: "session", operation_id: operationId, expected_intent_version: "2" },
    expected_create_operation_id: oldCreateId, parent_id: parentId, name: "수정한 사본.md",
  } });
});

test("동일 재승인 요청은 같은 새 식별자를 수용한다", async () => {
  respond(result);
  expect(await retry(binding, documentId, "session", operationId, "2", oldCreateId, parentId, "copy.MD")).toEqual(result);
  expect(await retry(binding, documentId, "session", operationId, "2", oldCreateId, parentId, "copy.MD")).toEqual(result);
});

test("잘못된 등록·문서·session·원 작업·version 또는 이전 create ID는 호출 전에 거절한다", async () => {
  respond(result);
  const invalid: [WorkspaceRegistration, string, string, string, string, string, string, string][] = [
    [{ ...binding, device_id: null } as unknown as WorkspaceRegistration, documentId, "session", operationId, "2", oldCreateId, parentId, "copy.md"],
    [binding, "invalid", "session", operationId, "2", oldCreateId, parentId, "copy.md"],
    [binding, documentId, "", operationId, "2", oldCreateId, parentId, "copy.md"],
    [binding, documentId, "session", "invalid", "2", oldCreateId, parentId, "copy.md"],
    [binding, documentId, "session", operationId, "01", oldCreateId, parentId, "copy.md"],
    [binding, documentId, "session", operationId, "9223372036854775808", oldCreateId, parentId, "copy.md"],
    [binding, documentId, "session", operationId, "2", "invalid", parentId, "copy.md"],
    [binding, documentId, "session", operationId, "2", operationId, parentId, "copy.md"],
  ];
  for (const [scope, doc, session, operation, version, oldCreate, parent, name] of invalid) {
    mocked.invoke.mockClear();
    await expect(retry(scope, doc, session, operation, version, oldCreate, parent, name)).rejects.toThrow();
    expect(mocked.invoke).not.toHaveBeenCalledWith("workspace_retry_content_conflict_copy", expect.anything());
  }
});

test("잘못된 부모와 이름은 native 호출 전에 거절한다", async () => {
  respond(result);
  for (const [parent, name] of [
    ["invalid", "copy.md"], [parentId, ""], [parentId, "copy.txt"], [parentId, "../copy.md"],
    [parentId, "bad\\copy.md"], [parentId, "bad\ncopy.md"], [parentId, "a".repeat(253) + ".md"],
  ]) {
    mocked.invoke.mockClear();
    await expect(retry(binding, documentId, "session", operationId, "2", oldCreateId, parent, name)).rejects.toThrow();
    expect(mocked.invoke).not.toHaveBeenCalledWith("workspace_retry_content_conflict_copy", expect.anything());
  }
});

test("재승인 응답의 상태·version과 새 node/create ID가 다르면 거절한다", async () => {
  for (const value of [
    null, {}, { ...result, status: "completed" }, { ...result, version: "3" }, { ...result, version: "01" },
    { ...result, node_id: "invalid" }, { ...result, create_operation_id: "invalid" },
    { ...result, node_id: documentId }, { ...result, node_id: newCreateId },
    { ...result, create_operation_id: oldCreateId }, { ...result, create_operation_id: operationId },
    { ...result, unexpected: true },
  ]) {
    respond(value);
    await expect(retry(binding, documentId, "session", operationId, "2", oldCreateId, parentId, "copy.md")).rejects.toThrow();
  }
});
