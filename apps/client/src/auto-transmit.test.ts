import { afterEach, expect, test, vi } from "vitest";
import { AutoTransmit, type AutoTransmitPorts, type TransmitScope } from "./auto-transmit";

const a: TransmitScope = { binding: "binding-a", documentId: "a", editSessionId: "edit-a" };
const b: TransmitScope = { binding: "binding-b", documentId: "b", editSessionId: "edit-b" };

function setup() {
  vi.useFakeTimers();
  vi.setSystemTime(0);
  const ports: AutoTransmitPorts = {
    queue: vi.fn(async (scope, generation) => ({ operationId: `op-${scope.documentId}-${generation}`, documentId: scope.documentId, generation })),
    transmit: vi.fn(async () => ({ status: "idle" as const })),
    acknowledged: vi.fn(),
    authRequired: vi.fn(),
    failed: vi.fn(),
  };
  return { ports, scheduler: new AutoTransmit(ports) };
}

afterEach(() => vi.useRealTimers());

test("queues only a durable generation after 750 ms of inactivity", async () => {
  const { ports, scheduler } = setup();
  scheduler.edit(a, "1");
  await vi.advanceTimersByTimeAsync(1000);
  expect(ports.queue).not.toHaveBeenCalled();
  scheduler.durable(a, "1");
  await vi.advanceTimersByTimeAsync(0);
  expect(ports.queue).toHaveBeenCalledExactlyOnceWith(a, "1");
  expect(ports.transmit).toHaveBeenCalledOnce();
  scheduler.edit(a, "2");
  scheduler.durable(a, "2");
  await vi.advanceTimersByTimeAsync(749);
  expect(ports.queue).toHaveBeenCalledTimes(1);
  await vi.advanceTimersByTimeAsync(1);
  expect(ports.queue).toHaveBeenLastCalledWith(a, "2");
  await scheduler.dispose();
});

test("restoring a durable checkpoint queues it without another edit", async () => {
  const { ports, scheduler } = setup();
  await scheduler.restore(a, "7");
  expect(ports.queue).toHaveBeenCalledExactlyOnceWith(a, "7");
  expect(ports.transmit).toHaveBeenCalledOnce();
  expect(ports.acknowledged).not.toHaveBeenCalled();
  await scheduler.dispose();
});

test.each(["queued", "sent"] as const)("restoring an existing %s operation retains its identity and remains pending", async (status) => {
  const { ports, scheduler } = setup();
  vi.mocked(ports.queue).mockResolvedValueOnce({ operationId: `existing-${status}`, documentId: "a", generation: "7", committed: false });
  await scheduler.restore(a, "7");
  expect(ports.queue).toHaveBeenCalledExactlyOnceWith(a, "7");
  expect(ports.acknowledged).not.toHaveBeenCalled();
  scheduler.acknowledge(a, { operationId: `existing-${status}`, documentId: "a", generation: "7", committed: false }, true);
  expect(ports.acknowledged).toHaveBeenCalledExactlyOnceWith(a, "7", true);
  await scheduler.dispose();
});

test("restoring a committed generation reports server completion without another send", async () => {
  const { ports, scheduler } = setup();
  vi.mocked(ports.queue).mockResolvedValueOnce({ operationId: "already-committed", documentId: "a", generation: "7", committed: true });
  await scheduler.restore(a, "7");
  expect(ports.queue).toHaveBeenCalledExactlyOnceWith(a, "7");
  expect(ports.acknowledged).toHaveBeenCalledExactlyOnceWith(a, "7", true);
  expect(ports.transmit).not.toHaveBeenCalled();
  await scheduler.dispose();
});

test("a restored committed result cannot mark later input as synced", async () => {
  const { ports, scheduler } = setup();
  let resolve!: (value: { operationId: string; documentId: string; generation: string; committed: boolean }) => void;
  vi.mocked(ports.queue).mockReturnValueOnce(new Promise((done) => { resolve = done; }));
  const restoring = scheduler.restore(a, "7");
  await Promise.resolve();
  scheduler.edit(a, "8");
  resolve({ operationId: "committed-7", documentId: "a", generation: "7", committed: true });
  await restoring;
  expect(ports.acknowledged).toHaveBeenCalledExactlyOnceWith(a, "7", false);
  scheduler.durable(a, "8");
  await scheduler.dispose();
});

