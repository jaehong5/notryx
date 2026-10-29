import { afterEach, expect, test, vi } from "vitest";
import { AutoTransmit, QueueStorageError, type AutoTransmitPorts, type TransmitScope } from "./auto-transmit";

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
  const scheduler = new AutoTransmit(ports);
  scheduler.openSession(a);
  scheduler.openSession(b);
  return { ports, scheduler };
}

afterEach(() => vi.useRealTimers());

test("automatic transmission waits for one second of inactivity", async () => {
  const { ports, scheduler } = setup();
  scheduler.edit(a, "1"); scheduler.durable(a, "1");
  await vi.advanceTimersByTimeAsync(999);
  expect(ports.queue).not.toHaveBeenCalled();
  expect(ports.transmit).not.toHaveBeenCalled();
  await vi.advanceTimersByTimeAsync(1);
  expect(ports.queue).toHaveBeenCalledExactlyOnceWith(a, "1");
  expect(ports.transmit).toHaveBeenCalledOnce();
  await scheduler.dispose();
});

test("continuous typing never causes automatic server transmission at five seconds", async () => {
  const { ports, scheduler } = setup();
  scheduler.edit(a, "1"); scheduler.durable(a, "1");
  for (let generation = 2; generation <= 40; generation++) {
    await vi.advanceTimersByTimeAsync(200);
    scheduler.edit(a, String(generation)); scheduler.durable(a, String(generation));
  }
  expect(ports.queue).not.toHaveBeenCalled();
  expect(ports.transmit).not.toHaveBeenCalled();
  await vi.advanceTimersByTimeAsync(999);
  expect(ports.transmit).not.toHaveBeenCalled();
  await vi.advanceTimersByTimeAsync(1);
  expect(ports.queue).toHaveBeenCalledExactlyOnceWith(a, "40");
  expect(ports.transmit).toHaveBeenCalledOnce();
  await scheduler.dispose();
});

test("closing persists the operation without transmitting before the idle deadline", async () => {
  const { ports, scheduler } = setup();
  scheduler.edit(a, "1"); scheduler.durable(a, "1");
  await vi.advanceTimersByTimeAsync(400);
  await scheduler.deactivate(a);
  expect(ports.queue).toHaveBeenCalledExactlyOnceWith(a, "1");
  expect(ports.transmit).not.toHaveBeenCalled();
  await vi.advanceTimersByTimeAsync(599);
  expect(ports.transmit).not.toHaveBeenCalled();
  await vi.advanceTimersByTimeAsync(1);
  expect(ports.transmit).toHaveBeenCalledOnce();
  await scheduler.dispose();
});

test("a newer uncommitted edit holds an already queued operation until the latest checkpoint", async () => {
  const { ports, scheduler } = setup();
  scheduler.edit(a, "1"); scheduler.durable(a, "1");
  await scheduler.flushDocument(a);
  scheduler.edit(a, "2");
  await vi.advanceTimersByTimeAsync(1000);
  expect(ports.transmit).not.toHaveBeenCalled();
  scheduler.durable(a, "2");
  await vi.advanceTimersByTimeAsync(0);
  expect(ports.queue).toHaveBeenLastCalledWith(a, "2");
  expect(ports.transmit).toHaveBeenCalledOnce();
  await scheduler.dispose();
});

test("a newer edit during queue preparation prevents an early server request", async () => {
  const { ports, scheduler } = setup();
  let resolve!: (value: { operationId: string; documentId: string; generation: string }) => void;
  vi.mocked(ports.queue).mockReturnValueOnce(new Promise((done) => { resolve = done; }));
  scheduler.edit(a, "1"); scheduler.durable(a, "1");
  const preparing = scheduler.flushDocument(a);
  await Promise.resolve();
  await vi.advanceTimersByTimeAsync(400);
  scheduler.edit(a, "2"); scheduler.durable(a, "2");
  resolve({ operationId: "op-a-1", documentId: "a", generation: "1" });
  await preparing;
  expect(ports.queue).toHaveBeenLastCalledWith(a, "2");
  expect(ports.transmit).not.toHaveBeenCalled();
  await vi.advanceTimersByTimeAsync(999);
  expect(ports.transmit).not.toHaveBeenCalled();
  await vi.advanceTimersByTimeAsync(1);
  expect(ports.transmit).toHaveBeenCalledOnce();
  await scheduler.dispose();
});

