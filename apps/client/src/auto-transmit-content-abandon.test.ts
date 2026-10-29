import { afterEach, expect, test, vi } from "vitest";
import { AutoTransmit, type AutoTransmitPorts } from "./auto-transmit";

const binding = "binding-a";
const blocked = { status: "content_blocked" as const, binding, operationId: "followup-2", documentId: "document-a" };

function setup() {
  vi.useFakeTimers();
  vi.setSystemTime(0);
  const ports: AutoTransmitPorts = {
    queue: vi.fn(async (scope, generation) => ({ operationId: `new-${scope.documentId}`, documentId: scope.documentId, generation })),
    transmit: vi.fn(async () => ({ status: "idle" as const })),
    acknowledged: vi.fn(), authRequired: vi.fn(), failed: vi.fn(),
  };
  const scheduler = new AutoTransmit(ports);
  scheduler.activateBinding(binding);
  return { scheduler, ports };
}

afterEach(() => vi.useRealTimers());

test("only the approved FIFO followup resumes a matching content block", async () => {
  const { scheduler, ports } = setup();
  vi.mocked(ports.transmit)
    .mockResolvedValueOnce(blocked)
    .mockResolvedValueOnce({ status: "abandoned", operationId: "followup-2" })
    .mockResolvedValueOnce({ status: "committed", operationId: "other-3" })
    .mockResolvedValueOnce({ status: "idle" });
  scheduler.resume();
  await vi.advanceTimersByTimeAsync(1000);
  expect(ports.transmit).toHaveBeenCalledTimes(1);
  expect(scheduler.resumeContentAfterAbandon("binding-b", "followup-2")).toBe(false);
  expect(scheduler.resumeContentAfterAbandon(binding, "original-412")).toBe(false);
  expect(scheduler.resumeContentAfterAbandon(binding, "other-3")).toBe(false);
  scheduler.kick();
  await vi.advanceTimersByTimeAsync(0);
  expect(ports.transmit).toHaveBeenCalledTimes(1);
  expect(scheduler.resumeContentAfterAbandon(binding, "followup-2")).toBe(true);
  await vi.advanceTimersByTimeAsync(0);
  expect(ports.transmit).toHaveBeenCalledTimes(4);
  expect(ports.queue).not.toHaveBeenCalled();
  expect(ports.failed).not.toHaveBeenCalled();
  expect(scheduler.resumeContentAfterAbandon(binding, "followup-2")).toBe(false);
  await vi.advanceTimersByTimeAsync(2000);
  expect(ports.transmit).toHaveBeenCalledTimes(4);
  await scheduler.dispose();
});

test("matching approval during an in-flight blocked response is not lost", async () => {
  const { scheduler, ports } = setup();
  let release!: (value: typeof blocked) => void;
  vi.mocked(ports.transmit)
    .mockImplementationOnce(() => new Promise((resolve) => { release = resolve; }))
    .mockResolvedValueOnce({ status: "abandoned", operationId: "followup-2" })
    .mockResolvedValueOnce({ status: "idle" });
  scheduler.resume();
  await vi.advanceTimersByTimeAsync(1000);
  expect(ports.transmit).toHaveBeenCalledTimes(1);
  expect(scheduler.resumeContentAfterAbandon(binding, "followup-2")).toBe(true);
  scheduler.resumeContentAfterAbandon(binding, "followup-2");
  release(blocked);
  await vi.advanceTimersByTimeAsync(0);
  expect(ports.transmit).toHaveBeenCalledTimes(3);
  expect(ports.failed).not.toHaveBeenCalled();
  await scheduler.dispose();
});