test("a clean generation zero does not create a restored operation", async () => {
  const { ports, scheduler } = setup();
  await scheduler.restore(a, "0");
  expect(ports.queue).not.toHaveBeenCalled();
  expect(ports.transmit).not.toHaveBeenCalled();
  await scheduler.dispose();
});

test("a newer checkpoint during restore is queued after stale_checkpoint", async () => {
  const { ports, scheduler } = setup();
  let reject!: (error: Error) => void;
  vi.mocked(ports.queue).mockReturnValueOnce(new Promise((_resolve, fail) => { reject = fail; }));
  const restoring = scheduler.restore(a, "7");
  await Promise.resolve();
  scheduler.edit(a, "8"); scheduler.durable(a, "8");
  reject(new Error("stale_checkpoint"));
  await restoring;
  await vi.advanceTimersByTimeAsync(750);
  expect(ports.queue).toHaveBeenCalledTimes(2);
  expect(ports.queue).toHaveBeenLastCalledWith(a, "8");
  expect(ports.failed).not.toHaveBeenCalled();
  await scheduler.dispose();
});

test("a restored session cannot be initialized twice, including generation zero", async () => {
  const { scheduler } = setup();
  await scheduler.restore(a, "0");
  await expect(scheduler.restore(a, "7")).rejects.toThrow("session_already_initialized");
  await scheduler.dispose();
});

test("continuous durable edits are queued at the five-second limit", async () => {
  const { ports, scheduler } = setup();
  scheduler.edit(a, "1");
  scheduler.durable(a, "1");
  for (let n = 2; n <= 25; n++) {
    await vi.advanceTimersByTimeAsync(200);
    scheduler.edit(a, String(n));
    scheduler.durable(a, String(n));
  }
  expect(ports.queue).not.toHaveBeenCalled();
  await vi.advanceTimersByTimeAsync(200);
  expect(ports.queue).toHaveBeenCalledExactlyOnceWith(a, "25");
  await scheduler.dispose();
});

test("continuous input transmits a durable generation at five seconds even while newer input is uncommitted", async () => {
  const { ports, scheduler } = setup();
  scheduler.edit(a, "1"); scheduler.durable(a, "1");
  for (let n = 2; n <= 25; n++) {
    await vi.advanceTimersByTimeAsync(200);
    scheduler.edit(a, String(n));
    if (n < 25) scheduler.durable(a, String(n));
  }
  await vi.advanceTimersByTimeAsync(200);
  expect(ports.queue).toHaveBeenCalledExactlyOnceWith(a, "24");
  expect(ports.transmit).toHaveBeenCalledOnce();
  scheduler.acknowledge(a, { operationId: "op-a-24", documentId: "a", generation: "24" }, true);
  expect(ports.acknowledged).toHaveBeenCalledExactlyOnceWith(a, "24", false);
  scheduler.durable(a, "25");
  await scheduler.dispose();
});

test("composition defers network transmission without preventing durable checkpoints", async () => {
  const { ports, scheduler } = setup();
  scheduler.composition(a, true);
  scheduler.edit(a, "1");
  scheduler.durable(a, "1");
  await vi.advanceTimersByTimeAsync(6000);
  expect(ports.queue).not.toHaveBeenCalled();
  expect(ports.transmit).not.toHaveBeenCalled();
  scheduler.composition(a, false);
  await vi.advanceTimersByTimeAsync(0);
  expect(ports.queue).toHaveBeenCalledExactlyOnceWith(a, "1");
  expect(ports.transmit).toHaveBeenCalledOnce();
  await scheduler.dispose();
});

test("a document switch queues the last committed generation before returning", async () => {
  const { ports, scheduler } = setup();
  let done!: () => void;
  const waiting = new Promise<{ operationId: string; documentId: string; generation: string }>((resolve) => { done = () => resolve({ operationId: "op-a-1", documentId: "a", generation: "1" }); });
  vi.mocked(ports.queue).mockReturnValueOnce(waiting);
  scheduler.edit(a, "1");
  scheduler.durable(a, "1");
  const flush = scheduler.flushDocument(a);
  let complete = false;
  void flush.then(() => { complete = true; });
  await Promise.resolve();
  expect(ports.queue).toHaveBeenCalledExactlyOnceWith(a, "1");
  expect(complete).toBe(false);
  done();
  await flush;
  expect(complete).toBe(true);
  await scheduler.dispose();
});

