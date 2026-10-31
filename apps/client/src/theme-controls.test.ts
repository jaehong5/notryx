// @vitest-environment jsdom
import { act, createElement } from "react";
import { createRoot } from "react-dom/client";
import { afterEach, expect, test, vi } from "vitest";
import { ThemeControls, useTheme } from "./theme-controls";

const { invoke } = vi.hoisted(() => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke }));
const light = { selection: "light", custom: null, safe_mode: false, warning: null };
let controller: ReturnType<typeof useTheme>;
function App() { controller = useTheme(); return createElement(ThemeControls, { controller }); }
afterEach(() => invoke.mockReset());

test("a synchronous theme bridge failure keeps the default theme and recovery controls", async () => {
  invoke.mockImplementationOnce(() => { throw new Error("bridge unavailable"); });
  const container = document.createElement("div"); const root = createRoot(container);
  try {
    await act(async () => root.render(createElement(App)));
    expect(controller.theme.base).toBe("light");
    expect(controller.ready).toBe(true);
    expect(container.querySelector('[role="alert"]')?.textContent).toContain("기본 테마를 사용");
    expect(container.querySelector<HTMLButtonElement>("#theme-reset")?.disabled).toBe(false);
  } finally { await act(async () => root.unmount()); }
});

test("restores selection, retains the old theme on a save failure, and resets explicitly", async () => {
  invoke.mockResolvedValueOnce({ ...light, selection: "dark" });
  const container = document.createElement("div"); const root = createRoot(container);
  await act(async () => root.render(createElement(App)));
  expect(controller.theme.base).toBe("dark");
  invoke.mockRejectedValueOnce(new Error("theme_storage_error"));
  await act(async () => controller.select("light"));
  expect(controller.theme.base).toBe("dark");
  expect(container.textContent).toContain("이전 테마를 유지");
  invoke.mockResolvedValueOnce(light);
  await act(async () => controller.reset());
  expect(controller.theme.base).toBe("light");
  expect(invoke).toHaveBeenLastCalledWith("theme_reset", undefined);
  await act(async () => root.unmount());
});

test("bounds file reads and locks selection while an import is being read", async () => {
  invoke.mockResolvedValue(light);
  const root = createRoot(document.createElement("div"));
  await act(async () => root.render(createElement(App)));
  const oversized = { size: 256 * 1024 + 1, arrayBuffer: vi.fn() } as unknown as File;
  await act(async () => controller.importFile(oversized));
  expect(oversized.arrayBuffer).not.toHaveBeenCalled();
  let finish!: (value: ArrayBuffer) => void;
  const file = { size: 2, arrayBuffer: () => new Promise<ArrayBuffer>((resolve) => { finish = resolve; }) } as File;
  let importing!: Promise<void>;
  await act(async () => { importing = controller.importFile(file); });
  expect(controller.busy).toBe(true);
  await act(async () => controller.select("dark"));
  expect(invoke).toHaveBeenCalledTimes(1);
  await act(async () => { finish(new TextEncoder().encode("{}").buffer); await importing; });
  expect(invoke).toHaveBeenLastCalledWith("theme_import", { json: "{}" });
  expect(controller.busy).toBe(false);
  await act(async () => root.unmount());
});

test("safe mode disables imports and selection while keeping reset available", async () => {
  invoke.mockResolvedValue({ ...light, safe_mode: true, warning: "safe_mode" });
  const container = document.createElement("div"); const root = createRoot(container);
  await act(async () => root.render(createElement(App)));
  expect(container.querySelector<HTMLSelectElement>("select")?.disabled).toBe(true);
  expect(container.querySelector<HTMLInputElement>("input")?.disabled).toBe(true);
  expect(container.querySelector<HTMLButtonElement>("#theme-reset")?.disabled).toBe(false);
  expect(container.textContent).toContain("안전 모드");
  await act(async () => root.unmount());
});
