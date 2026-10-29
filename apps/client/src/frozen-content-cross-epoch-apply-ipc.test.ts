import { beforeEach, expect, test, vi } from "vitest";
import { workspaceApplyFrozenContentCrossEpoch, workspaceInspectFrozenContentCrossEpoch } from "./ipc";
import type { WorkspaceFrozenContentInspection, WorkspaceRegistration } from "../../../packages/protocol-types";

const mocked = vi.hoisted(() => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: mocked.invoke }));
const current: WorkspaceRegistration = { server_origin: "https://example.invalid", workspace_id: "workspace", workspace_epoch: "epoch", device_id: "current", device_stream_id: "stream" };
const old: WorkspaceRegistration = { ...current, workspace_epoch: "old-epoch", device_id: "old", device_stream_id: "old-stream" };
const documentId = "019a1000-0000-7000-8000-000000000002", operationId = "019a1000-0000-7000-8000-000000000003", newId = "019a1000-0000-7000-8000-000000000004";
const inspected: WorkspaceFrozenContentInspection = {
  current_binding: current, old_binding: old, document_id: documentId, operation_id: operationId, operation_seq: "1", operation_status: "sent", abandon_state: "none", operation_generation: "1", local_generation: "2", expected_revision: "base-rev", outcome_unknown: true,
  base_revision: "base-rev", base_bytes_base64: "YmFzZQo=", base_hash: "sha256:f34848ca92665c342abd5816c9e3eda0e82180671195362bcd0080544a3bc2ac",
  operation_bytes_base64: "YmFzZQo=", operation_hash: "sha256:f34848ca92665c342abd5816c9e3eda0e82180671195362bcd0080544a3bc2ac",
  local_bytes_base64: "bG9jYWwK", local_hash: "sha256:efb83f2a277e9f49b38efd505f5cbb93885e721b6bd16b788937c9396174c006",
  remote_revision: "remote-rev", remote_bytes_base64: "Y3VycmVudAo=", remote_hash: "sha256:48aa6cae8c70abdb28631d22b316e6d9f9d0768ec2911de7090e248b2afe6ca1",
};
beforeEach(() => {
  mocked.invoke.mockReset().mockImplementation(async (command: string) => {
    if (command === "m0_renderer_session") return "owner";
    if (command === "workspace_inspect_frozen_content_cross_epoch") return { inspected, current_target: target };
    if (command === "workspace_apply_frozen_content_cross_epoch") return { operation_id: newId, operation_seq: "7" };
    throw new Error("unexpected command");
  });
});
const target = { node_id: documentId, parent_id: "019a1000-0000-7000-8000-000000000005", name: "current.md", path: "/folder/current.md" };
async function review() {
  return workspaceInspectFrozenContentCrossEpoch(current, old, operationId, documentId);
}

test("검증한 교차 epoch 대상과 원문을 별도 확인 요청에 그대로 전달한다", async () => {
  const snapshot = await review();
  expect(snapshot.current_target).toEqual(target);
  expect(await workspaceApplyFrozenContentCrossEpoch(snapshot, true)).toEqual({ binding: current, operation_id: newId });
  expect(mocked.invoke).toHaveBeenCalledWith("workspace_apply_frozen_content_cross_epoch", { request: { renderer_session_id: "owner", inspected: { inspected, current_target: target }, same_node_is_not_identity: true } });
});

test("다른 대상 ID, 잘못된 경로와 변경된 원문은 교차 epoch 승인에 사용하지 않는다", async () => {
  const snapshot = await review();
  for (const changed of [
    { ...snapshot, current_target: { ...target, node_id: newId } },
    { ...snapshot, current_target: { ...target, parent_id: newId } },
    { ...snapshot, current_target: { ...target, name: "different.md" } },
    { ...snapshot, current_target: { ...target, path: "/folder/other.md" } },
    { ...snapshot, local_bytes: new Uint8Array([0]) }, { ...snapshot, remote_bytes: new Uint8Array([0]) },
    { ...snapshot, binding: { ...current, workspace_epoch: "replacement" } },
  ]) await expect(workspaceApplyFrozenContentCrossEpoch(changed, true)).rejects.toThrow();
  expect(mocked.invoke).not.toHaveBeenCalledWith("workspace_apply_frozen_content_cross_epoch", expect.anything());
});

test("대상 동일성 미확인을 명시하지 않은 요청은 보내지 않는다", async () => {
  const snapshot = await review();
  await expect(workspaceApplyFrozenContentCrossEpoch(snapshot, false as true)).rejects.toThrow();
  expect(mocked.invoke).not.toHaveBeenCalledWith("workspace_apply_frozen_content_cross_epoch", expect.anything());
});

test("조회 응답의 부정확한 현재 대상, 불일치된 epoch와 알 수 없는 필드는 거절한다", async () => {
  for (const badTarget of [
    { ...target, node_id: newId }, { ...target, parent_id: "invalid" }, { ...target, name: "../x" },
    { ...target, path: "/folder/other.md" }, { ...target, path: "folder/current.md" },
    { ...target, extra: true },
  ]) {
    mocked.invoke.mockImplementation(async (command: string) => command === "m0_renderer_session" ? "owner" : { inspected, current_target: badTarget });
    await expect(review()).rejects.toThrow();
  }
  mocked.invoke.mockImplementation(async (command: string) => command === "m0_renderer_session" ? "owner" : { inspected: { ...inspected, old_binding: current }, current_target: target });
  await expect(review()).rejects.toThrow();
});
