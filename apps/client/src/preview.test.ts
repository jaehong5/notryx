// @vitest-environment jsdom
import { describe, expect, test, vi } from "vitest";
import { act, createElement } from "react";
import { createRoot } from "react-dom/client";
import { renderToStaticMarkup } from "react-dom/server";
import { classifyPreviewLink, MarkdownPreview } from "./preview";

function render(source: string): string {
  return renderToStaticMarkup(createElement(MarkdownPreview, { source }));
}

describe("AT-31 basic Markdown preview", () => {
  test("renders Markdown and GFM without changing the source", () => {
    const source = "# 제목\n\n| 이름 | 값 |\n| --- | --- |\n| 가 | 1 |\n\n- [x] 완료\n\n```custom\n<!-- source -->\n```";
    const html = render(source);
    expect(html).toContain("<h1>제목</h1>");
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
    const loadImage = vi.fn(async () => new Uint8Array([137, 80, 78, 71]));
    const create = vi.fn(() => "blob:local-preview");
    const revoke = vi.fn();
    Object.defineProperty(URL, "createObjectURL", { configurable: true, value: create });
    Object.defineProperty(URL, "revokeObjectURL", { configurable: true, value: revoke });
    const container = document.createElement("div");
    const root = createRoot(container);
    await act(async () => root.render(createElement(MarkdownPreview, {
      source: "![로컬](./image.png) ![외부](https://example.invalid/track)", loadImage,
    })));
    expect(loadImage).toHaveBeenCalledExactlyOnceWith("./image.png");
    expect(container.querySelector("img")?.getAttribute("src")).toBe("blob:local-preview");
    expect(container.querySelectorAll("img")).toHaveLength(1);
    await act(async () => root.unmount());
    expect(revoke).toHaveBeenCalledExactlyOnceWith("blob:local-preview");
  });

  test("does not create a blob when a late image arrives after the preview closes", async () => {
    let complete!: (bytes: Uint8Array) => void;
    const pending = new Promise<Uint8Array>((resolve) => { complete = resolve; });
    const create = vi.fn(() => "blob:late");
    Object.defineProperty(URL, "createObjectURL", { configurable: true, value: create });
    const root = createRoot(document.createElement("div"));
    await act(async () => root.render(createElement(MarkdownPreview, { source: "![로컬](image.png)", loadImage: () => pending })));
    await act(async () => root.unmount());
    await act(async () => complete(new Uint8Array([1])));
    expect(create).not.toHaveBeenCalled();
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
    for (const unsafe of ["javascript:alert(1)", "data:text/html,x", "file:///etc/passwd", "//example.invalid/x", "https://user:pass@example.invalid/", "foo/%2fbar", "foo\\bar", "http://example.invalid/%0aheader", "#fragment"]) {
      expect(classifyPreviewLink(unsafe), unsafe).toBeNull();
    }
  });
});