test("an in-flight response cannot start the next operation while editing resumes", async () => {
  const { ports, scheduler } = setup();
  let resolve!: (value: { status: "committed"; operationId: string }) => void;
  vi.mocked(ports.transmit).mockReturnValueOnce(new Promise((done) => { resolve = done; })).mockResolvedValue({ status: "idle" });
  scheduler.edit(a, "1"); scheduler.durable(a, "1");
  await scheduler.syncNow(a);
  await vi.advanceTimersByTimeAsync(0);
  expect(ports.transmit).toHaveBeenCalledOnce();
  scheduler.edit(a, "2"); scheduler.durable(a, "2");
  scheduler.acknowledge(a, { operationId: "op-a-1", documentId: "a", generation: "1" }, true);
  expect(ports.acknowledged).toHaveBeenCalledExactlyOnceWith(a, "1", false);
  resolve({ status: "committed", operationId: "op-a-1" });
  await vi.advanceTimersByTimeAsync(999);
  expect(ports.transmit).toHaveBeenCalledOnce();
  await vi.advanceTimersByTimeAsync(1);
  expect(ports.queue).toHaveBeenLastCalledWith(a, "2");
  expect(ports.transmit).toHaveBeenCalledTimes(2);
  await scheduler.dispose();
});

test("restart waits one second before sending an already durable outbox", async () => {
  const { ports, scheduler } = setup();
  scheduler.resume();
  await vi.advanceTimersByTimeAsync(999);
  expect(ports.transmit).not.toHaveBeenCalled();
  await vi.advanceTimersByTimeAsync(1);
  expect(ports.transmit).toHaveBeenCalledOnce();
  await scheduler.dispose();
});

test("a backward wall-clock adjustment does not permanently block a pending outbox", async () => {
  const { ports, scheduler } = setup();
  scheduler.resume();
  vi.setSystemTime(-60_000);
  await vi.advanceTimersByTimeAsync(1000);
  expect(ports.transmit).toHaveBeenCalledOnce();
  await scheduler.dispose();
});

test("scope-less input and composition also pause automatic server requests", async () => {
  const { ports, scheduler } = setup();
  scheduler.resume();
  scheduler.input();
  scheduler.inputComposition(true);
  await vi.advanceTimersByTimeAsync(3000);
  expect(ports.transmit).not.toHaveBeenCalled();
  scheduler.inputComposition(false);
  await vi.advanceTimersByTimeAsync(0);
  expect(ports.transmit).toHaveBeenCalledOnce();
  scheduler.input();
  scheduler.kick();
  await vi.advanceTimersByTimeAsync(999);
  expect(ports.transmit).toHaveBeenCalledOnce();
  await vi.advanceTimersByTimeAsync(1);
  expect(ports.transmit).toHaveBeenCalledTimes(2);
  await scheduler.dispose();
});

test("scope-less edits hold the outbox until their latest checkpoint is durable", async () => {
  const { ports, scheduler } = setup();
  scheduler.resume();
  scheduler.unscopedEdit("recovery-session", "1");
  scheduler.unscopedEdit("recovery-session", "2");
  await vi.advanceTimersByTimeAsync(1000);
  expect(ports.transmit).not.toHaveBeenCalled();
  scheduler.unscopedDurable("recovery-session", "1");
  scheduler.unscopedDurable("old-session", "2");
  await vi.advanceTimersByTimeAsync(0);
  expect(ports.transmit).not.toHaveBeenCalled();
  scheduler.unscopedDurable("recovery-session", "2");
  await vi.advanceTimersByTimeAsync(0);
  expect(ports.transmit).toHaveBeenCalledOnce();
  await scheduler.dispose();
});

test("new input while explicit sync prepares its operation revokes the immediate send", async () => {
  const { ports, scheduler } = setup();
  let resolve!: (value: { operationId: string; documentId: string; generation: string }) => void;
  vi.mocked(ports.queue).mockReturnValueOnce(new Promise((done) => { resolve = done; }));
  scheduler.edit(a, "1"); scheduler.durable(a, "1");
  const syncing = scheduler.syncNow(a);
  await Promise.resolve();
  await vi.advanceTimersByTimeAsync(400);
  scheduler.edit(a, "2"); scheduler.durable(a, "2");
  resolve({ operationId: "op-a-1", documentId: "a", generation: "1" });
  await syncing;
  expect(ports.transmit).not.toHaveBeenCalled();
  await vi.advanceTimersByTimeAsync(999);
  expect(ports.transmit).not.toHaveBeenCalled();
  await vi.advanceTimersByTimeAsync(1);
  expect(ports.transmit).toHaveBeenCalledOnce();
  await scheduler.dispose();
});

test("queues only a durable generation after 1000 ms of inactivity", async () => {
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
  await vi.advanceTimersByTimeAsync(999);
  expect(ports.queue).toHaveBeenCalledTimes(1);
  await vi.advanceTimersByTimeAsync(1);
  expect(ports.queue).toHaveBeenLastCalledWith(a, "2");
  await scheduler.dispose();
});