test("queue failure prevents the switch and leaves the generation retryable", async () => {
  const { ports, scheduler } = setup();
  vi.mocked(ports.queue).mockRejectedValueOnce(new Error("disk"));
  scheduler.edit(a, "1");
  scheduler.durable(a, "1");
  await expect(scheduler.flushDocument(a)).rejects.toThrow("disk");
  expect(ports.transmit).not.toHaveBeenCalled();
  await scheduler.flushDocument(a);
  expect(ports.queue).toHaveBeenCalledTimes(2);
  await scheduler.dispose();
});

test("a blocked queue can close the native session while retaining the durable draft", async () => {
  const { ports, scheduler } = setup();
  const close = vi.fn(async () => {});
  vi.mocked(ports.queue).mockRejectedValueOnce(new Error("ReconciliationRequired"));
  scheduler.edit(a, "1"); scheduler.durable(a, "1");
  await expect(scheduler.deactivate(a, close)).rejects.toThrow("ReconciliationRequired");
  expect(close).not.toHaveBeenCalled();
  await scheduler.deactivatePreservingDraft(a, close);
  expect(close).toHaveBeenCalledOnce();
  expect(() => scheduler.edit(a, "2")).toThrow("stale_session");
  expect(ports.transmit).not.toHaveBeenCalled();
  await scheduler.dispose();
});

test("failed native close leaves the scheduler session usable", async () => {
  const { ports, scheduler } = setup();
  vi.mocked(ports.queue).mockRejectedValueOnce(new Error("OutboxBlocked"));
  scheduler.edit(a, "1"); scheduler.durable(a, "1");
  await expect(scheduler.deactivate(a, async () => {})).rejects.toThrow("OutboxBlocked");
  await expect(scheduler.deactivatePreservingDraft(a, async () => { throw new Error("close failed"); })).rejects.toThrow("close failed");
  scheduler.edit(a, "2"); scheduler.durable(a, "2");
  const close = vi.fn(async () => {});
  await scheduler.deactivate(a, close);
  expect(ports.queue).toHaveBeenLastCalledWith(a, "2");
  expect(close).toHaveBeenCalledOnce();
  await scheduler.dispose();
});

test("preserving a draft refuses to close an uncommitted edit", async () => {
  const { scheduler } = setup();
  const close = vi.fn(async () => {});
  scheduler.edit(a, "1");
  await expect(scheduler.deactivatePreservingDraft(a, close)).rejects.toThrow("checkpoint_pending");
  expect(close).not.toHaveBeenCalled();
  scheduler.durable(a, "1");
  await scheduler.deactivatePreservingDraft(a, close);
  await scheduler.dispose();
});

test("failed native close after successful queue does not deactivate the session", async () => {
  const { scheduler } = setup();
  scheduler.edit(a, "1"); scheduler.durable(a, "1");
  await expect(scheduler.deactivate(a, async () => { throw new Error("close failed"); })).rejects.toThrow("close failed");
  scheduler.edit(a, "2"); scheduler.durable(a, "2");
  await scheduler.deactivate(a, async () => {});
  await scheduler.dispose();
});

test("composition that starts during queue prevents closing the native session", async () => {
  const { ports, scheduler } = setup();
  let resolve!: (value: { operationId: string; documentId: string; generation: string }) => void;
  vi.mocked(ports.queue).mockReturnValueOnce(new Promise((done) => { resolve = done; }));
  scheduler.edit(a, "1"); scheduler.durable(a, "1");
  const close = vi.fn(async () => {});
  const switching = scheduler.deactivate(a, close);
  await Promise.resolve();
  scheduler.composition(a, true);
  resolve({ operationId: "op-a-1", documentId: "a", generation: "1" });
  await expect(switching).rejects.toThrow("composition_in_progress");
  expect(close).not.toHaveBeenCalled();
  scheduler.composition(a, false);
  await scheduler.deactivate(a, close);
  expect(close).toHaveBeenCalledOnce();
  await scheduler.dispose();
});

test("closing after a queue failure never sends an earlier composition result", async () => {
  const { ports, scheduler } = setup();
  scheduler.edit(a, "1"); scheduler.durable(a, "1");
  await scheduler.flushDocument(a);
  scheduler.composition(a, true);
  scheduler.edit(a, "2"); scheduler.composition(a, false); scheduler.durable(a, "2");
  vi.mocked(ports.queue).mockRejectedValueOnce(new Error("OutboxBlocked"));
  await expect(scheduler.deactivate(a, async () => {})).rejects.toThrow("OutboxBlocked");
  await scheduler.deactivatePreservingDraft(a, async () => {});
  scheduler.resume();
  await vi.advanceTimersByTimeAsync(1000);
  expect(ports.transmit).toHaveBeenCalledTimes(1);
  const reopened = { ...a, editSessionId: "new-session" };
  scheduler.edit(reopened, "2"); scheduler.durable(reopened, "2");
  await scheduler.flushDocument(reopened);
  await vi.advanceTimersByTimeAsync(0);
  expect(ports.transmit).toHaveBeenCalledTimes(2);
  await scheduler.dispose();
});

