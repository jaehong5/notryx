import { expect, test, vi } from "vitest";
import { PreviewImageQueue } from "./preview-image-queue";

const tick = () => new Promise<void>((resolve) => setTimeout(resolve, 0));

test("seventeen valid image requests eventually load without concurrent decoding", async () => {
  const queue = new PreviewImageQueue();
  const controller = new AbortController();
  const releases: Array<() => void> = [];
  let inFlight = 0;
  let maximumInFlight = 0;
  const results = Promise.allSettled(Array.from({ length: 17 }, (_, index) => queue.load(async () => {
    inFlight++;
    maximumInFlight = Math.max(maximumInFlight, inFlight);
    await new Promise<void>((resolve) => releases.push(resolve));
    inFlight--;
    return index;
  }, controller.signal)));
  for (let index = 0; index < 16; index++) {
    await tick();
    expect(releases[index]).toBeDefined();
    releases[index]();
  }
  await tick();
  if (releases[16]) releases[16]();
  const settled = await results;
  expect(settled.every((result) => result.status === "fulfilled")).toBe(true);
  expect(settled.map((result) => result.status === "fulfilled" ? result.value : null)).toEqual(Array.from({ length: 17 }, (_, index) => index));
  expect(maximumInFlight).toBe(1);
});

test("a cancelled request waiting for capacity never starts", async () => {
  const queue = new PreviewImageQueue();
  const active = new AbortController();
  const cancelled = new AbortController();
  let release!: () => void;
  const first = queue.load(() => new Promise<void>((resolve) => { release = resolve; }), active.signal);
  const other = Array.from({ length: 15 }, () => queue.load(async () => {}, active.signal));
  const work = vi.fn(async () => {});
  const waiting = queue.load(work, cancelled.signal);
  const rejected = expect(waiting).rejects.toThrow("preview_cancelled");
  await tick();
  cancelled.abort();
  await rejected;
  release();
  await Promise.all([first, ...other]);
  expect(work).not.toHaveBeenCalled();
});

test("repeatedly cancelled capacity waiters release their registrations while decoding is blocked", async () => {
  const queue = new PreviewImageQueue();
  const active = new AbortController();
  let release!: () => void;
  const first = queue.load(() => new Promise<void>((resolve) => { release = resolve; }), active.signal);
  const other = Array.from({ length: 15 }, () => queue.load(async () => {}, active.signal));
  await tick();
  for (let round = 0; round < 3; round++) {
    const controllers = Array.from({ length: 50 }, () => new AbortController());
    const removals = controllers.map(({ signal }) => vi.spyOn(signal, "removeEventListener"));
    const waiting = controllers.map(({ signal }) => queue.load(async () => {}, signal));
    const results = Promise.allSettled(waiting);
    controllers.forEach((controller) => controller.abort());
    expect((await results).every((result) => result.status === "rejected")).toBe(true);
    expect(removals.every((remove) => remove.mock.calls.some(([type]) => type === "abort"))).toBe(true);
    expect((queue as unknown as { waiting: Set<unknown> }).waiting.size).toBe(0);
  }
  release();
  await Promise.all([first, ...other]);
});

test("a waiter cancelled as capacity opens passes the slot to the next waiter", async () => {
  const queue = new PreviewImageQueue();
  const active = new AbortController();
  const cancelled = new AbortController();
  let release!: () => void;
  const first = queue.load(() => new Promise<void>((resolve) => { release = resolve; }), active.signal);
  const other = Array.from({ length: 15 }, () => queue.load(async () => {}, active.signal));
  const remove = cancelled.signal.removeEventListener.bind(cancelled.signal);
  vi.spyOn(cancelled.signal, "removeEventListener").mockImplementation((...args) => {
    remove(...args);
    cancelled.abort();
  });
  const skipped = queue.load(async () => "skipped", cancelled.signal);
  const skippedResult = expect(skipped).rejects.toThrow("preview_cancelled");
  const next = queue.load(async () => "loaded", active.signal);
  await tick();
  release();
  await skippedResult;
  expect(await next).toBe("loaded");
  await Promise.all([first, ...other]);
});
