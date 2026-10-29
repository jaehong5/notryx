import { beforeEach, expect, test, vi } from "vitest";
import { workspaceDismissFrozenContentLocal, workspaceReviewFrozenContentLocal } from "./ipc";
import type { FrozenContentLocalReviewSnapshot } from "./frozen-content-review";
import type { WorkspaceRegistration } from "../../../packages/protocol-types";

const mocked = vi.hoisted(() => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: mocked.invoke }));
const current: WorkspaceRegistration = { server_origin: "https://example.invalid", workspace_id: "workspace", workspace_epoch: "epoch", device_id: "current", device_stream_id: "stream" };
const old: WorkspaceRegistration = { ...current, device_id: "old", device_stream_id: "old-stream" };
const documentId = "019a1000-0000-7000-8000-000000000002", operationId = "019a1000-0000-7000-8000-000000000003";
const inspected = {
  current_binding: current, old_binding: old, document_id: documentId, operation_id: operationId, operation_seq: "1", operation_status: "sent", abandon_state: "none", operation_generation: "1", local_generation: "2", expected_revision: "base-rev", outcome_unknown: true,
  base_revision: "base-rev", base_bytes_base64: "YmFzZQo=", base_hash: "sha256:f34848ca92665c342abd5816c9e3eda0e82180671195362bcd0080544a3bc2ac",
  operation_bytes_base64: "YmFzZQo=", operation_hash: "sha256:f34848ca92665c342abd5816c9e3eda0e82180671195362bcd0080544a3bc2ac",
  local_bytes_base64: "bG9jYWwK", local_hash: "sha256:efb83f2a277e9f49b38efd505f5cbb93885e721b6bd16b788937c9396174c006",
  dependency: null, remote_evidence: { kind: "unknown" }, remote_index_error: false,
};
beforeEach(() => {
  mocked.invoke.mockReset().mockImplementation(async (command: string) => {
    if (command === "m0_renderer_session") return "owner";
    if (command === "workspace_review_frozen_content_local") return inspected;
    if (command === "workspace_dismiss_frozen_content_local") return { operation_id: operationId, operation_seq: "1", operation_status: "sent", abandon_state: "none" };
    throw new Error("unexpected command");
  });
});
async function review(): Promise<FrozenContentLocalReviewSnapshot> {
  return workspaceReviewFrozenContentLocal(current, old, operationId, documentId);
}

test("서버 본문 없이 검증한 로컬 원문과 원격 상태 증거를 표시하고 그대로 확인한다", async () => {
  const snapshot = await review();
  expect(snapshot.remote_status).toBe("unknown");
  expect(snapshot.operation_bytes).toEqual(new TextEncoder().encode("base\n"));
  expect(mocked.invoke).toHaveBeenCalledWith("workspace_review_frozen_content_local", { request: { renderer_session_id: "owner", current_binding: current, old_binding: old, document_id: documentId, operation_id: operationId } });
  expect(await workspaceDismissFrozenContentLocal(snapshot)).toEqual({ binding: current, old_operation_id: operationId });
  expect(mocked.invoke).toHaveBeenCalledWith("workspace_dismiss_frozen_content_local", { request: { renderer_session_id: "owner", inspected } });
});

test("다른 등록, 원문 손상, 잘못된 원격 상태와 누락된 증거를 거절한다", async () => {
  for (const value of [
    { ...inspected, current_binding: { ...current, device_id: "other" } },
    { ...inspected, operation_id: documentId }, { ...inspected, operation_hash: "sha256:" + "0".repeat(64) },
    { ...inspected, dependency: undefined }, { ...inspected, remote_evidence: { kind: "deleted" } },
    { ...inspected, remote_evidence: { kind: "too_large", change_seq: "3", size: "1" } },
    { ...inspected, remote_evidence: { kind: "unknown", change_seq: "3" } },
  ]) {
    mocked.invoke.mockImplementation(async (command: string) => command === "m0_renderer_session" ? "owner" : value);
    await expect(review()).rejects.toThrow();
  }
});

test("표시한 로컬 원문 또는 원격 상태를 바꾸면 포기 명령을 보내지 않는다", async () => {
  const snapshot = await review();
  for (const changed of [
    { ...snapshot, operation_bytes: new Uint8Array([0]) }, { ...snapshot, base_bytes: new Uint8Array([0]) },
    { ...snapshot, local_bytes: new Uint8Array([0]) }, { ...snapshot, generation: "3" },
    { ...snapshot, dependency: "019a1000-0000-7000-8000-000000000004" },
    { ...snapshot, remote_status: "deleted" as const }, { ...snapshot, old_binding: { ...old, device_id: "other" } },
  ]) await expect(workspaceDismissFrozenContentLocal(changed)).rejects.toThrow();
  expect(mocked.invoke).not.toHaveBeenCalledWith("workspace_dismiss_frozen_content_local", expect.anything());
});

test("삭제와 크기 초과에는 확인된 변경 순서와 크기가 필요하다", async () => {
  for (const remote_evidence of [{ kind: "deleted", change_seq: "3" }, { kind: "too_large", change_seq: "3", size: "26214401" }]) {
    mocked.invoke.mockImplementation(async (command: string) => command === "m0_renderer_session" ? "owner" : { ...inspected, remote_evidence });
    await expect(review()).resolves.toMatchObject({ remote_status: remote_evidence.kind });
  }
});