test("a frozen old binding cannot block a new binding after draft-preserving close", async () => {
  const { ports, scheduler } = setup();
  scheduler.activateBinding(a.binding);
  scheduler.edit(a, "1"); scheduler.durable(a, "1");
  await scheduler.flushDocument(a);
  scheduler.composition(a, true);
  scheduler.edit(a, "2"); scheduler.composition(a, false); scheduler.durable(a, "2");
  vi.mocked(ports.queue).mockRejectedValueOnce(new Error("OutboxBlocked"));
  await expect(scheduler.deactivate(a, async () => {})).rejects.toThrow("OutboxBlocked");
  await scheduler.deactivatePreservingDraft(a, async () => {});
  const before = vi.mocked(ports.transmit).mock.calls.length;
  scheduler.activateBinding(b.binding);
  scheduler.edit(b, "1"); scheduler.durable(b, "1");
  await scheduler.flushDocument(b);
  await vi.advanceTimersByTimeAsync(0);
  expect(vi.mocked(ports.transmit).mock.calls.length).toBeGreaterThan(before);
  await scheduler.dispose();
});

test("changing binding ignores an old authentication response and continues the new worker", async () => {
  const { ports, scheduler } = setup();
  let resolve!: (result: { status: "auth_required" }) => void;
  vi.mocked(ports.transmit).mockReturnValueOnce(new Promise((done) => { resolve = done; })).mockResolvedValueOnce({ status: "idle" });
  scheduler.activateBinding(a.binding);
  scheduler.resume();
  await vi.advanceTimersByTimeAsync(0);
  scheduler.activateBinding(b.binding);
  resolve({ status: "auth_required" });
  await vi.advanceTimersByTimeAsync(0);
  expect(ports.authRequired).not.toHaveBeenCalled();
  expect(ports.transmit).toHaveBeenCalledTimes(2);
  await scheduler.dispose();
});

test("reaffirming the same binding cannot clear an authentication stop", async () => {
  const { ports, scheduler } = setup();
  vi.mocked(ports.transmit).mockResolvedValueOnce({ status: "auth_required" }).mockResolvedValueOnce({ status: "idle" });
  scheduler.activateBinding(a.binding);
  scheduler.resume();
  await vi.advanceTimersByTimeAsync(0);
  scheduler.activateBinding(a.binding);
  scheduler.kick();
  await vi.advanceTimersByTimeAsync(60_000);
  expect(ports.transmit).toHaveBeenCalledOnce();
  await scheduler.dispose();
});

test("an acknowledgement from a previous binding is ignored", async () => {
  const { ports, scheduler } = setup();
  scheduler.activateBinding(a.binding);
  scheduler.edit(a, "1"); scheduler.durable(a, "1");
  await scheduler.flushDocument(a);
  scheduler.activateBinding(b.binding);
  scheduler.acknowledge(a, { operationId: "op-a-1", documentId: "a", generation: "1" }, true);
  expect(ports.acknowledged).not.toHaveBeenCalled();
  await scheduler.dispose();
});

test("closing an unqueued composition draft does not block another document", async () => {
  const { ports, scheduler } = setup();
  scheduler.composition(a, true);
  scheduler.edit(a, "1"); scheduler.durable(a, "1");
  scheduler.composition(a, false);
  vi.mocked(ports.queue).mockRejectedValueOnce(new Error("OutboxBlocked"));
  await expect(scheduler.deactivate(a, async () => {})).rejects.toThrow("OutboxBlocked");
  await scheduler.deactivatePreservingDraft(a, async () => {});
  expect(ports.transmit).not.toHaveBeenCalled();
  scheduler.edit(b, "1"); scheduler.durable(b, "1");
  await scheduler.flushDocument(b);
  await vi.advanceTimersByTimeAsync(0);
  expect(ports.queue).toHaveBeenLastCalledWith(b, "1");
  expect(ports.transmit).toHaveBeenCalledOnce();
  await scheduler.dispose();
});

