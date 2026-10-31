// @vitest-environment jsdom
/// <reference types="node" />
import { expect, test, vi } from "vitest";
import { webcrypto } from "node:crypto";
import { MermaidRenderQueue } from "./mermaid-queue";

Object.defineProperty(globalThis, "crypto", { value: webcrypto, configurable: true });
const svg = '<svg xmlns="http://www.w3.org/2000/svg"><text>한글</text></svg>';
const turn = () => new Promise((resolve) => setTimeout(resolve, 10));

test("limits work to two surfaces, drops cancelled queued work, and caches by source and theme", async () => {
  const complete: ((svg: string) => void)[] = [];
  const render = vi.fn(() => new Promise<string>((resolve) => complete.push(resolve)));
  const queue = new MermaidRenderQueue(render);
  const controller = new AbortController();
  const first = queue.render("graph LR\nA-->B", new AbortController().signal);
  const second = queue.render("graph LR\nB-->C", new AbortController().signal);
  const third = queue.render("graph LR\nC-->D", controller.signal).catch((error: Error) => error.name);
  await turn();
  expect(render).toHaveBeenCalledTimes(2);
  controller.abort();
  complete[0](svg); complete[1](svg);
  expect(await first).toContain("한글");
  await second;
  expect(await third).toBe("AbortError");
  await queue.render("graph LR\nA-->B", new AbortController().signal);
  expect(render).toHaveBeenCalledTimes(2);
  const dark = queue.render("graph LR\nA-->B", new AbortController().signal, "dark");
  await turn();
  expect(render).toHaveBeenCalledTimes(3);
  complete[2](svg); await dark;
});

test("does not cache unsafe output or cancelled results", async () => {
  const render = vi.fn(async () => '<svg xmlns="http://www.w3.org/2000/svg"><script/></svg>');
  const queue = new MermaidRenderQueue(render);
  await expect(queue.render("graph LR\nA-->B", new AbortController().signal)).rejects.toThrow("mermaid_unsafe_svg");
  await expect(queue.render("graph LR\nA-->B", new AbortController().signal)).rejects.toThrow("mermaid_unsafe_svg");
  expect(render).toHaveBeenCalledTimes(2);
});

test("releases both slots when the native bridge throws synchronously", async () => {
  const queue = new MermaidRenderQueue(() => { throw new Error("bridge unavailable"); });
  const results = await Promise.allSettled(["A", "B", "C"].map((source) => queue.render(source, new AbortController().signal)));
  expect(results.every((result) => result.status === "rejected")).toBe(true);
});
