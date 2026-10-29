import { afterEach, expect, test, vi } from "vitest";
import { AutoTransmit, type AutoTransmitPorts, type TransmitScope } from "./auto-transmit";

afterEach(() => vi.useRealTimers());

test("조합 후 미전송 checkpoint가 있는 충돌 session을 비활성화해도 다른 문서 전송은 진행한다", async () => {
  vi.useFakeTimers();
  vi.setSystemTime(0);
  const first: TransmitScope = { binding: "binding", documentId: "first", editSessionId: "session-first" };
  const other: TransmitScope = { binding: "binding", documentId: "other", editSessionId: "session-other" };
  const ports: AutoTransmitPorts = {
    queue: vi.fn(async (scope, generation) => ({ operationId: `op-${scope.documentId}-${generation}`, documentId: scope.documentId, generation })),
    transmit: vi.fn(async () => ({ status: "idle" as const })),
    acknowledged: vi.fn(), authRequired: vi.fn(), failed: vi.fn(),
  };
  const scheduler = new AutoTransmit(ports);
  scheduler.openSession(first);
  scheduler.openSession(other);
  scheduler.edit(first, "1");
  scheduler.durable(first, "1");
  await scheduler.flushDocument(first);
  await vi.advanceTimersByTimeAsync(1000);
  expect(ports.transmit).toHaveBeenCalled();
  vi.mocked(ports.transmit).mockClear();

  scheduler.composition(first, true);
  scheduler.edit(first, "2");
  scheduler.durable(first, "2");
  scheduler.composition(first, false);
  await scheduler.holdConflictSession(first);
  scheduler.retireConflictSession(first);

  scheduler.edit(other, "1");
  scheduler.durable(other, "1");
  await vi.advanceTimersByTimeAsync(1000);
  expect(ports.queue).toHaveBeenCalledWith(other, "1");
  expect(ports.queue).not.toHaveBeenCalledWith(first, "2");
  expect(ports.transmit).toHaveBeenCalled();
  await scheduler.dispose();
});
