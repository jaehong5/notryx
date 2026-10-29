import { afterEach, expect, test, vi } from "vitest";
import { AutoTransmit, type AutoTransmitPorts } from "./auto-transmit";

function setup() {
  vi.useFakeTimers();
  const ports: AutoTransmitPorts = {
    queue: vi.fn(),
    transmit: vi.fn(async () => ({ status: "idle" as const })),
    acknowledged: vi.fn(),
    authRequired: vi.fn(),
    permissionDenied: vi.fn(),
    failed: vi.fn(),
  };
  const scheduler = new AutoTransmit(ports);
  scheduler.activateBinding("binding-a");
  return { ports, scheduler };
}

afterEach(() => vi.useRealTimers());

test("서버 권한 거절 후 같은 등록의 같은 복원 취소만 전송을 재개한다", async () => {
  const { ports, scheduler } = setup();
  vi.mocked(ports.transmit)
    .mockResolvedValueOnce({ status: "permission_denied", operationId: "restore-1" })
    .mockResolvedValueOnce({ status: "abandoned", operationId: "restore-1" })
    .mockResolvedValueOnce({ status: "committed", operationId: "content-2" })
    .mockResolvedValue({ status: "idle" });
  scheduler.resume();
  await vi.advanceTimersByTimeAsync(1000);
  expect(ports.transmit).toHaveBeenCalledTimes(1);
  expect(ports.permissionDenied).toHaveBeenCalledExactlyOnceWith("restore-1");
  scheduler.kick();
  scheduler.resumeSync();
  scheduler.activateBinding("binding-a");
  expect(scheduler.resumeRestoreAfterCancel("binding-b", "restore-1")).toBe(false);
  expect(scheduler.resumeRestoreAfterCancel("binding-a", "other")).toBe(false);
  await vi.advanceTimersByTimeAsync(0);
  expect(ports.transmit).toHaveBeenCalledTimes(1);
  expect(scheduler.resumeRestoreAfterCancel("binding-a", "restore-1")).toBe(true);
  await vi.advanceTimersByTimeAsync(0);
  expect(ports.transmit).toHaveBeenCalledTimes(4);
  expect(ports.failed).not.toHaveBeenCalled();
  await scheduler.dispose();
});

test("권한 변경 후 명시 재시도는 같은 등록과 복원 작업만 재전송한다", async () => {
  const { ports, scheduler } = setup();
  const retry = scheduler;
  vi.mocked(ports.transmit)
    .mockResolvedValueOnce({ status:"permission_denied", operationId:"restore-1" })
    .mockResolvedValueOnce({ status:"committed", operationId:"restore-1" })
    .mockResolvedValue({ status:"idle" });
  scheduler.resume();
  await vi.advanceTimersByTimeAsync(1000);
  expect(retry.retryRestoreAfterPermissionDenied("binding-b", "restore-1")).toBe(false);
  expect(retry.retryRestoreAfterPermissionDenied("binding-a", "other")).toBe(false);
  await vi.advanceTimersByTimeAsync(0);
  expect(ports.transmit).toHaveBeenCalledTimes(1);
  expect(retry.retryRestoreAfterPermissionDenied("binding-a", "restore-1")).toBe(true);
  await vi.advanceTimersByTimeAsync(0);
  expect(ports.transmit).toHaveBeenCalledTimes(3);
  expect(ports.queue).not.toHaveBeenCalled();
  expect(ports.failed).not.toHaveBeenCalled();
  await scheduler.dispose();
});

test.each(["needs_review", "sync_pending", "auth_required", "gateway_auth_required"] as const)(
  "권한 변경 후 재시도는 무관한 %s 중단을 해제하지 않는다",
  async (status) => {
    const { ports, scheduler } = setup();
    const retry = scheduler;
    vi.mocked(ports.transmit).mockResolvedValueOnce({ status }).mockResolvedValue({ status:"idle" });
    scheduler.resume();
    await vi.advanceTimersByTimeAsync(1000);
    expect(retry.retryRestoreAfterPermissionDenied("binding-a", "restore-1")).toBe(false);
    await vi.advanceTimersByTimeAsync(1000);
    expect(ports.transmit).toHaveBeenCalledTimes(1);
    await scheduler.dispose();
  },
);