test("a delayed queue rejects a result for a different generation", async () => {
  const { ports, scheduler } = setup();
  let resolve!: (value: { operationId: string; documentId: string; generation: string }) => void;
  vi.mocked(ports.queue).mockReturnValueOnce(new Promise((done) => { resolve = done; }));
  scheduler.edit(a, "1"); scheduler.durable(a, "1");
  const flush = scheduler.flushDocument(a);
  await Promise.resolve();
  scheduler.edit(a, "2"); scheduler.durable(a, "2");
  resolve({ operationId: "op-a-2", documentId: "a", generation: "2" });
  await expect(flush).rejects.toThrow("invalid_queued_operation");
  expect(ports.queue).toHaveBeenCalledOnce();
  scheduler.acknowledge(a, { operationId: "op-a-2", documentId: "a", generation: "2" }, true);
  expect(ports.acknowledged).not.toHaveBeenCalled();
  await scheduler.flushDocument(a);
  expect(ports.queue).toHaveBeenLastCalledWith(a, "2");
  await scheduler.dispose();
});

test("a mismatched future generation is rejected even if its acknowledgement arrives", async () => {
  const { ports, scheduler } = setup();
  vi.mocked(ports.queue).mockResolvedValueOnce({ operationId: "op-a-2", documentId: "a", generation: "2" });
  scheduler.edit(a, "1"); scheduler.durable(a, "1");
  await expect(scheduler.flushDocument(a)).rejects.toThrow("invalid_queued_operation");
  scheduler.acknowledge(a, { operationId: "op-a-2", documentId: "a", generation: "2" }, true);
  expect(ports.acknowledged).not.toHaveBeenCalled();
  scheduler.edit(a, "2"); scheduler.durable(a, "2");
  await scheduler.flushDocument(a);
  scheduler.acknowledge(a, { operationId: "op-a-2", documentId: "a", generation: "2" }, true);
  expect(ports.acknowledged).toHaveBeenLastCalledWith(a, "2", true);
  await scheduler.dispose();
});

test("stale queue during IME composition retries only after the latest stable checkpoint", async () => {
  const { ports, scheduler } = setup();
  let reject!: (error: Error) => void;
  vi.mocked(ports.queue).mockReturnValueOnce(new Promise((_resolve, fail) => { reject = fail; }));
  scheduler.edit(a, "1"); scheduler.durable(a, "1");
  await vi.advanceTimersByTimeAsync(750);
  expect(ports.queue).toHaveBeenCalledExactlyOnceWith(a, "1");
  scheduler.composition(a, true);
  scheduler.edit(a, "2"); scheduler.durable(a, "2");
  reject(new Error("stale_checkpoint"));
  await vi.advanceTimersByTimeAsync(1000);
  expect(ports.queue).toHaveBeenCalledOnce();
  expect(ports.transmit).not.toHaveBeenCalled();
  expect(ports.failed).not.toHaveBeenCalled();
  scheduler.composition(a, false);
  await vi.advanceTimersByTimeAsync(0);
  expect(ports.queue).toHaveBeenLastCalledWith(a, "2");
  expect(ports.transmit).toHaveBeenCalledOnce();
  await scheduler.dispose();
});

test("stale queue during a switch retries the newer durable generation before closing", async () => {
  const { ports, scheduler } = setup();
  let reject!: (error: string) => void;
  vi.mocked(ports.queue).mockReturnValueOnce(new Promise((_resolve, fail) => { reject = fail; }));
  scheduler.edit(a, "1"); scheduler.durable(a, "1");
  const close = vi.fn(async () => {});
  const switching = scheduler.deactivate(a, close);
  await Promise.resolve();
  scheduler.edit(a, "2"); scheduler.durable(a, "2");
  reject("stale_checkpoint");
  await switching;
  expect(ports.queue).toHaveBeenCalledTimes(2);
  expect(ports.queue).toHaveBeenLastCalledWith(a, "2");
  expect(close).toHaveBeenCalledOnce();
  await scheduler.dispose();
});

test("timer-triggered queue failure does not retry in a tight loop", async () => {
  const { ports, scheduler } = setup();
  vi.mocked(ports.queue).mockRejectedValue(new Error("disk"));
  scheduler.edit(a, "1"); scheduler.durable(a, "1");
  await vi.advanceTimersByTimeAsync(750);
  expect(ports.queue).toHaveBeenCalledOnce();
  await vi.advanceTimersByTimeAsync(10_000);
  expect(ports.queue).toHaveBeenCalledOnce();
  expect(ports.failed).toHaveBeenCalledOnce();
  vi.mocked(ports.queue).mockResolvedValue({ operationId: "op-a-1", documentId: "a", generation: "1" });
  await scheduler.dispose();
});

