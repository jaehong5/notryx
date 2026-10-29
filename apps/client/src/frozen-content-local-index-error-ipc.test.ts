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
  dependency: null, remote_evidence: { kind: "unknown" }, remote_index_error: true,
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

test("인덱스 오류를 로컬 증거에 보존하고 포기 요청은 차단한다", async () => {
  const snapshot = await review();
  expect(snapshot).toMatchObject({ remote_status: "unknown", remote_index_error: true });
  await expect(workspaceDismissFrozenContentLocal(snapshot)).rejects.toThrow();
  await expect(workspaceDismissFrozenContentLocal({ ...snapshot, remote_index_error: false } as FrozenContentLocalReviewSnapshot & { remote_index_error: boolean })).rejects.toThrow();
  expect(mocked.invoke).not.toHaveBeenCalledWith("workspace_dismiss_frozen_content_local", expect.anything());
});

test("진단이 누락되거나 boolean이 아니면 거절하고 오류와 확정 증거를 함께 허용하지 않는다", async () => {
  for (const value of [
    { ...inspected, remote_index_error: undefined }, { ...inspected, remote_index_error: "true" },
    { ...inspected, remote_evidence: { kind: "deleted", change_seq: "3" } },
    { ...inspected, remote_evidence: { kind: "too_large", change_seq: "3", size: "26214401" } },
  ]) {
    mocked.invoke.mockImplementation(async (command: string) => command === "m0_renderer_session" ? "owner" : value);
    await expect(review()).rejects.toThrow();
  }
});

test("인덱스가 정상인 unknown은 서버 삭제를 추정하지 않고 별도로 전달한다", async () => {
  mocked.invoke.mockImplementation(async (command: string) => command === "m0_renderer_session" ? "owner" : { ...inspected, remote_index_error: false });
  await expect(review()).resolves.toMatchObject({ remote_status: "unknown", remote_index_error: false });
});