test("취소 저장이 진행 중인 복원의 늦은 권한 거절보다 먼저 끝나도 전송을 재개한다", async () => {
  const { ports, scheduler } = setup();
  let release!: (result: { status: "permission_denied"; operationId: string }) => void;
  vi.mocked(ports.transmit)
    .mockImplementationOnce(() => new Promise((resolve) => { release = resolve; }))
    .mockResolvedValueOnce({ status: "abandoned", operationId: "restore-1" })
    .mockResolvedValue({ status: "idle" });
  scheduler.resume();
  await vi.advanceTimersByTimeAsync(1000);
  expect(ports.transmit).toHaveBeenCalledTimes(1);
  expect(scheduler.resumeRestoreAfterCancel("binding-a", "restore-1")).toBe(true);
  release({ status: "permission_denied", operationId: "restore-1" });
  await vi.advanceTimersByTimeAsync(0);
  expect(ports.transmit).toHaveBeenCalledTimes(3);
  expect(ports.permissionDenied).not.toHaveBeenCalled();
  await scheduler.dispose();
});

test("진행 중인 다른 복원에 대한 취소는 늦은 권한 거절을 해제하지 않는다", async () => {
  const { ports, scheduler } = setup();
  let release!: (result: { status: "permission_denied"; operationId: string }) => void;
  vi.mocked(ports.transmit)
    .mockImplementationOnce(() => new Promise((resolve) => { release = resolve; }))
    .mockResolvedValue({ status: "idle" });
  scheduler.resume();
  await vi.advanceTimersByTimeAsync(1000);
  expect(scheduler.resumeRestoreAfterCancel("binding-b", "restore-1")).toBe(false);
  scheduler.resumeRestoreAfterCancel("binding-a", "other");
  release({ status: "permission_denied", operationId: "restore-1" });
  await vi.advanceTimersByTimeAsync(1000);
  expect(ports.transmit).toHaveBeenCalledTimes(1);
  expect(ports.permissionDenied).toHaveBeenCalledExactlyOnceWith("restore-1");
  await scheduler.dispose();
});

test("취소 저장이 진행 중이어도 늦은 다른 검토 오류는 해제하지 않는다", async () => {
  const { ports, scheduler } = setup();
  let release!: (result: { status: "needs_review" }) => void;
  vi.mocked(ports.transmit)
    .mockImplementationOnce(() => new Promise((resolve) => { release = resolve; }))
    .mockResolvedValue({ status: "idle" });
  scheduler.resume();
  await vi.advanceTimersByTimeAsync(1000);
  expect(scheduler.resumeRestoreAfterCancel("binding-a", "restore-1")).toBe(true);
  release({ status: "needs_review" });
  await vi.advanceTimersByTimeAsync(1000);
  expect(ports.transmit).toHaveBeenCalledTimes(1);
  await scheduler.dispose();
});

test("이전 등록의 늦은 권한 거절은 새 등록을 차단하거나 안내하지 않는다", async () => {
  const { ports, scheduler } = setup();
  let release!: (result: { status: "permission_denied"; operationId: string }) => void;
  vi.mocked(ports.transmit)
    .mockImplementationOnce(() => new Promise((resolve) => { release = resolve; }))
    .mockResolvedValue({ status: "idle" });
  scheduler.resume();
  await vi.advanceTimersByTimeAsync(1000);
  scheduler.activateBinding("binding-b");
  release({ status: "permission_denied", operationId: "restore-1" });
  await vi.advanceTimersByTimeAsync(1000);
  expect(ports.transmit).toHaveBeenCalledTimes(2);
  expect(ports.permissionDenied).not.toHaveBeenCalled();
  await scheduler.dispose();
});