test("a late acknowledgement never marks newer edits or another document synced", async () => {
  const { ports, scheduler } = setup();
  scheduler.edit(a, "1"); scheduler.durable(a, "1");
  await scheduler.flushDocument(a);
  scheduler.edit(a, "2");
  scheduler.acknowledge(a, { operationId: "op-a-1", documentId: "a", generation: "1" }, true);
  scheduler.acknowledge(b, { operationId: "op-a-1", documentId: "a", generation: "1" }, true);
  expect(ports.acknowledged).toHaveBeenNthCalledWith(1, a, "1", false);
  expect(ports.acknowledged).toHaveBeenCalledTimes(1);
  scheduler.durable(a, "2");
  await scheduler.flushDocument(a);
  scheduler.acknowledge(a, { operationId: "op-a-2", documentId: "a", generation: "2" }, true);
  expect(ports.acknowledged).toHaveBeenLastCalledWith(a, "2", true);
  await scheduler.dispose();
});

test("acknowledgements require the operation, document, generation and session to match", async () => {
  const { ports, scheduler } = setup();
  scheduler.edit(a, "1"); scheduler.durable(a, "1");
  await scheduler.flushDocument(a);
  const otherSession = { ...a, editSessionId: "replacement" };
  scheduler.acknowledge(a, { operationId: "unknown", documentId: "a", generation: "1" }, true);
  scheduler.acknowledge(a, { operationId: "op-a-1", documentId: "b", generation: "1" }, true);
  scheduler.acknowledge(a, { operationId: "op-a-1", documentId: "a", generation: "2" }, true);
  scheduler.acknowledge(otherSession, { operationId: "op-a-1", documentId: "a", generation: "1" }, true);
  expect(ports.acknowledged).not.toHaveBeenCalled();
  scheduler.acknowledge(a, { operationId: "op-a-1", documentId: "a", generation: "1" }, true);
  expect(ports.acknowledged).toHaveBeenCalledExactlyOnceWith(a, "1", true);
  await scheduler.dispose();
});

test("a late acknowledgement cannot mark a closed session as current", async () => {
  const { ports, scheduler } = setup();
  scheduler.edit(a, "1"); scheduler.durable(a, "1");
  await scheduler.deactivate(a);
  scheduler.acknowledge(a, { operationId: "op-a-1", documentId: "a", generation: "1" }, true);
  expect(ports.acknowledged).toHaveBeenCalledExactlyOnceWith(a, "1", false);
  await scheduler.dispose();
});

test("an ack_pending result stays conservative until durable outcome is confirmed", async () => {
  const { ports, scheduler } = setup();
  scheduler.edit(a, "1"); scheduler.durable(a, "1");
  await scheduler.flushDocument(a);
  const receipt = { operationId: "op-a-1", documentId: "a", generation: "1" };
  scheduler.acknowledge(a, receipt, false);
  expect(ports.acknowledged).toHaveBeenLastCalledWith(a, "1", false);
  scheduler.acknowledge(a, receipt, true);
  expect(ports.acknowledged).toHaveBeenLastCalledWith(a, "1", true);
  await scheduler.dispose();
});

test("composition that begins before a scheduled network call prevents the call", async () => {
  const { ports, scheduler } = setup();
  scheduler.resume();
  scheduler.composition(a, true);
  await vi.advanceTimersByTimeAsync(0);
  expect(ports.transmit).not.toHaveBeenCalled();
  scheduler.composition(a, false);
  await vi.advanceTimersByTimeAsync(0);
  expect(ports.transmit).toHaveBeenCalledOnce();
  await scheduler.dispose();
});

test("composition end waits for the latest checkpoint and refreshes the queued generation", async () => {
  const { ports, scheduler } = setup();
  scheduler.edit(a, "1"); scheduler.durable(a, "1");
  await scheduler.flushDocument(a);
  scheduler.composition(a, true);
  scheduler.edit(a, "2");
  scheduler.composition(a, false);
  await vi.advanceTimersByTimeAsync(1000);
  expect(ports.transmit).toHaveBeenCalledTimes(1);
  scheduler.durable(a, "2");
  await vi.advanceTimersByTimeAsync(0);
  expect(ports.queue).toHaveBeenLastCalledWith(a, "2");
  expect(ports.transmit).toHaveBeenCalledTimes(2);
  await scheduler.dispose();
});

