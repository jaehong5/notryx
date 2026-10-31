// @vitest-environment jsdom
import { act, createElement } from "react";
import { createRoot } from "react-dom/client";
import { afterEach, expect, test, vi } from "vitest";
import { MarkdownPreview } from "./preview";
import { mermaidQueue } from "./mermaid-queue";

vi.mock("./mermaid-queue", () => ({ mermaidQueue: { render: vi.fn() } }));
afterEach(() => { vi.restoreAllMocks(); vi.unstubAllGlobals(); vi.mocked(mermaidQueue.render).mockReset(); });

test("renders only visible Mermaid fences, preserves source and releases the image on close", async () => {
  let appear!: () => void;
  const disconnect = vi.fn();
  vi.stubGlobal("IntersectionObserver", class {
    constructor(callback: IntersectionObserverCallback) { appear = () => callback([{ isIntersecting: true } as IntersectionObserverEntry], this as unknown as IntersectionObserver); }
    observe() {}
    disconnect = disconnect;
  });
  vi.mocked(mermaidQueue.render).mockResolvedValue('<svg xmlns="http://www.w3.org/2000/svg"/>');
  const create = vi.fn(() => "blob:diagram");
  const revoke = vi.fn();
  Object.defineProperty(URL, "createObjectURL", { configurable: true, value: create });
  Object.defineProperty(URL, "revokeObjectURL", { configurable: true, value: revoke });
  const container = document.createElement("div");
  const root = createRoot(container);
  const source = "```mermaid\ngraph LR\nA-->B\n```\n\n```custom\nkeep me\n```";
  await act(async () => root.render(createElement(MarkdownPreview, { source })));
  expect(mermaidQueue.render).not.toHaveBeenCalled();
  expect(container.querySelector("code.language-custom")?.textContent).toBe("keep me\n");
  await act(async () => appear());
  expect(mermaidQueue.render).toHaveBeenCalledExactlyOnceWith("graph LR\nA-->B\n", expect.any(AbortSignal), "light");
  expect(container.querySelector(".mermaid-preview img")?.getAttribute("src")).toBe("blob:diagram");
  expect(container.querySelector("details code")?.textContent).toBe("graph LR\nA-->B\n");
  expect(container.querySelector("svg")).toBeNull();
  await act(async () => root.unmount());
  expect(revoke).toHaveBeenCalledExactlyOnceWith("blob:diagram");
  expect(vi.mocked(mermaidQueue.render).mock.calls[0][1].aborted).toBe(true);
});

test("keeps source and explains rejected configuration", async () => {
  vi.stubGlobal("IntersectionObserver", undefined);
  vi.mocked(mermaidQueue.render).mockRejectedValue(new Error("mermaid_document_config"));
  const container = document.createElement("div");
  const root = createRoot(container);
  await act(async () => root.render(createElement(MarkdownPreview, { source: '```mermaid\n%%{init:{"htmlLabels":true}}%%\ngraph LR\nA-->B\n```' })));
  expect(container.textContent).toContain("설정 변경은 지원하지 않습니다");
  expect(container.querySelector("code")?.textContent).toContain('%%{init:{"htmlLabels":true}}%%');
  expect(container.querySelector("img")).toBeNull();
  await act(async () => root.unmount());
});

test("ignores a previous document's delayed diagram result", async () => {
  vi.stubGlobal("IntersectionObserver", undefined);
  let resolve!: (svg: string) => void;
  vi.mocked(mermaidQueue.render).mockReturnValue(new Promise((done) => { resolve = done; }));
  const create = vi.fn();
  Object.defineProperty(URL, "createObjectURL", { configurable: true, value: create });
  const container = document.createElement("div");
  const root = createRoot(container);
  await act(async () => root.render(createElement(MarkdownPreview, { source: "```mermaid\ngraph LR\nA-->B\n```" })));
  await act(async () => root.render(createElement(MarkdownPreview, { source: "# 다른 문서" })));
  await act(async () => resolve("<svg/>"));
  expect(create).not.toHaveBeenCalled();
  expect(container.textContent).toBe("다른 문서");
  await act(async () => root.unmount());
});