test("전송 중 명시 재시도는 뒤늦게 도착한 권한 거절로 사라지지 않는다", async () => {
  const { ports, scheduler } = setup();
  let release!: (result: { status: "permission_denied"; operationId: string }) => void;
  vi.mocked(ports.transmit)
    .mockImplementationOnce(() => new Promise((resolve) => { release = resolve; }))
    .mockResolvedValue({ status: "idle" });
  scheduler.resume();
  await vi.advanceTimersByTimeAsync(1000);
  expect(ports.transmit).toHaveBeenCalledTimes(1);
  scheduler.resume();
  release({ status:"permission_denied", operationId:"restore-1" });
  await vi.advanceTimersByTimeAsync(0);
  expect(ports.transmit).toHaveBeenCalledTimes(2);
  await scheduler.dispose();
});

test("명시 재시도와 경합한 권한 거절도 빈 작업 ID는 거절한다", async () => {
  const { ports, scheduler } = setup();
  let release!: (result: { status: "permission_denied"; operationId: string }) => void;
  vi.mocked(ports.transmit)
    .mockImplementationOnce(() => new Promise((resolve) => { release = resolve; }))
    .mockResolvedValue({ status: "idle" });
  scheduler.resume();
  await vi.advanceTimersByTimeAsync(1000);
  scheduler.resume();
  release({ status:"permission_denied", operationId:"" });
  await vi.advanceTimersByTimeAsync(0);
  expect(ports.transmit).toHaveBeenCalledTimes(1);
  expect(ports.permissionDenied).not.toHaveBeenCalled();
  expect(ports.failed).toHaveBeenCalledOnce();
  await scheduler.dispose();
});

test("복원 취소 후 늦은 권한 거절을 받아도 조합 중에는 새 전송을 시작하지 않는다", async () => {
  const { ports, scheduler } = setup();
  let release!: (result: { status: "permission_denied"; operationId: string }) => void;
  vi.mocked(ports.transmit)
    .mockImplementationOnce(() => new Promise((resolve) => { release = resolve; }))
    .mockResolvedValueOnce({ status:"abandoned", operationId:"restore-1" })
    .mockResolvedValue({ status:"idle" });
  scheduler.resume();
  await vi.advanceTimersByTimeAsync(1000);
  scheduler.inputComposition(true);
  expect(scheduler.resumeRestoreAfterCancel("binding-a", "restore-1")).toBe(true);
  release({ status:"permission_denied", operationId:"restore-1" });
  await vi.advanceTimersByTimeAsync(1000);
  expect(ports.transmit).toHaveBeenCalledTimes(1);
  scheduler.inputComposition(false);
  await vi.advanceTimersByTimeAsync(0);
  expect(ports.transmit).toHaveBeenCalledTimes(3);
  await scheduler.dispose();
});

test.each(["needs_review", "sync_pending", "auth_required", "gateway_auth_required"] as const)(
  "복원 취소는 무관한 %s 중단을 해제하지 않는다",
  async (status) => {
    const { ports, scheduler } = setup();
    vi.mocked(ports.transmit).mockResolvedValueOnce({ status }).mockResolvedValue({ status: "idle" });
    scheduler.resume();
    await vi.advanceTimersByTimeAsync(1000);
    expect(ports.transmit).toHaveBeenCalledTimes(1);
    expect(scheduler.resumeRestoreAfterCancel("binding-a", "restore-1")).toBe(false);
    await vi.advanceTimersByTimeAsync(1000);
    expect(ports.transmit).toHaveBeenCalledTimes(1);
    await scheduler.dispose();
  },
);

test("등록 교체 후 이전 복원 취소로 새 등록을 재개하지 않는다", async () => {
  const { ports, scheduler } = setup();
  vi.mocked(ports.transmit)
    .mockResolvedValueOnce({ status: "permission_denied", operationId: "restore-1" })
    .mockResolvedValue({ status: "idle" });
  scheduler.resume();
  await vi.advanceTimersByTimeAsync(1000);
  scheduler.activateBinding("binding-b");
  expect(scheduler.resumeRestoreAfterCancel("binding-a", "restore-1")).toBe(false);
  await vi.advanceTimersByTimeAsync(1000);
  expect(ports.transmit).toHaveBeenCalledTimes(2);
  await scheduler.dispose();
});
