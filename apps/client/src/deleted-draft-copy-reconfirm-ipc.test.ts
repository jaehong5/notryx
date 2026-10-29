import { beforeEach, expect, test, vi } from "vitest";
import * as ipc from "./ipc";
import type { WorkspaceDeletedDraftCopyConfirmResult, WorkspaceRegistration } from "../../../packages/protocol-types";

const mocked = vi.hoisted(() => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: mocked.invoke }));

const binding: WorkspaceRegistration = { server_origin: "https://example.invalid", workspace_id: "workspace", workspace_epoch: "epoch", device_id: "device", device_stream_id: "stream" };
const documentId = "019a1000-0000-7000-8000-000000000002";
const oldCreateId = "019a1000-0000-7000-8000-000000000003";
const parentId = "019a1000-0000-7000-8000-000000000004";
const newCopyId = "019a1000-0000-7000-8000-000000000005";
const newCreateId = "019a1000-0000-7000-8000-000000000006";
const result = { status: "pending", node_id: newCopyId, create_operation_id: newCreateId } as const;

type Reconfirm = (binding: WorkspaceRegistration, documentId: string, expectedCreateOperationId: string, parentId: string, name: string) => Promise<WorkspaceDeletedDraftCopyConfirmResult>;
const reconfirm: Reconfirm = (...args) => (ipc as unknown as { reconfirmWorkspaceDeletedDraftCopy: Reconfirm }).reconfirmWorkspaceDeletedDraftCopy(...args);

function respond(value: unknown) {
  mocked.invoke.mockImplementation(async (command: string) => command === "m0_renderer_session" ? "owner" : value);
}

beforeEach(() => mocked.invoke.mockReset());

test("삭제 사본 재승인은 이전 create ID와 사용자가 고른 새 대상을 전달한다", async () => {
  respond(result);
  await expect(reconfirm(binding, documentId, oldCreateId, parentId, "다시 저장.md")).resolves.toEqual(result);
  expect(mocked.invoke).toHaveBeenCalledWith("workspace_reconfirm_deleted_draft_copy", { request: {
    renderer_session_id: "owner", binding, document_id: documentId, expected_create_operation_id: oldCreateId, parent_id: parentId, name: "다시 저장.md",
  } });
});

test("같은 명시적 재승인 요청은 이전과 같은 새 node 및 create ID를 보존한다", async () => {
  respond(result);
  expect(await reconfirm(binding, documentId, oldCreateId, parentId, "Copy.MD")).toEqual(result);
  expect(await reconfirm(binding, documentId, oldCreateId, parentId, "Copy.MD")).toEqual(result);
});

test("잘못된 등록·문서·이전 create ID 또는 새 대상은 native 호출 전에 거절한다", async () => {
  respond(result);
  const invalid: [WorkspaceRegistration, string, string, string, string][] = [
    [{ ...binding, device_id: null } as unknown as WorkspaceRegistration, documentId, oldCreateId, parentId, "copy.md"],
    [binding, "invalid", oldCreateId, parentId, "copy.md"], [binding, documentId, "invalid", parentId, "copy.md"],
    [binding, documentId, oldCreateId, "invalid", "copy.md"], [binding, documentId, oldCreateId, parentId, ""],
    [binding, documentId, oldCreateId, parentId, "plain.txt"], [binding, documentId, oldCreateId, parentId, "../copy.md"],
    [binding, documentId, oldCreateId, parentId, "bad\\name.md"], [binding, documentId, oldCreateId, parentId, "bad\nname.md"],
    [binding, documentId, oldCreateId, parentId, "a".repeat(253) + ".md"],
  ];
  for (const [scope, doc, oldCreate, parent, name] of invalid) {
    mocked.invoke.mockClear();
    await expect(reconfirm(scope, doc, oldCreate, parent, name)).rejects.toThrow();
    expect(mocked.invoke).not.toHaveBeenCalledWith("workspace_reconfirm_deleted_draft_copy", expect.anything());
  }
});

test("재승인 응답은 pending과 원본·이전 create와 구분되는 새 ID만 허용한다", async () => {
  for (const value of [
    null, {}, { ...result, status: "saved" }, { ...result, node_id: "invalid" },
    { ...result, create_operation_id: "invalid" }, { ...result, node_id: documentId },
    { ...result, node_id: newCreateId }, { ...result, create_operation_id: oldCreateId },
    { ...result, extra: true },
  ]) {
    respond(value);
    await expect(reconfirm(binding, documentId, oldCreateId, parentId, "copy.md")).rejects.toThrow();
  }
});
