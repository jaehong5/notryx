import { afterEach, expect, test, vi } from "vitest";
import { AutoTransmit } from "./auto-transmit";
import { receiptWaitResult, staleTransmissionResult } from "./send-result";

afterEach(() => vi.useRealTimers());

test("a response from an earlier registration cannot confirm a pending receipt even if the binding matches", async () => {
  vi.useFakeTimers();
  const scope = { binding: "binding-a", documentId: "doc-a", editSessionId: "session-a" };
  const confirmed = vi.fn();
  let attempts = 0;
  const scheduler = new AutoTransmit({
    queue: async (_scope, generation) => ({ operationId: "op-a", documentId: scope.documentId, generation }),
    transmit: async () => {
      attempts++;
      if (attempts === 1) return { status: "ack_pending", operationId: "op-a" };
      if (attempts === 2) return staleTransmissionResult(1, 2, scope.binding, [scope.binding]) ?? { status: "idle" };
      return staleTransmissionResult(2, 2, scope.binding, [scope.binding]) ?? { status: "idle" };
    },
    acknowledged: vi.fn(),
    receiptConfirmed: confirmed,
    authRequired: vi.fn(),
    failed: vi.fn(),
  });
  scheduler.activateBinding(scope.binding);
  scheduler.openSession(scope);
  scheduler.edit(scope, "1");
  scheduler.durable(scope, "1");
  await scheduler.flushDocument(scope);
  await vi.advanceTimersByTimeAsync(0);
  expect(attempts).toBe(1);
  await vi.advanceTimersByTimeAsync(1000);
  expect(attempts).toBe(2);
  expect(confirmed).not.toHaveBeenCalled();
  await vi.advanceTimersByTimeAsync(2000);
  expect(confirmed).toHaveBeenCalledExactlyOnceWith("op-a");
  await scheduler.dispose();
});

test("a response from a different active binding is retried instead of treated as idle", () => {
  expect(staleTransmissionResult(1, 1, "old-binding", ["new-binding"])).toEqual({ status: "pending" });
  expect(staleTransmissionResult(1, 1, "old-binding", [])).toBeUndefined();
  expect(staleTransmissionResult(1, 1, "new-binding", ["new-binding"])).toBeUndefined();
});

test.each(["ack_pending", "auth_required", "gateway_auth_required"] as const)("%s carries only a fully matched operation identity", (status) => {
  const identity = { operation_id: "op-a", document_id: "doc-a", generation: "1" };
  const operation = { scope: { binding: "binding-a" }, documentId: "doc-a", generation: "1" };
  expect(receiptWaitResult(status, identity, "binding-a", operation)).toEqual({ status, operationId: "op-a" });
  expect(receiptWaitResult(status, identity, "binding-b", operation)).toEqual({ status });
  expect(receiptWaitResult(status, { ...identity, document_id: "doc-b" }, "binding-a", operation)).toEqual({ status });
  expect(receiptWaitResult(status, { ...identity, generation: "2" }, "binding-a", operation)).toEqual({ status });
  expect(receiptWaitResult(status, { ...identity, operation_id: null }, "binding-a", operation)).toEqual({ status });
  expect(receiptWaitResult(status, identity, "binding-a", undefined)).toEqual({ status });
});

test.each(["auth_required", "gateway_auth_required"] as const)("%s confirms only the matching receipt after authentication resumes", async (status) => {
  vi.useFakeTimers();
  const scope = { binding: "binding-a", documentId: "doc-a", editSessionId: "session-a" };
  const confirmed = vi.fn();
  const authenticated = vi.fn();
  const acknowledged = vi.fn();
  let calls = 0;
  const scheduler = new AutoTransmit({
    queue: async (_scope, generation) => ({ operationId: "op-a", documentId: scope.documentId, generation }),
    transmit: async () => ++calls === 1
      ? receiptWaitResult(status, { operation_id: "op-a", document_id: scope.documentId, generation: "1" }, scope.binding, { scope, documentId: scope.documentId, generation: "1" })
      : { status: "idle" },
    acknowledged,
    receiptConfirmed: confirmed,
    authRequired: authenticated,
    failed: vi.fn(),
  });
  scheduler.activateBinding(scope.binding);
  scheduler.openSession(scope);
  scheduler.edit(scope, "1");
  scheduler.durable(scope, "1");
  await scheduler.flushDocument(scope);
  await vi.advanceTimersByTimeAsync(0);
  expect(authenticated).toHaveBeenCalledExactlyOnceWith(status);
  expect(confirmed).not.toHaveBeenCalled();
  scheduler.resume();
  await vi.advanceTimersByTimeAsync(0);
  expect(confirmed).toHaveBeenCalledExactlyOnceWith("op-a");
  expect(acknowledged).not.toHaveBeenCalled();
  await scheduler.dispose();
});

test("a mismatched authentication response never confirms an unrelated operation", async () => {
  vi.useFakeTimers();
  const scope = { binding: "binding-a", documentId: "doc-a", editSessionId: "session-a" };
  const confirmed = vi.fn();
  let calls = 0;
  const scheduler = new AutoTransmit({
    queue: async (_scope, generation) => ({ operationId: "op-a", documentId: scope.documentId, generation }),
    transmit: async () => ++calls === 1
      ? receiptWaitResult("auth_required", { operation_id: "op-a", document_id: "doc-other", generation: "1" }, scope.binding, { scope, documentId: scope.documentId, generation: "1" })
      : { status: "idle" },
    acknowledged: vi.fn(),
    receiptConfirmed: confirmed,
    authRequired: vi.fn(),
    failed: vi.fn(),
  });
  scheduler.activateBinding(scope.binding);
  scheduler.openSession(scope);
  scheduler.edit(scope, "1");
  scheduler.durable(scope, "1");
  await scheduler.flushDocument(scope);
  await vi.advanceTimersByTimeAsync(0);
  scheduler.resume();
  await vi.advanceTimersByTimeAsync(0);
  expect(confirmed).not.toHaveBeenCalled();
  await scheduler.dispose();
});
