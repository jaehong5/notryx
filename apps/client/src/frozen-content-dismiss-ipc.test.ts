import { beforeEach, expect, test, vi } from "vitest";
import { workspaceDismissFrozenContent, workspaceInspectFrozenContent, workspaceFrozenContentDecisionStatus } from "./ipc";
import type { FrozenContentReviewSnapshot } from "./frozen-content-review";
import type { WorkspaceFrozenContentInspection, WorkspaceRegistration } from "../../../packages/protocol-types";

const mocked = vi.hoisted(() => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: mocked.invoke }));
const current: WorkspaceRegistration = { server_origin: "https://example.invalid", workspace_id: "workspace", workspace_epoch: "epoch", device_id: "current", device_stream_id: "stream" };
const old: WorkspaceRegistration = { ...current, device_id: "old", device_stream_id: "old-stream" };
const documentId = "019a1000-0000-7000-8000-000000000002", operationId = "019a1000-0000-7000-8000-000000000003";
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
    if (command === "workspace_inspect_frozen_content") return inspected;
    if (command === "workspace_dismiss_frozen_content") return { operation_id: operationId, operation_seq: "1", operation_status: "sent", abandon_state: "none" };
    throw new Error("unexpected command");
  });
});
async function review(): Promise<FrozenContentReviewSnapshot> {
  const value = await workspaceInspectFrozenContent(current, old, operationId, documentId);
  return { binding: value.current_binding, old_binding: value.old_binding, document_id: value.document_id, operation_id: value.operation_id,
    operation_seq: value.operation_seq, generation: value.operation_generation, local_generation: value.local_generation,
    status: value.operation_status, abandon_state: value.abandon_state, expected_revision: value.expected_revision, outcome_unknown: value.outcome_unknown,
    base_revision: value.base_revision, remote_revision: value.remote_revision, operation_bytes: value.operation_bytes, base_bytes: value.base_bytes,
    local_bytes: value.local_bytes, remote_bytes: value.remote_bytes, inspected: value.inspected };
}

test("검증한 비교 snapshot을 포기 요청에 그대로 전달한다", async () => {
  const snapshot = await review();
  expect(await workspaceDismissFrozenContent(snapshot)).toEqual({ binding: current, old_operation_id: operationId });
  expect(mocked.invoke).toHaveBeenCalledWith("workspace_dismiss_frozen_content", { request: { renderer_session_id: "owner", inspected } });
});

test("바뀐 비교 원문과 상태는 native 호출 전에 거절한다", async () => {
  const snapshot = await review();
  for (const altered of [
    { ...snapshot, operation_bytes: new Uint8Array([0]) }, { ...snapshot, base_bytes: new Uint8Array([0]) },
    { ...snapshot, local_bytes: new Uint8Array([0]) }, { ...snapshot, remote_bytes: new Uint8Array([0]) },
    { ...snapshot, generation: "3" }, { ...snapshot, status: "queued" as const },
    { ...snapshot, old_binding: { ...old, device_id: "other" } }, { ...snapshot, inspected: undefined },
  ]) await expect(workspaceDismissFrozenContent(altered)).rejects.toThrow();
  expect(mocked.invoke).not.toHaveBeenCalledWith("workspace_dismiss_frozen_content", expect.anything());
});

test("결과의 이전 작업 식별자와 상태 및 순번을 검증한다", async () => {
  const snapshot = await review();
  for (const bad of [
    { operation_id: "invalid", operation_seq: "1", operation_status: "sent", abandon_state: "none" },
    { operation_id: operationId, operation_seq: "2", operation_status: "sent", abandon_state: "none" },
    { operation_id: operationId, operation_seq: "1", operation_status: "queued", abandon_state: "none" },
    { operation_id: operationId, operation_seq: "1", operation_status: "sent", abandon_state: "sent" },
    { operation_id: operationId, operation_seq: "1", operation_status: "sent", abandon_state: "none", extra: true },
  ]) {
    mocked.invoke.mockImplementation(async (command: string) => command === "m0_renderer_session" ? "owner" : bad);
    await expect(workspaceDismissFrozenContent(snapshot)).rejects.toThrow();
  }
});

test("결정 상태는 포기된 이전 작업의 순번과 불확정 상태를 검증한다", async () => {
  const response = { binding: current, old_binding: old, document_id: documentId, old_operation_id: operationId, decision: null,
    dismissed: { operation_id: operationId, operation_seq: "1", operation_status: "sent", abandon_state: "none" } };
  mocked.invoke.mockImplementation(async (command: string) => command === "m0_renderer_session" ? "owner" : response);
  await expect(workspaceFrozenContentDecisionStatus(current, old, documentId, operationId)).resolves.toEqual({
    kind: "dismissed", binding: current, old_binding: old, document_id: documentId, old_operation_id: operationId,
    operation_seq: "1", old_status: "sent", abandon_state: "none" });
  for (const dismissed of [
    { ...response.dismissed, operation_id: "invalid" }, { ...response.dismissed, operation_seq: "0" }, { ...response.dismissed, operation_status: "unknown" },
    { ...response.dismissed, abandon_state: "sent" }, { ...response.dismissed, operation_status: "abandoning", abandon_state: "none" },
    { ...response.dismissed, extra: true },
  ]) {
    mocked.invoke.mockImplementation(async (command: string) => command === "m0_renderer_session" ? "owner" : { ...response, dismissed });
    await expect(workspaceFrozenContentDecisionStatus(current, old, documentId, operationId)).rejects.toThrow();
  }
  mocked.invoke.mockImplementation(async (command: string) => command === "m0_renderer_session" ? "owner" : { ...response, decision: {} });
  await expect(workspaceFrozenContentDecisionStatus(current, old, documentId, operationId)).rejects.toThrow();
});