test("restoring a durable checkpoint queues it without another edit", async () => {
  const { ports, scheduler } = setup();
  await scheduler.restore(a, "7");
  expect(ports.queue).toHaveBeenCalledExactlyOnceWith(a, "7");
  expect(ports.transmit).not.toHaveBeenCalled();
  await vi.advanceTimersByTimeAsync(1000);
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
  await vi.advanceTimersByTimeAsync(1000);
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

test("continuous durable edits are queued only after one second of inactivity", async () => {
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
  expect(ports.queue).not.toHaveBeenCalled();
  await vi.advanceTimersByTimeAsync(800);
  expect(ports.queue).toHaveBeenCalledExactlyOnceWith(a, "25");
  await scheduler.dispose();
});

test("continuous input waits for the latest checkpoint before sending", async () => {
  const { ports, scheduler } = setup();
  scheduler.edit(a, "1"); scheduler.durable(a, "1");
  for (let n = 2; n <= 25; n++) {
    await vi.advanceTimersByTimeAsync(200);
    scheduler.edit(a, String(n));
    if (n < 25) scheduler.durable(a, String(n));
  }
  await vi.advanceTimersByTimeAsync(200);
  expect(ports.queue).not.toHaveBeenCalled();
  expect(ports.transmit).not.toHaveBeenCalled();
  await vi.advanceTimersByTimeAsync(800);
  expect(ports.queue).not.toHaveBeenCalled();
  scheduler.durable(a, "25");
  await vi.advanceTimersByTimeAsync(0);
  expect(ports.queue).toHaveBeenCalledExactlyOnceWith(a, "25");
  expect(ports.transmit).toHaveBeenCalledOnce();
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
  const internal = scheduler as unknown as { documents: Map<string, unknown> };
  scheduler.edit(a, "1"); scheduler.durable(a, "1");
  await scheduler.flushDocument(a);
  scheduler.composition(a, true);
  scheduler.edit(a, "2"); scheduler.composition(a, false); scheduler.durable(a, "2");
  vi.mocked(ports.queue).mockRejectedValueOnce(new Error("OutboxBlocked"));
  await expect(scheduler.deactivate(a, async () => {})).rejects.toThrow("OutboxBlocked");
  await scheduler.deactivatePreservingDraft(a, async () => {});
  expect(internal.documents.size).toBe(2);
  expect(() => scheduler.edit(a, "3")).toThrow("stale_session");
  scheduler.resume();
  await vi.advanceTimersByTimeAsync(1000);
  expect(ports.transmit).not.toHaveBeenCalled();
  const reopened = { ...a, editSessionId: "new-session" };
  scheduler.openSession(reopened);
  scheduler.edit(reopened, "2"); scheduler.durable(reopened, "2");
  await scheduler.flushDocument(reopened);
  expect(internal.documents.size).toBe(2);
  await vi.advanceTimersByTimeAsync(1000);
  expect(ports.transmit).toHaveBeenCalledTimes(1);
  await scheduler.dispose();
});

test("queue failure preserves the draft and earlier outbox until the draft is reopened", async () => {
  const { ports, scheduler } = setup();
  const pending: string[] = [];
  const sent: string[] = [];
  const storageFailed = vi.fn();
  const recovered = vi.fn();
  Object.assign(ports, { queueStorageFailed: storageFailed, queueRecovered: recovered });
  vi.mocked(ports.queue).mockImplementation(async (scope, generation) => {
    if (generation === "2" && !storageFailed.mock.calls.length) throw "Storage";
    const operationId = `op-${generation}`;
    pending.push(operationId);
    return { operationId, documentId: scope.documentId, generation };
  });
  vi.mocked(ports.transmit).mockImplementation(async () => {
    const operationId = pending.shift();
    if (!operationId) return { status: "idle" };
    sent.push(operationId);
    return { status: "committed", operationId };
  });
  scheduler.edit(a, "1"); scheduler.durable(a, "1");
  await scheduler.flushDocument(a);
  scheduler.edit(a, "2"); scheduler.durable(a, "2");
  await expect(scheduler.deactivate(a)).rejects.toBeInstanceOf(QueueStorageError);
  expect(storageFailed).toHaveBeenCalledExactlyOnceWith(a);
  const preservedDraft = vi.fn(async () => {});
  await scheduler.deactivatePreservingDraft(a, preservedDraft);
  expect(preservedDraft).toHaveBeenCalledOnce();
  expect(sent).toEqual([]);
  await vi.advanceTimersByTimeAsync(1000);
  expect(sent).toEqual(["op-1"]);
  const reopened = { ...a, editSessionId: "reopened" };
  scheduler.openSession(reopened);
  await scheduler.restore(reopened, "2");
  expect(recovered).toHaveBeenLastCalledWith(reopened);
  await vi.advanceTimersByTimeAsync(0);
  expect(sent).toEqual(["op-1", "op-2"]);
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
  await vi.advanceTimersByTimeAsync(1000);
  expect(vi.mocked(ports.transmit).mock.calls.length).toBeGreaterThan(before);
  await scheduler.dispose();
});

test("changing binding ignores an old authentication response and continues the new worker", async () => {
  const { ports, scheduler } = setup();
  let resolve!: (result: { status: "auth_required" }) => void;
  vi.mocked(ports.transmit).mockReturnValueOnce(new Promise((done) => { resolve = done; })).mockResolvedValueOnce({ status: "idle" });
  scheduler.activateBinding(a.binding);
  scheduler.resume();
  await vi.advanceTimersByTimeAsync(1000);
  scheduler.activateBinding(b.binding);
  resolve({ status: "auth_required" });
  await vi.advanceTimersByTimeAsync(1000);
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
  await vi.advanceTimersByTimeAsync(1000);
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
  await vi.advanceTimersByTimeAsync(1000);
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
  await vi.advanceTimersByTimeAsync(1000);
  expect(ports.queue).toHaveBeenCalledOnce();
  await vi.advanceTimersByTimeAsync(10_000);
  expect(ports.queue).toHaveBeenCalledOnce();
  expect(ports.failed).toHaveBeenCalledOnce();
  vi.mocked(ports.queue).mockResolvedValue({ operationId: "op-a-1", documentId: "a", generation: "1" });
  await scheduler.dispose();
});

test("a transient queue storage failure retries the durable generation without another edit", async () => {
  const { ports, scheduler } = setup();
  vi.mocked(ports.queue).mockRejectedValueOnce("Storage");
  scheduler.activateBinding(a.binding);
  scheduler.edit(a, "1"); scheduler.durable(a, "1");
  await vi.advanceTimersByTimeAsync(1000);
  expect(ports.queue).toHaveBeenCalledTimes(1);
  expect(ports.failed).toHaveBeenCalledOnce();
  await vi.advanceTimersByTimeAsync(999);
  expect(ports.queue).toHaveBeenCalledTimes(1);
  await vi.advanceTimersByTimeAsync(1);
  expect(ports.queue).toHaveBeenCalledTimes(2);
  expect(ports.queue).toHaveBeenLastCalledWith(a, "1");
  expect(ports.transmit).toHaveBeenCalledOnce();
  await scheduler.dispose();
});

test("a transient queue storage failure pauses for composition and resumes afterwards", async () => {
  const { ports, scheduler } = setup();
  vi.mocked(ports.queue).mockRejectedValueOnce("Storage");
  scheduler.activateBinding(a.binding);
  scheduler.edit(a, "1"); scheduler.durable(a, "1");
  await vi.advanceTimersByTimeAsync(1000);
  scheduler.composition(a, true);
  await vi.advanceTimersByTimeAsync(5_000);
  expect(ports.queue).toHaveBeenCalledOnce();
  scheduler.composition(a, false);
  await vi.advanceTimersByTimeAsync(1000);
  expect(ports.queue).toHaveBeenCalledTimes(2);
  await scheduler.dispose();
});

test("repeated queue storage failures use bounded backoff rather than a tight loop", async () => {
  const { ports, scheduler } = setup();
  vi.mocked(ports.queue).mockRejectedValueOnce("Storage").mockRejectedValueOnce("Storage");
  scheduler.activateBinding(a.binding);
  scheduler.edit(a, "1"); scheduler.durable(a, "1");
  await vi.advanceTimersByTimeAsync(1000);
  expect(ports.queue).toHaveBeenCalledTimes(1);
  await vi.advanceTimersByTimeAsync(1000);
  expect(ports.queue).toHaveBeenCalledTimes(2);
  await vi.advanceTimersByTimeAsync(1999);
  expect(ports.queue).toHaveBeenCalledTimes(2);
  await vi.advanceTimersByTimeAsync(1);
  expect(ports.queue).toHaveBeenCalledTimes(3);
  await scheduler.dispose();
});

test("closing after a transient queue failure cancels its scheduled retry", async () => {
  const { ports, scheduler } = setup();
  vi.mocked(ports.queue).mockRejectedValueOnce("Storage");
  scheduler.activateBinding(a.binding);
  scheduler.edit(a, "1"); scheduler.durable(a, "1");
  await vi.advanceTimersByTimeAsync(1000);
  await scheduler.deactivatePreservingDraft(a, async () => {});
  await vi.advanceTimersByTimeAsync(60_000);
  expect(ports.queue).toHaveBeenCalledOnce();
  await scheduler.dispose();
});

test("queue reconciliation errors do not retry automatically", async () => {
  const { ports, scheduler } = setup();
  vi.mocked(ports.queue).mockRejectedValueOnce("ReconciliationRequired");
  scheduler.activateBinding(a.binding);
  scheduler.edit(a, "1"); scheduler.durable(a, "1");
  await vi.advanceTimersByTimeAsync(60_000);
  expect(ports.queue).toHaveBeenCalledOnce();
  expect(ports.failed).toHaveBeenCalledOnce();
  await scheduler.dispose();
});

test("explicit sync retries a failed durable queue without waiting for its timer", async () => {
  const { ports, scheduler } = setup();
  vi.mocked(ports.queue).mockRejectedValueOnce("Storage");
  scheduler.activateBinding(a.binding);
  scheduler.edit(a, "1"); scheduler.durable(a, "1");
  await vi.advanceTimersByTimeAsync(1000);
  expect(ports.queue).toHaveBeenCalledOnce();
  await scheduler.syncNow(a);
  expect(ports.queue).toHaveBeenCalledTimes(2);
  expect(ports.transmit).toHaveBeenCalledOnce();
  await scheduler.dispose();
});

test("explicit sync expedites a network retry without clearing an authentication stop", async () => {
  const { ports, scheduler } = setup();
  scheduler.activateBinding(a.binding);
  vi.mocked(ports.transmit)
    .mockResolvedValueOnce({ status: "pending" })
    .mockResolvedValueOnce({ status: "auth_required" })
    .mockResolvedValueOnce({ status: "idle" });
  scheduler.edit(a, "1"); scheduler.durable(a, "1");
  await scheduler.flushDocument(a);
  await vi.advanceTimersByTimeAsync(1000);
  expect(ports.transmit).toHaveBeenCalledOnce();
  await scheduler.syncNow(a);
  await vi.advanceTimersByTimeAsync(0);
  expect(ports.transmit).toHaveBeenCalledTimes(2);
  expect(ports.authRequired).toHaveBeenCalledExactlyOnceWith("auth_required");
  await scheduler.syncNow(a);
  await vi.advanceTimersByTimeAsync(30_000);
  expect(ports.transmit).toHaveBeenCalledTimes(2);
  await scheduler.dispose();
});

test("explicit sync rejects an owner whose binding changes while flushing", async () => {
  const { ports, scheduler } = setup();
  scheduler.activateBinding(a.binding);
  const syncing = scheduler.syncNow(a);
  scheduler.activateBinding(b.binding);
  await expect(syncing).rejects.toThrow("stale_registration");
  await vi.advanceTimersByTimeAsync(1000);
  expect(ports.transmit).toHaveBeenCalledOnce();
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

test("successful queue reports its scope after Storage failure and subsequent success", async () => {
  const { ports, scheduler } = setup();
  const recovered = vi.fn();
  const failed = vi.fn();
  Object.assign(ports, { queueRecovered: recovered, queueStorageFailed: failed });
  vi.mocked(ports.queue).mockRejectedValueOnce("Storage");
  scheduler.edit(a, "1"); scheduler.durable(a, "1");
  await vi.advanceTimersByTimeAsync(1000);
  expect(failed).toHaveBeenCalledExactlyOnceWith(a);
  expect(vi.mocked(ports.failed).mock.calls[0][0]).toBeInstanceOf(QueueStorageError);
  expect(recovered).not.toHaveBeenCalled();
  await vi.advanceTimersByTimeAsync(1000);
  expect(recovered).toHaveBeenCalledExactlyOnceWith(a);
  scheduler.edit(a, "2"); scheduler.durable(a, "2");
  await scheduler.flushDocument(a);
  expect(recovered).toHaveBeenCalledTimes(2);
  await scheduler.dispose();
});

test("reopening a failed draft reports queue success for its new session", async () => {
  const { ports, scheduler } = setup();
  const recovered = vi.fn();
  Object.assign(ports, { queueRecovered: recovered });
  vi.mocked(ports.queue).mockRejectedValueOnce("Storage");
  scheduler.edit(a, "1"); scheduler.durable(a, "1");
  await vi.advanceTimersByTimeAsync(1000);
  await scheduler.deactivatePreservingDraft(a, async () => {});
  const reopened = { ...a, editSessionId: "reopened" };
  scheduler.openSession(reopened);
  await scheduler.restore(reopened, "1");
  expect(recovered).toHaveBeenCalledExactlyOnceWith(reopened);
  await scheduler.dispose();
});

test("idle after ack_pending retires only the identified receipt without assuming commit", async () => {
  const { ports, scheduler } = setup();
  const retired = vi.fn();
  Object.assign(ports, { receiptConfirmed: retired });
  vi.mocked(ports.transmit).mockResolvedValueOnce({ status: "ack_pending", operationId: "op-a-1" });
  scheduler.activateBinding(a.binding);
  scheduler.edit(a, "1"); scheduler.durable(a, "1");
  await scheduler.flushDocument(a);
  await vi.advanceTimersByTimeAsync(1000);
  const internal = scheduler as unknown as { operations: Map<string, unknown> };
  expect(internal.operations.size).toBe(1);
  expect(retired).not.toHaveBeenCalled();
  scheduler.edit(a, "2"); scheduler.durable(a, "2");
  await scheduler.flushDocument(a);
  expect(internal.operations.size).toBe(2);
  await vi.advanceTimersByTimeAsync(1000);
  expect(retired).toHaveBeenCalledExactlyOnceWith("op-a-1");
  expect(internal.operations.size).toBe(1);
  expect(ports.acknowledged).not.toHaveBeenCalled();
  scheduler.acknowledge(a, { operationId: "op-a-2", documentId: "a", generation: "2" }, true);
  expect(ports.acknowledged).toHaveBeenCalledExactlyOnceWith(a, "2", true);
  await scheduler.dispose();
});

test("unidentified ack_pending and authentication failure do not retire an operation", async () => {
  const { ports, scheduler } = setup();
  const retired = vi.fn();
  Object.assign(ports, { receiptConfirmed: retired });
  vi.mocked(ports.transmit)
    .mockResolvedValueOnce({ status: "ack_pending" })
    .mockResolvedValueOnce({ status: "auth_required" });
  scheduler.activateBinding(a.binding);
  scheduler.edit(a, "1"); scheduler.durable(a, "1");
  await scheduler.flushDocument(a);
  await vi.advanceTimersByTimeAsync(1000);
  await vi.advanceTimersByTimeAsync(1000);
  expect(retired).not.toHaveBeenCalled();
  expect(ports.authRequired).toHaveBeenCalledExactlyOnceWith("auth_required");
  scheduler.acknowledge(a, { operationId: "op-a-1", documentId: "a", generation: "1" }, true);
  expect(ports.acknowledged).toHaveBeenCalledExactlyOnceWith(a, "1", true);
  await scheduler.dispose();
});

test.each(["auth_required", "gateway_auth_required"] as const)("%s with an operation retires only after authenticated idle", async (status) => {
  const { ports, scheduler } = setup();
  const retired = vi.fn();
  Object.assign(ports, { receiptConfirmed: retired });
  vi.mocked(ports.transmit).mockResolvedValueOnce({ status, operationId: "op-a-1" });
  scheduler.activateBinding(a.binding);
  scheduler.edit(a, "1"); scheduler.durable(a, "1");
  await scheduler.flushDocument(a);
  await vi.advanceTimersByTimeAsync(1000);
  expect(ports.authRequired).toHaveBeenCalledExactlyOnceWith(status);
  expect(retired).not.toHaveBeenCalled();
  scheduler.resume();
  await vi.advanceTimersByTimeAsync(0);
  expect(retired).toHaveBeenCalledExactlyOnceWith("op-a-1");
  expect(ports.acknowledged).not.toHaveBeenCalled();
  await scheduler.dispose();
});

test("authentication failure before receipt never retires an operation while retry remains pending", async () => {
  const { ports, scheduler } = setup();
  const retired = vi.fn();
  Object.assign(ports, { receiptConfirmed: retired });
  vi.mocked(ports.transmit)
    .mockResolvedValueOnce({ status: "auth_required", operationId: "op-a-1" })
    .mockResolvedValueOnce({ status: "pending" });
  scheduler.activateBinding(a.binding);
  scheduler.edit(a, "1"); scheduler.durable(a, "1");
  await scheduler.flushDocument(a);
  await vi.advanceTimersByTimeAsync(0);
  scheduler.resume();
  await vi.advanceTimersByTimeAsync(0);
  expect(retired).not.toHaveBeenCalled();
  scheduler.acknowledge(a, { operationId: "op-a-1", documentId: "a", generation: "1" }, true);
  expect(ports.acknowledged).toHaveBeenCalledExactlyOnceWith(a, "1", true);
  await scheduler.dispose();
});

test("switching binding releases closed state and old operation tracking", async () => {
  const { scheduler } = setup();
  const internal = scheduler as unknown as { documents: Map<string, unknown>; operations: Map<string, unknown> };
  scheduler.activateBinding(a.binding);
  scheduler.edit(a, "1"); scheduler.durable(a, "1");
  await scheduler.flushDocument(a);
  scheduler.composition(a, true);
  scheduler.edit(a, "2"); scheduler.durable(a, "2"); scheduler.composition(a, false);
  await scheduler.deactivatePreservingDraft(a, async () => {});
  expect(internal.documents.size).toBe(2);
  expect(internal.operations.size).toBe(1);
  scheduler.activateBinding(b.binding);
  expect(internal.documents.size).toBe(1);
  expect(internal.operations.size).toBe(0);
  await scheduler.dispose();
});

test("explicit sync preserves the queue Storage error type and its cause", async () => {
  const { ports, scheduler } = setup();
  const failed = vi.fn();
  Object.assign(ports, { queueStorageFailed: failed });
  vi.mocked(ports.queue).mockRejectedValueOnce("Storage");
  scheduler.edit(a, "1"); scheduler.durable(a, "1");
  const error = await scheduler.syncNow(a).catch((cause: unknown) => cause);
  expect(error).toBeInstanceOf(QueueStorageError);
  expect((error as Error).cause).toBe("Storage");
  expect(failed).toHaveBeenCalledExactlyOnceWith(a);
  expect(ports.failed).not.toHaveBeenCalled();
  await vi.advanceTimersByTimeAsync(1000);
  await scheduler.dispose();
});

test("closed sessions are reclaimed without accepting old edits or dropping a late ack", async () => {
  const { ports, scheduler } = setup();
  const internal = scheduler as unknown as { documents: Map<string, unknown>; operations: Map<string, unknown> };
  scheduler.edit(a, "1"); scheduler.durable(a, "1");
  await scheduler.deactivate(a);
  expect(internal.documents.size).toBe(1);
  expect(internal.operations.size).toBe(1);
  expect(() => scheduler.edit(a, "2")).toThrow("stale_session");
  const receipt = { operationId: "op-a-1", documentId: "a", generation: "1" };
  scheduler.acknowledge(a, receipt, false);
  expect(internal.operations.size).toBe(1);
  scheduler.acknowledge(a, receipt, true);
  expect(ports.acknowledged).toHaveBeenLastCalledWith(a, "1", false);
  expect(internal.operations.size).toBe(0);
  await scheduler.dispose();
});

test("repeated document sessions release confirmed operations and reject old edits", async () => {
  const { ports, scheduler } = setup();
  const internal = scheduler as unknown as { documents: Map<string, unknown>; operations: Map<string, unknown> };
  for (let i = 0; i < 20; i++) {
    const scope = { ...a, editSessionId: `repeat-${i}` };
    scheduler.openSession(scope);
    scheduler.edit(scope, "1"); scheduler.durable(scope, "1");
    await scheduler.deactivate(scope);
    expect(() => scheduler.edit(scope, "2")).toThrow("stale_session");
    scheduler.acknowledge(scope, { operationId: "op-a-1", documentId: "a", generation: "1" }, true);
  }
  expect(internal.documents.size).toBe(2);
  expect(internal.operations.size).toBe(0);
  expect(ports.acknowledged).toHaveBeenCalledTimes(20);
  await scheduler.dispose();
});

test("terminal non-commit results release only their resolved operation", async () => {
  const { ports, scheduler } = setup();
  const internal = scheduler as unknown as { operations: Map<string, unknown> };
  vi.mocked(ports.transmit).mockResolvedValueOnce({ status: "conflict", operationId: "op-a-1" });
  scheduler.edit(a, "1"); scheduler.durable(a, "1");
  await scheduler.flushDocument(a);
  await vi.advanceTimersByTimeAsync(1000);
  expect(internal.operations.size).toBe(0);
  await scheduler.dispose();
});

test("composition that begins before a scheduled network call prevents the call", async () => {
  const { ports, scheduler } = setup();
  scheduler.resume();
  scheduler.composition(a, true);
  await vi.advanceTimersByTimeAsync(0);
  expect(ports.transmit).not.toHaveBeenCalled();
  scheduler.composition(a, false);
  await vi.advanceTimersByTimeAsync(1000);
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
  expect(ports.transmit).not.toHaveBeenCalled();
  scheduler.durable(a, "2");
  await vi.advanceTimersByTimeAsync(0);
  expect(ports.queue).toHaveBeenLastCalledWith(a, "2");
  expect(ports.transmit).toHaveBeenCalledTimes(1);
  await scheduler.dispose();
});

test("resuming drains terminal operations until the outbox is idle", async () => {
  const { ports, scheduler } = setup();
  vi.mocked(ports.transmit)
    .mockResolvedValueOnce({ status: "committed", operationId: "op-1" })
    .mockResolvedValueOnce({ status: "abandoned", operationId: "op-2" })
    .mockResolvedValueOnce({ status: "idle" });
  scheduler.resume();
  await vi.advanceTimersByTimeAsync(1000);
  expect(ports.transmit).toHaveBeenCalledTimes(3);
  await scheduler.dispose();
});

test.each(["pending", "offline", "ack_pending"] as const)("%s retries with backoff instead of a tight loop", async (status) => {
  const { ports, scheduler } = setup();
  vi.mocked(ports.transmit).mockResolvedValueOnce({ status }).mockResolvedValueOnce({ status: "idle" });
  scheduler.resume();
  await vi.advanceTimersByTimeAsync(1000);
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

test("a transmission stop keeps manual saves and document closing available", async () => {
  const { ports, scheduler } = setup();
  vi.mocked(ports.transmit).mockResolvedValueOnce({ status: "needs_review" }).mockResolvedValueOnce({ status: "idle" });
  scheduler.edit(a, "1"); scheduler.durable(a, "1");
  await vi.advanceTimersByTimeAsync(1000);
  expect(ports.transmit).toHaveBeenCalledOnce();
  scheduler.edit(a, "2"); scheduler.durable(a, "2");
  await scheduler.syncNow(a);
  expect(ports.queue).toHaveBeenLastCalledWith(a, "2");
  await scheduler.deactivate(a);
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
  await vi.advanceTimersByTimeAsync(1000);
  expect(ports.transmit).toHaveBeenCalledOnce();
  scheduler.kick();
  await vi.advanceTimersByTimeAsync(60_000);
  expect(ports.transmit).toHaveBeenCalledOnce();
  scheduler.resume();
  await vi.advanceTimersByTimeAsync(0);
  expect(ports.transmit).toHaveBeenCalledTimes(2);
  await scheduler.dispose();
});

test("a resume during an in-flight request is not lost to a late review response", async () => {
  const { ports, scheduler } = setup();
  let release!: (value: { status: "needs_review" }) => void;
  vi.mocked(ports.transmit)
    .mockImplementationOnce(() => new Promise((resolve) => { release = resolve; }))
    .mockResolvedValueOnce({ status: "idle" });
  scheduler.resume();
  await vi.advanceTimersByTimeAsync(1000);
  expect(ports.transmit).toHaveBeenCalledOnce();
  scheduler.resume();
  release({ status: "needs_review" });
  await vi.advanceTimersByTimeAsync(0);
  expect(ports.transmit).toHaveBeenCalledTimes(2);
  await scheduler.dispose();
});

test("sync readiness resumes initial synchronization waiting", async () => {
  const { ports, scheduler } = setup();
  vi.mocked(ports.transmit).mockResolvedValueOnce({ status: "sync_pending" }).mockResolvedValueOnce({ status: "idle" });
  scheduler.resume();
  await vi.advanceTimersByTimeAsync(1000);
  expect(ports.transmit).toHaveBeenCalledOnce();
  scheduler.resumeSync();
  await vi.advanceTimersByTimeAsync(0);
  expect(ports.transmit).toHaveBeenCalledTimes(2);
  await scheduler.dispose();
});

test("sync readiness does not clear an operation requiring review", async () => {
  const { ports, scheduler } = setup();
  vi.mocked(ports.transmit).mockResolvedValueOnce({ status: "needs_review" }).mockResolvedValueOnce({ status: "idle" });
  scheduler.resume();
  await vi.advanceTimersByTimeAsync(1000);
  expect(ports.transmit).toHaveBeenCalledOnce();
  scheduler.resumeSync();
  await vi.advanceTimersByTimeAsync(0);
  expect(ports.transmit).toHaveBeenCalledOnce();
  await scheduler.dispose();
});

test("sync readiness during a request survives a late initial-sync wait response", async () => {
  const { ports, scheduler } = setup();
  let release!: (value: { status: "sync_pending" }) => void;
  vi.mocked(ports.transmit).mockImplementationOnce(() => new Promise((resolve) => { release = resolve; })).mockResolvedValueOnce({ status: "idle" });
  scheduler.resume();
  await vi.advanceTimersByTimeAsync(1000);
  scheduler.resumeSync();
  release({ status: "sync_pending" });
  await vi.advanceTimersByTimeAsync(0);
  expect(ports.transmit).toHaveBeenCalledTimes(2);
  await scheduler.dispose();
});

test("a new review response is not cleared by an earlier sync readiness signal", async () => {
  const { ports, scheduler } = setup();
  let release!: (value: { status: "needs_review" }) => void;
  vi.mocked(ports.transmit).mockImplementationOnce(() => new Promise((resolve) => { release = resolve; })).mockResolvedValueOnce({ status: "idle" });
  scheduler.resume();
  await vi.advanceTimersByTimeAsync(1000);
  scheduler.resumeSync();
  release({ status: "needs_review" });
  await vi.advanceTimersByTimeAsync(0);
  expect(ports.transmit).toHaveBeenCalledOnce();
  scheduler.resumeSync();
  await vi.advanceTimersByTimeAsync(0);
  expect(ports.transmit).toHaveBeenCalledOnce();
  await scheduler.dispose();
});

test("kick requests a worker pass when no stop or retry is pending", async () => {
  const { ports, scheduler } = setup();
  scheduler.kick();
  await vi.advanceTimersByTimeAsync(1000);
  expect(ports.transmit).toHaveBeenCalledOnce();
  await scheduler.dispose();
});

test("kick does not bypass a pending retry backoff", async () => {
  const { ports, scheduler } = setup();
  vi.mocked(ports.transmit).mockResolvedValueOnce({ status: "ack_pending" }).mockResolvedValueOnce({ status: "idle" });
  scheduler.resume();
  await vi.advanceTimersByTimeAsync(1000);
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
  await vi.advanceTimersByTimeAsync(1000);
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
  await scheduler.syncNow(a);
  scheduler.edit(b, "1"); scheduler.durable(b, "1");
  await scheduler.flushDocument(b);
  expect(ports.transmit).toHaveBeenCalledTimes(1);
  finish();
  await vi.advanceTimersByTimeAsync(999);
  expect(ports.transmit).toHaveBeenCalledTimes(1);
  await vi.advanceTimersByTimeAsync(1);
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
  await vi.advanceTimersByTimeAsync(1000);
  expect(ports.queue).toHaveBeenCalledExactlyOnceWith(a, "1");
  expect(ports.transmit).toHaveBeenCalledOnce();
  await scheduler.dispose();
});
