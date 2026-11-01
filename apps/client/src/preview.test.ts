// @vitest-environment jsdom
import { describe, expect, test, vi } from "vitest";
import { act, createElement } from "react";
import { createRoot } from "react-dom/client";
import { renderToStaticMarkup } from "react-dom/server";
import { classifyPreviewLink, MarkdownPreview } from "./preview";
import type { PreviewImageData } from "./ipc";
import type { DrawioPngStatus } from "../../../packages/protocol-types";

const plainImage = (bytes: Uint8Array): PreviewImageData => ({ bytes, drawioStatus: "none" });

function render(source: string): string {
  return renderToStaticMarkup(createElement(MarkdownPreview, { source }));
}

describe("AT-31 basic Markdown preview", () => {
  test("opens only validated diagrams with an explicit unsaved trial action", async () => {
    Object.defineProperty(URL, "createObjectURL", { configurable: true, value: () => "blob:trial" });
    Object.defineProperty(URL, "revokeObjectURL", { configurable: true, value: vi.fn() });
    const container = document.createElement("div");
    const root = createRoot(container);
    const open = vi.fn();
    for (const status of ["embedded_xml", "invalid_xml", "none"] as const) {
      await act(async () => root.render(createElement(MarkdownPreview, {
        source: "![그림](diagram.drawio.png)", onOpenDiagram: open,
        loadImage: async () => ({ bytes: new Uint8Array([137]), drawioStatus: status }),
      })));
      const button = container.querySelector<HTMLButtonElement>(".diagram-trial");
      if (status === "embedded_xml") {
        expect(button?.textContent).toContain("저장 안 됨");
        button!.click();
        expect(open).toHaveBeenCalledWith("diagram.drawio.png");
      } else expect(button).toBeNull();
    }
    await act(async () => root.unmount());
  });

  test.each([
    ["embedded_xml", "편집 데이터 포함", "diagram.png"],
    ["none", "편집 데이터가 없습니다", "diagram.drawio.png"],
    ["invalid_xml", "편집 데이터가 손상되었습니다", "diagram.drawio.png"],
    ["unsupported_xml", "지원하지 않는 편집 데이터입니다", "diagram.drawio.png"],
    ["limit_exceeded", "편집 데이터가 검사 한도를 초과했습니다", "diagram.drawio.png"],
  ])("keeps the passive image and explains %s metadata", async (drawioStatus, message, target) => {
    Object.defineProperty(URL, "createObjectURL", { configurable: true, value: () => "blob:drawio" });
    Object.defineProperty(URL, "revokeObjectURL", { configurable: true, value: vi.fn() });
    const container = document.createElement("div");
    const root = createRoot(container);
    const bytes = new Uint8Array([137, 80, 78, 71]);
    await act(async () => root.render(createElement(MarkdownPreview, {
      source: `![그림](${target})`, loadImage: async () => ({ bytes, drawioStatus: drawioStatus as DrawioPngStatus }),
    })));
    expect(container.querySelector("img")?.getAttribute("src")).toBe("blob:drawio");
    expect(container.textContent).toContain(message);
    if (drawioStatus === "embedded_xml") expect(container.textContent).toContain("현재 미리보기만 지원");
    expect(container.querySelector("iframe, svg, a")).toBeNull();
    await act(async () => root.unmount());
  });

  test("renders Markdown and GFM without changing the source", () => {
    const source = "# 제목\n\n| 이름 | 값 |\n| --- | --- |\n| 가 | 1 |\n\n- [x] 완료\n\n```custom\n<!-- source -->\n```";
    const html = render(source);
    expect(html).toContain('data-notryx-heading="제목">제목</h1>');
    expect(html).toContain("<table>");
    expect(html).toContain("disabled=\"\"");
    expect(html).toContain("&lt;!-- source --&gt;");
    expect(source).toContain("<!-- source -->");
  });

  test("does not render raw HTML, SVG, scripts, or remote images", () => {
    const html = render([
      '<script>alert(1)</script>',
      '<img src="https://attacker.invalid/a" onerror="alert(1)">',
      '<svg onload="alert(1)"><script>alert(1)</script></svg>',
      '![외부](https://attacker.invalid/track.png)',
      '![로컬](attachment.png)',
      '![SVG](data:image/svg+xml,%3Csvg%20onload%3Dalert(1)%3E)',
    ].join("\n\n"));
    expect(html).not.toMatch(/<(?:script|svg|img|iframe|object|embed)\b/i);
    expect(html).not.toMatch(/\s(?:src|onerror|onload)=/i);
    expect(html).not.toContain("attacker.invalid");
    expect(html).toContain("외부");
    expect(html).toContain("로컬");
  });

  test("never creates a direct navigable link in the WebView", () => {
    const html = render("[외부](https://example.invalid/) [로컬](notes.md) [위험](javascript:alert(1))");
    expect(html).not.toMatch(/\s(?:href|src|target)=/i);
    expect(html).toContain("외부");
    expect(html).toContain("로컬");
    expect(html).toContain("위험");
  });

  test("loads only local images and releases their blob when the preview closes", async () => {
    const loadImage = vi.fn(async () => plainImage(new Uint8Array([137, 80, 78, 71])));
    const create = vi.fn(() => "blob:local-preview");
    const revoke = vi.fn();
    Object.defineProperty(URL, "createObjectURL", { configurable: true, value: create });
    Object.defineProperty(URL, "revokeObjectURL", { configurable: true, value: revoke });
    const container = document.createElement("div");
    const root = createRoot(container);
    await act(async () => root.render(createElement(MarkdownPreview, {
      source: "![로컬](./image.png) ![외부](https://example.invalid/track)", loadImage,
    })));
    expect(loadImage).toHaveBeenCalledExactlyOnceWith("./image.png", expect.any(AbortSignal));
    expect(container.querySelector("img")?.getAttribute("src")).toBe("blob:local-preview");
    expect(container.querySelectorAll("img")).toHaveLength(1);
    await act(async () => root.unmount());
    expect(revoke).toHaveBeenCalledExactlyOnceWith("blob:local-preview");
  });

  test("does not create a blob when a late image arrives after the preview closes", async () => {
    let complete!: (bytes: PreviewImageData) => void;
    const pending = new Promise<PreviewImageData>((resolve) => { complete = resolve; });
    const create = vi.fn(() => "blob:late");
    Object.defineProperty(URL, "createObjectURL", { configurable: true, value: create });
    const root = createRoot(document.createElement("div"));
    await act(async () => root.render(createElement(MarkdownPreview, { source: "![로컬](image.png)", loadImage: () => pending })));
    await act(async () => root.unmount());
    await act(async () => complete(plainImage(new Uint8Array([1]))));
    expect(create).not.toHaveBeenCalled();
  });

  test("keeps an in-flight image request across parent rerenders", async () => {
    let complete!: (bytes: PreviewImageData) => void;
    const pending = new Promise<PreviewImageData>((resolve) => { complete = resolve; });
    const loadImage = vi.fn<(target: string, signal: AbortSignal) => Promise<PreviewImageData>>(() => pending);
    const create = vi.fn(() => "blob:retained");
    const revoke = vi.fn();
    Object.defineProperty(URL, "createObjectURL", { configurable: true, value: create });
    Object.defineProperty(URL, "revokeObjectURL", { configurable: true, value: revoke });
    const container = document.createElement("div");
    const root = createRoot(container);
    const source = "![로컬](image.png)";
    await act(async () => root.render(createElement(MarkdownPreview, { source, loadImage, onOpenLink: () => {} })));
    const signal = loadImage.mock.calls[0][1];
    await act(async () => root.render(createElement(MarkdownPreview, { source, loadImage, onOpenLink: () => {} })));
    expect(signal.aborted).toBe(false);
    expect(loadImage).toHaveBeenCalledTimes(1);
    await act(async () => complete(plainImage(new Uint8Array([1]))));
    expect(container.querySelector("img")?.getAttribute("src")).toBe("blob:retained");
    await act(async () => root.unmount());
    expect(signal.aborted).toBe(true);
    expect(revoke).toHaveBeenCalledExactlyOnceWith("blob:retained");
  });

  test("keeps an image inside a link loading across parent rerenders", async () => {
    let complete!: (bytes: PreviewImageData) => void;
    const pending = new Promise<PreviewImageData>((resolve) => { complete = resolve; });
    const loadImage = vi.fn<(target: string, signal: AbortSignal) => Promise<PreviewImageData>>(() => pending);
    const onOpenLink = vi.fn();
    Object.defineProperty(URL, "createObjectURL", { configurable: true, value: () => "blob:linked" });
    Object.defineProperty(URL, "revokeObjectURL", { configurable: true, value: () => {} });
    const container = document.createElement("div");
    const root = createRoot(container);
    const source = "[![로컬](image.png)](target.md)";
    await act(async () => root.render(createElement(MarkdownPreview, { source, loadImage, onOpenLink: vi.fn() })));
    const signal = loadImage.mock.calls[0][1];
    await act(async () => root.render(createElement(MarkdownPreview, { source, loadImage, onOpenLink })));
    expect(signal.aborted).toBe(false);
    expect(loadImage).toHaveBeenCalledTimes(1);
    await act(async () => complete(plainImage(new Uint8Array([1]))));
    expect(container.querySelector("button img")?.getAttribute("src")).toBe("blob:linked");
    await act(async () => container.querySelector("button")?.click());
    expect(onOpenLink).toHaveBeenCalledExactlyOnceWith({ kind: "document", target: "target.md" });
    await act(async () => root.unmount());
  });

  test("activates a safe link only when explicitly clicked", async () => {
    const onOpenLink = vi.fn();
    const container = document.createElement("div");
    document.body.append(container);
    const root = createRoot(container);
    await act(async () => root.render(createElement(MarkdownPreview, {
      source: "[외부](https://example.invalid/) [위험](javascript:alert(1))",
      onOpenLink,
    })));
    expect(onOpenLink).not.toHaveBeenCalled();
    const buttons = container.querySelectorAll("button");
    expect(buttons).toHaveLength(1);
    expect(container.querySelector("a")).toBeNull();
    await act(async () => buttons[0].click());
    expect(onOpenLink).toHaveBeenCalledOnce();
    expect(onOpenLink).toHaveBeenCalledWith({ kind: "external", target: "https://example.invalid/" });
    await act(async () => root.unmount());
    container.remove();
  });

  test("classifies only explicit http(s) links and bounded relative links", () => {
    expect(classifyPreviewLink("https://example.invalid/path")).toEqual({ kind: "external", target: "https://example.invalid/path" });
    expect(classifyPreviewLink("guide/readme.md#part")).toEqual({ kind: "document", target: "guide/readme.md#part" });
    expect(classifyPreviewLink("../guide/my%20notes.md")).toEqual({ kind: "document", target: "../guide/my%20notes.md" });
    expect(classifyPreviewLink("#%EC%A0%9C%EB%AA%A9")).toEqual({ kind: "anchor", target: "제목" });
    for (const unsafe of ["javascript:alert(1)", "data:text/html,x", "file:///etc/passwd", "//example.invalid/x", "https://user:pass@example.invalid/", "foo/%2fbar", "foo\\bar", "http://example.invalid/%0aheader", "#bad%00anchor", "%23fragment"]) {
      expect(classifyPreviewLink(unsafe), unsafe).toBeNull();
    }
  });

  test("assigns stable unique heading anchors and follows a same-document link", async () => {
    const container = document.createElement("div");
    const root = createRoot(container);
    await act(async () => root.render(createElement(MarkdownPreview, {
      source: "## 한글 **제목**\n\n## 한글 제목\n\n[이동](#%ED%95%9C%EA%B8%80-%EC%A0%9C%EB%AA%A9-1)",
    })));
    const headings = container.querySelectorAll("h2");
    expect(headings[0].getAttribute("data-notryx-heading")).toBe("한글-제목");
    expect(headings[1].getAttribute("data-notryx-heading")).toBe("한글-제목-1");
    const scroll = vi.fn();
    Object.defineProperty(headings[1], "scrollIntoView", { configurable: true, value: scroll });
    await act(async () => container.querySelector("button")?.click());
    expect(scroll).toHaveBeenCalledOnce();
    await act(async () => root.unmount());
  });

  test("a fragment supplied after a document switch scrolls to its heading", async () => {
    const container = document.createElement("div");
    const root = createRoot(container);
    const scroll = vi.fn();
    Object.defineProperty(Element.prototype, "scrollIntoView", { configurable: true, value: scroll });
    await act(async () => root.render(createElement(MarkdownPreview, { source: "# 시작" })));
    await act(async () => root.render(createElement(MarkdownPreview, { source: "# 다음 문서", fragment: { fragment: "다음-문서" } })));
    expect(container.querySelector("h1")?.getAttribute("data-notryx-heading")).toBe("다음-문서");
    expect(scroll).toHaveBeenCalledOnce();
    await act(async () => root.render(createElement(MarkdownPreview, { source: "# 다음 문서", fragment: { fragment: "다음-문서" } })));
    expect(scroll).toHaveBeenCalledTimes(2);
    await act(async () => root.unmount());
    Reflect.deleteProperty(Element.prototype, "scrollIntoView");
  });
});
