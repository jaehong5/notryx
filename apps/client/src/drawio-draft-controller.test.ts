import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { DiagramDraftController, type DiagramDraftState } from "./drawio-draft-controller";

type Save = (request: { generation: string; xml: string }) => Promise<{ generation: string }>;

function setup(save = vi.fn<Save>(async () => ({ generation: "1" }))) {
  const states: DiagramDraftState[] = [];
  const close = vi.fn(async () => {});
  const controller = new DiagramDraftController("0", { save, close, status: state => states.push(state) });
  return { controller, save, close, states };
}

beforeEach(() => vi.useFakeTimers());
afterEach(() => vi.useRealTimers());

test("checkpoints the latest XML only after one second without changes", async () => {
  const { controller, save, states } = setup();
  controller.change("first");
  await vi.advanceTimersByTimeAsync(900);
  controller.change("latest");
  await vi.advanceTimersByTimeAsync(999);
  expect(save).not.toHaveBeenCalled();
  await vi.advanceTimersByTimeAsync(1);
  expect(save).toHaveBeenCalledExactlyOnceWith({ generation: "0", xml: "latest" });
  expect(states.at(-1)).toBe("saved");
});

test("serializes checkpoints and does not acknowledge newer edits with an older result", async () => {
  let finishFirst!: (value: { generation: string }) => void;
  let finishSecond!: (value: { generation: string }) => void;
  const save = vi.fn<Save>(() => new Promise<{ generation: string }>(resolve => {
    if (save.mock.calls.length === 1) finishFirst = resolve;
    else finishSecond = resolve;
  }));
  const { controller, states } = setup(save);
  controller.change("first");
  await vi.advanceTimersByTimeAsync(1000);
  controller.change("second");
  await vi.advanceTimersByTimeAsync(1000);
  expect(save).toHaveBeenCalledTimes(1);
  finishFirst({ generation: "1" });
  await vi.advanceTimersByTimeAsync(1);
  expect(save).toHaveBeenLastCalledWith({ generation: "1", xml: "second" });
  expect(states).not.toContain("saved");
  finishSecond({ generation: "2" });
  await vi.advanceTimersByTimeAsync(1);
  expect(states.at(-1)).toBe("saved");
});

test("keeps the window open and retains pending XML after a checkpoint failure", async () => {
  const save = vi.fn<Save>(async () => ({ generation: "1" }))
    .mockRejectedValueOnce(new Error("disk full"));
  const { controller, close, states } = setup(save);
  controller.change("preserved XML");
  await expect(controller.close()).rejects.toThrow("disk full");
  expect(close).not.toHaveBeenCalled();
  expect(states.at(-1)).toBe("error");
  await controller.close();
  expect(save).toHaveBeenLastCalledWith({ generation: "0", xml: "preserved XML" });
  expect(close).toHaveBeenCalledOnce();
});

test("invalid latest XML remains unsaved after an older checkpoint completes", async () => {
  let finish!: (value: { generation: string }) => void;
  const save = vi.fn<Save>(() => new Promise<{ generation: string }>(resolve => { finish = resolve; }));
  const { controller, close, states } = setup(save);
  controller.change("valid earlier XML");
  await vi.advanceTimersByTimeAsync(1000);
  controller.reject();
  finish({ generation: "1" });
  await vi.advanceTimersByTimeAsync(1);
  expect(states.at(-1)).toBe("error");
  await expect(controller.close()).rejects.toThrow();
  expect(close).not.toHaveBeenCalled();
  expect(save).toHaveBeenCalledTimes(1);
});

test("close flushes pending changes immediately and cancels the idle save", async () => {
  const { controller, save, close } = setup();
  controller.change("latest");
  await controller.close();
  await vi.advanceTimersByTimeAsync(2000);
  expect(save).toHaveBeenCalledExactlyOnceWith({ generation: "0", xml: "latest" });
  expect(close).toHaveBeenCalledOnce();
  controller.dispose();
});

test("returns the durable generation only after the pending snapshot is saved", async () => {
  const { controller, save } = setup();
  controller.change("export snapshot");
  await expect(controller.flush()).resolves.toBe("1");
  expect(save).toHaveBeenCalledExactlyOnceWith({ generation: "0", xml: "export snapshot" });
});
