import { beforeEach, expect, test, vi } from "vitest";
import * as ipc from "./ipc";
import type { WorkspaceDeletedDraftCopyStatus, WorkspaceOpenedDocument, WorkspaceRegistration } from "../../../packages/protocol-types";

const mocked = vi.hoisted(() => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: mocked.invoke }));

const binding: WorkspaceRegistration = { server_origin: "https://example.invalid", workspace_id: "workspace", workspace_epoch: "epoch", device_id: "device", device_stream_id: "stream" };
const documentId = "019a1000-0000-7000-8000-000000000002";
const parentId = "019a1000-0000-7000-8000-000000000003";
const copyId = "019a1000-0000-7000-8000-000000000004";
const createId = "019a1000-0000-7000-8000-000000000005";
const contentId = "019a1000-0000-7000-8000-000000000006";
const otherId = "019a1000-0000-7000-8000-000000000007";
const previousSession = "previous-session";
const document: WorkspaceOpenedDocument = { binding, document_id: documentId, edit_session_id: "new-session", generation: "3", bytes_base64: "ZHJhZnQK", readonly: false, local_draft: true, has_session: true, cache_warning: null, open_notice: null };
const state: WorkspaceDeletedDraftCopyStatus = { binding, document_id: documentId, generation: "2", local_hash: "sha256:efb83f2a277e9f49b38efd505f5cbb93885e721b6bd16b788937c9396174c006", tombstone_seq: "7", terminal_operation_id: null, parent_id: parentId, name: "초안 사본.md", node_id: copyId, create_operation_id: createId, create_status: "created", create_failure: null, content_status: "pending", content_operation_id: contentId, blocking_followup: null };

type Reopen = (binding: WorkspaceRegistration, documentId: string, previousSession: string) => Promise<{ state: WorkspaceDeletedDraftCopyStatus | null; document: WorkspaceOpenedDocument }>;
const reopen: Reopen = (...args) => (ipc as unknown as { reopenWorkspaceDeletedDraftCopy: Reopen }).reopenWorkspaceDeletedDraftCopy(...args);

function respond(value: unknown) {
  mocked.invoke.mockImplementation(async (command: string) => command === "m0_renderer_session" ? "owner" : value);
}

beforeEach(() => mocked.invoke.mockReset());

test("재열기는 등록·원본 문서·이전 session을 전달하고 새 session과 저장된 선택을 반환한다", async () => {
  respond({ state, document });
  await expect(reopen(binding, documentId, previousSession)).resolves.toEqual({ state, document });
  expect(mocked.invoke).toHaveBeenCalledWith("workspace_reopen_deleted_draft_copy", { request: {
    renderer_session_id: "owner", binding, document_id: documentId, previous_edit_session_id: previousSession,
  } });
  respond({ state: null, document });
  await expect(reopen(binding, documentId, previousSession)).resolves.toEqual({ state: null, document });
});

test("재열기는 다른 등록·원본 문서 또는 이전 session을 반환하면 거절한다", async () => {
  for (const invalid of [
    ...Object.keys(binding).map((field) => ({ ...document, binding: { ...binding, [field]: "other" } })),
    { ...document, binding: null }, { ...document, document_id: otherId }, { ...document, edit_session_id: previousSession },
    { ...document, edit_session_id: "" }, { ...document, readonly: true }, { ...document, local_draft: false },
  ]) {
    respond({ state, document: invalid });
    await expect(reopen(binding, documentId, previousSession)).rejects.toThrow();
  }
});

test("재열기는 다른 선택 identity 또는 잘못된 삭제 증거와 상태 조합을 거절한다", async () => {
  for (const invalid of [
    { ...state, binding: { ...binding, workspace_epoch: "other" } }, { ...state, document_id: otherId },
    { ...state, tombstone_seq: null, terminal_operation_id: null }, { ...state, terminal_operation_id: createId },
    { ...state, local_hash: "invalid" }, { ...state, node_id: documentId }, { ...state, generation: "0" },
    { ...state, create_status: "needs_review", create_failure: null }, { ...state, content_status: "saved", content_operation_id: null },
  ]) {
    respond({ state: invalid, document });
    await expect(reopen(binding, documentId, previousSession)).rejects.toThrow();
  }
});

test("재열기는 잘못된 요청 및 누락되거나 예기치 않은 응답 필드를 거절한다", async () => {
  respond({ state, document });
  for (const [scope, doc, old] of [
    [binding, "invalid", previousSession], [binding, documentId, ""],
    [{ ...binding, device_stream_id: null } as unknown as WorkspaceRegistration, documentId, previousSession],
  ] as const) {
    mocked.invoke.mockClear();
    await expect(reopen(scope, doc, old)).rejects.toThrow();
    expect(mocked.invoke).not.toHaveBeenCalledWith("workspace_reopen_deleted_draft_copy", expect.anything());
  }
  for (const value of [null, {}, { state }, { document }, { state, document, extra: true }, { state, document: null }]) {
    respond(value);
    await expect(reopen(binding, documentId, previousSession)).rejects.toThrow();
  }
});