test.each([
  ["binding-b", "followup-2"],
  [binding, "another-followup"],
] as const)("unrelated in-flight approval %s %s cannot clear a later block", async (approvedBinding, approvedId) => {
  const { scheduler, ports } = setup();
  let release!: (value: typeof blocked) => void;
  vi.mocked(ports.transmit)
    .mockImplementationOnce(() => new Promise((resolve) => { release = resolve; }))
    .mockResolvedValueOnce({ status: "idle" });
  scheduler.resume();
  await vi.advanceTimersByTimeAsync(1000);
  scheduler.resumeContentAfterAbandon(approvedBinding, approvedId);
  release(blocked);
  await vi.advanceTimersByTimeAsync(0);
  expect(ports.transmit).toHaveBeenCalledTimes(1);
  expect(scheduler.resumeContentAfterAbandon(binding, "followup-2")).toBe(true);
  await vi.advanceTimersByTimeAsync(0);
  expect(ports.transmit).toHaveBeenCalledTimes(2);
  await scheduler.dispose();
});

test("a restarted scheduler can resume after first observing its durable blocked head", async () => {
  const { scheduler, ports } = setup();
  vi.mocked(ports.transmit)
    .mockResolvedValueOnce(blocked)
    .mockResolvedValueOnce({ status: "abandoned", operationId: "followup-2" })
    .mockResolvedValueOnce({ status: "idle" });
  scheduler.resume();
  await vi.advanceTimersByTimeAsync(1000);
  expect(ports.transmit).toHaveBeenCalledTimes(1);
  expect(scheduler.resumeContentAfterAbandon(binding, "followup-2")).toBe(true);
  await vi.advanceTimersByTimeAsync(0);
  expect(ports.transmit).toHaveBeenCalledTimes(3);
  await scheduler.dispose();
});

test("a blocked response from a replaced binding cannot block or resume the new binding", async () => {
  const { scheduler, ports } = setup();
  let release!: (value: typeof blocked) => void;
  vi.mocked(ports.transmit)
    .mockImplementationOnce(() => new Promise((resolve) => { release = resolve; }))
    .mockResolvedValueOnce({ status: "idle" });
  scheduler.resume();
  await vi.advanceTimersByTimeAsync(1000);
  scheduler.resumeContentAfterAbandon(binding, "followup-2");
  scheduler.activateBinding("binding-b");
  release(blocked);
  await vi.advanceTimersByTimeAsync(1000);
  expect(ports.transmit).toHaveBeenCalledTimes(2);
  expect(scheduler.resumeContentAfterAbandon(binding, "followup-2")).toBe(false);
  expect(scheduler.resumeContentAfterAbandon("binding-b", "followup-2")).toBe(false);
  expect(ports.failed).not.toHaveBeenCalled();
  await scheduler.dispose();
});

test.each([
  { status: "needs_review" as const },
  { status: "auth_required" as const },
  { status: "gateway_auth_required" as const },
  { status: "permission_denied" as const, operationId: "followup-2" },
  { status: "sync_pending" as const },
])("content approval does not clear unrelated $status stops", async (stop) => {
  const { scheduler, ports } = setup();
  vi.mocked(ports.transmit).mockResolvedValueOnce(stop).mockResolvedValueOnce({ status: "idle" });
  scheduler.resume();
  await vi.advanceTimersByTimeAsync(1000);
  expect(ports.transmit).toHaveBeenCalledTimes(1);
  expect(scheduler.resumeContentAfterAbandon(binding, "followup-2")).toBe(false);
  scheduler.kick();
  await vi.advanceTimersByTimeAsync(0);
  expect(ports.transmit).toHaveBeenCalledTimes(1);
  await scheduler.dispose();
});

test("in-flight approval cannot clear a subsequently reported authentication stop", async () => {
  const { scheduler, ports } = setup();
  let release!: (value: { status: "auth_required" }) => void;
  vi.mocked(ports.transmit)
    .mockImplementationOnce(() => new Promise((resolve) => { release = resolve; }))
    .mockResolvedValueOnce({ status: "idle" });
  scheduler.resume();
  await vi.advanceTimersByTimeAsync(1000);
  scheduler.resumeContentAfterAbandon(binding, "followup-2");
  release({ status: "auth_required" });
  await vi.advanceTimersByTimeAsync(0);
  scheduler.kick();
  await vi.advanceTimersByTimeAsync(0);
  expect(ports.transmit).toHaveBeenCalledTimes(1);
  expect(ports.authRequired).toHaveBeenCalledExactlyOnceWith("auth_required");
  await scheduler.dispose();
});