test("resuming drains terminal operations until the outbox is idle", async () => {
  const { ports, scheduler } = setup();
  vi.mocked(ports.transmit)
    .mockResolvedValueOnce({ status: "committed", operationId: "op-1" })
    .mockResolvedValueOnce({ status: "abandoned", operationId: "op-2" })
    .mockResolvedValueOnce({ status: "idle" });
  scheduler.resume();
  await vi.advanceTimersByTimeAsync(0);
  expect(ports.transmit).toHaveBeenCalledTimes(3);
  await scheduler.dispose();
});

test.each(["pending", "offline", "ack_pending"] as const)("%s retries with backoff instead of a tight loop", async (status) => {
  const { ports, scheduler } = setup();
  vi.mocked(ports.transmit).mockResolvedValueOnce({ status }).mockResolvedValueOnce({ status: "idle" });
  scheduler.resume();
  await vi.advanceTimersByTimeAsync(999);
  expect(ports.transmit).toHaveBeenCalledOnce();
  await vi.advanceTimersByTimeAsync(1);
  expect(ports.transmit).toHaveBeenCalledTimes(2);
  await scheduler.dispose();
});

test("repeated pending results use increasing backoff and manual retry is immediate", async () => {
  const { ports, scheduler } = setup();
  vi.mocked(ports.transmit)
    .mockResolvedValueOnce({ status: "pending" })
    .mockResolvedValueOnce({ status: "pending" })
    .mockResolvedValueOnce({ status: "pending" })
    .mockResolvedValueOnce({ status: "idle" });
  scheduler.resume();
  await vi.advanceTimersByTimeAsync(1000);
  expect(ports.transmit).toHaveBeenCalledTimes(2);
  await vi.advanceTimersByTimeAsync(1999);
  expect(ports.transmit).toHaveBeenCalledTimes(2);
  await vi.advanceTimersByTimeAsync(1);
  expect(ports.transmit).toHaveBeenCalledTimes(3);
  scheduler.resume();
  await vi.advanceTimersByTimeAsync(0);
  expect(ports.transmit).toHaveBeenCalledTimes(4);
  await vi.advanceTimersByTimeAsync(10_000);
  expect(ports.transmit).toHaveBeenCalledTimes(4);
  await scheduler.dispose();
});

test("a terminal operation repeated by the worker stops draining", async () => {
  const { ports, scheduler } = setup();
  vi.mocked(ports.transmit).mockResolvedValue({ status: "committed", operationId: "same" });
  scheduler.resume();
  await vi.advanceTimersByTimeAsync(60_000);
  expect(ports.transmit).toHaveBeenCalledTimes(2);
  expect(ports.failed).toHaveBeenCalledOnce();
  await scheduler.dispose();
});

test("needs_review waits for an explicit retry", async () => {
  const { ports, scheduler } = setup();
  vi.mocked(ports.transmit).mockResolvedValueOnce({ status: "needs_review" }).mockResolvedValueOnce({ status: "idle" });
  scheduler.resume();
  await vi.advanceTimersByTimeAsync(60_000);
  expect(ports.transmit).toHaveBeenCalledOnce();
  scheduler.resume();
  await vi.advanceTimersByTimeAsync(0);
  expect(ports.transmit).toHaveBeenCalledTimes(2);
  await scheduler.dispose();
});

test.each(["needs_review", "auth_required", "gateway_auth_required"] as const)("kick preserves a %s stop until explicit resume", async (status) => {
  const { ports, scheduler } = setup();
  vi.mocked(ports.transmit).mockResolvedValueOnce({ status }).mockResolvedValueOnce({ status: "idle" });
  scheduler.resume();
  await vi.advanceTimersByTimeAsync(0);
  expect(ports.transmit).toHaveBeenCalledOnce();
  scheduler.kick();
  await vi.advanceTimersByTimeAsync(60_000);
  expect(ports.transmit).toHaveBeenCalledOnce();
  scheduler.resume();
  await vi.advanceTimersByTimeAsync(0);
  expect(ports.transmit).toHaveBeenCalledTimes(2);
  await scheduler.dispose();
});

test("kick requests a worker pass when no stop or retry is pending", async () => {
  const { ports, scheduler } = setup();
  scheduler.kick();
  await vi.advanceTimersByTimeAsync(0);
  expect(ports.transmit).toHaveBeenCalledOnce();
  await scheduler.dispose();
});

