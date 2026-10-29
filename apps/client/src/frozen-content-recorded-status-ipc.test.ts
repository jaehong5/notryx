import { beforeEach, expect, test, vi } from "vitest";
import { workspaceFrozenContentDecisionStatus } from "./ipc";
import type { WorkspaceRegistration } from "../../../packages/protocol-types";

const mocked = vi.hoisted(() => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: mocked.invoke }));
const current: WorkspaceRegistration = { server_origin: "https://example.invalid", workspace_id: "workspace", workspace_epoch: "epoch", device_id: "current", device_stream_id: "stream" };
const old: WorkspaceRegistration = { ...current, device_id: "old", device_stream_id: "old-stream" };
const documentId = "019a1000-0000-7000-8000-000000000002", oldOperationId = "019a1000-0000-7000-8000-000000000003", newId = "019a1000-0000-7000-8000-000000000004";
const decision = { operation_id: newId, operation_seq: "7", status: "queued", approved_generation: "2", approved_hash: "sha256:" + "a".repeat(64) };
const response = { binding: current, old_binding: old, document_id: documentId, old_operation_id: oldOperationId, decision, dismissed: null };
function respond(value: unknown) { mocked.invoke.mockImplementation(async (command: string) => command === "m0_renderer_session" ? "owner" : value); }
beforeEach(() => mocked.invoke.mockReset());
const status = () => workspaceFrozenContentDecisionStatus(current, old, documentId, oldOperationId);

test("양쪽 등록과 문서·이전 작업에 묶인 결정 상태를 읽는다", async () => {
  respond(response);
  await expect(status()).resolves.toEqual({ binding: current, old_binding: old, document_id: documentId, old_operation_id: oldOperationId, ...decision });
  expect(mocked.invoke).toHaveBeenCalledWith("workspace_frozen_content_decision_status", { request: { renderer_session_id: "owner", binding: current, old_binding: old, document_id: documentId, old_operation_id: oldOperationId } });
  respond({ ...response, decision: null }); await expect(status()).resolves.toBeNull();
});

test("다른 등록, 문서, 이전 작업이나 유효하지 않은 상태 및 승인 원문 식별자를 거절한다", async () => {
  for (const bad of [
    { ...response, binding: { ...current, device_id: "other" } }, { ...response, old_binding: { ...old, workspace_epoch: "other" } },
    { ...response, document_id: newId }, { ...response, old_operation_id: newId }, { ...response, unknown: true },
    { ...response, decision: { ...decision, operation_id: oldOperationId } }, { ...response, decision: { ...decision, operation_id: "invalid" } },
    { ...response, decision: { ...decision, operation_seq: "0" } }, { ...response, decision: { ...decision, approved_generation: "0" } },
    { ...response, decision: { ...decision, approved_hash: "sha256:invalid" } }, { ...response, decision: { ...decision, status: "unknown" } },
    { ...response, decision: { ...decision, extra: true } }, { ...response, decision: undefined }, { ...response, dismissed: undefined },
  ]) { respond(bad); await expect(status()).rejects.toThrow(); }
});

test.each(["queued", "sent", "committed", "conflict", "abandoning", "abandoned", "failed"])("%s 상태를 변환하지 않는다", async (state) => {
  respond({ ...response, decision: { ...decision, status: state } });
  await expect(status()).resolves.toMatchObject({ status: state });
});
