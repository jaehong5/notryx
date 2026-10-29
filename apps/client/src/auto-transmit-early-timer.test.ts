import { afterEach, expect, test, vi } from "vitest";
import { AutoTransmit, type AutoTransmitPorts } from "./auto-transmit";

afterEach(() => { vi.restoreAllMocks(); vi.useRealTimers(); });

test("1초 quiet timer가 단조 시계의 기한보다 일찍 깨어나도 남은 시간 뒤 전송을 다시 시도한다", async () => {
  vi.useFakeTimers();
  const now = vi.spyOn(performance, "now").mockReturnValue(0);
  const ports: AutoTransmitPorts = {
    queue: vi.fn(async (scope, generation) => ({ operationId: "op", documentId: scope.documentId, generation })),
    transmit: vi.fn(async () => ({ status: "idle" as const })),
    acknowledged: vi.fn(), authRequired: vi.fn(), failed: vi.fn(),
  };
  const scheduler = new AutoTransmit(ports);
  scheduler.activateBinding("binding");
  scheduler.kick();
  now.mockReturnValue(999.5);
  await vi.advanceTimersByTimeAsync(1000);
  expect(ports.transmit).not.toHaveBeenCalled();
  now.mockReturnValue(1001);
  await vi.advanceTimersByTimeAsync(2);
  expect(ports.transmit).toHaveBeenCalledOnce();
  await scheduler.dispose();
});