test("kick does not bypass a pending retry backoff", async () => {
  const { ports, scheduler } = setup();
  vi.mocked(ports.transmit).mockResolvedValueOnce({ status: "ack_pending" }).mockResolvedValueOnce({ status: "idle" });
  scheduler.resume();
  await vi.advanceTimersByTimeAsync(0);
  scheduler.kick();
  await vi.advanceTimersByTimeAsync(999);
  expect(ports.transmit).toHaveBeenCalledOnce();
  await vi.advanceTimersByTimeAsync(1);
  expect(ports.transmit).toHaveBeenCalledTimes(2);
  await scheduler.dispose();
});

test.each(["auth_required", "gateway_auth_required"] as const)("%s reports the correct action and waits for explicit retry", async (status) => {
  const { ports, scheduler } = setup();
  vi.mocked(ports.transmit).mockResolvedValueOnce({ status }).mockResolvedValueOnce({ status: "idle" });
  scheduler.resume();
  await vi.advanceTimersByTimeAsync(60_000);
  expect(ports.transmit).toHaveBeenCalledOnce();
  expect(ports.authRequired).toHaveBeenCalledExactlyOnceWith(status);
  expect(ports.failed).not.toHaveBeenCalled();
  scheduler.edit(a, "1"); scheduler.durable(a, "1");
  await scheduler.flushDocument(a);
  await vi.advanceTimersByTimeAsync(60_000);
  expect(ports.transmit).toHaveBeenCalledOnce();
  scheduler.resume();
  await vi.advanceTimersByTimeAsync(0);
  expect(ports.transmit).toHaveBeenCalledTimes(2);
  await scheduler.dispose();
});

test("an authentication result arriving after scheduler disposal is ignored", async () => {
  const { ports, scheduler } = setup();
  let resolve!: (result: { status: "auth_required" }) => void;
  vi.mocked(ports.transmit).mockReturnValueOnce(new Promise((done) => { resolve = done; }));
  scheduler.resume();
  await vi.advanceTimersByTimeAsync(0);
  await scheduler.dispose();
  resolve({ status: "auth_required" });
  await vi.advanceTimersByTimeAsync(0);
  expect(ports.authRequired).not.toHaveBeenCalled();
  expect(ports.failed).not.toHaveBeenCalled();
});

test("a document switch waits for the latest input to be durably checkpointed", async () => {
  const { ports, scheduler } = setup();
  scheduler.edit(a, "1"); scheduler.durable(a, "1");
  scheduler.edit(a, "2");
  await expect(scheduler.flushDocument(a)).rejects.toThrow("checkpoint_pending");
  expect(ports.queue).not.toHaveBeenCalled();
  scheduler.durable(a, "2");
  await scheduler.flushDocument(a);
  expect(ports.queue).toHaveBeenCalledExactlyOnceWith(a, "2");
  await scheduler.dispose();
});

test("transmission is single flight and a later queued generation triggers another pass", async () => {
  const { ports, scheduler } = setup();
  let finish!: () => void;
  vi.mocked(ports.transmit).mockReturnValueOnce(new Promise<{ status: "idle" }>((resolve) => { finish = () => resolve({ status: "idle" }); }));
  scheduler.edit(a, "1"); scheduler.durable(a, "1");
  await scheduler.flushDocument(a);
  scheduler.edit(b, "1"); scheduler.durable(b, "1");
  await scheduler.flushDocument(b);
  expect(ports.transmit).toHaveBeenCalledTimes(1);
  finish();
  await vi.advanceTimersByTimeAsync(0);
  expect(ports.transmit).toHaveBeenCalledTimes(2);
  await scheduler.dispose();
});

test("composition prevents queuing even when a checkpoint is durable", async () => {
  const { ports, scheduler } = setup();
  scheduler.composition(a, true);
  scheduler.edit(a, "1"); scheduler.durable(a, "1");
  await expect(scheduler.flushDocument(a)).rejects.toThrow("composition_in_progress");
  expect(ports.queue).not.toHaveBeenCalled();
  expect(ports.transmit).not.toHaveBeenCalled();
  scheduler.composition(a, false);
  await vi.advanceTimersByTimeAsync(750);
  expect(ports.queue).toHaveBeenCalledExactlyOnceWith(a, "1");
  expect(ports.transmit).toHaveBeenCalledOnce();
  await scheduler.dispose();
});
