// @vitest-environment jsdom
import { describe, expect, test } from "vitest";
import { checkMermaidSource, sanitizeMermaidSvg } from "./mermaid-policy";

const svg = (content: string) => `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 200 100">${content}</svg>`;

describe("AT-34 Mermaid source policy", () => {
  test("accepts standard source and the exact UTF-8 byte limit", () => {
    expect(() => checkMermaidSource("flowchart LR\n A[한글] --> B")).not.toThrow();
    expect(() => checkMermaidSource("a".repeat(65536))).not.toThrow();
    expect(() => checkMermaidSource("가".repeat(21846))).toThrow("mermaid_source_limit");
  });
  test.each([
    '%%{init: {"securityLevel":"loose"}}%%\ngraph LR\nA-->B',
    '%%{config: {"htmlLabels":"false"}}%%\ngraph LR\nA-->B',
    'graph LR\n%%{ init: {"flowchart":{"htmlLabels":true}} }%%\nA-->B',
    '\ufeff\n---\nconfig:\n  htmlLabels: true\n---\nflowchart LR\nA-->B',
    '---\rconfig: {securityLevel: loose}\r---\rgraph LR\rA-->B',
  ])("rejects document configuration without rewriting it", (source) => {
    expect(() => checkMermaidSource(source)).toThrow("mermaid_document_config");
  });
});

describe("AT-31 Mermaid SVG allowlist", () => {
  test("preserves text, geometry, styles and local arrow markers", () => {
    const source = svg('<style>.label{fill:#123;font-size:16px}.edge{marker-end:url(#arrow)}</style><defs><marker id="arrow" viewBox="0 0 10 10"><path d="M0 0L10 5L0 10z"/></marker></defs><g class="label"><text x="10" y="20"><tspan>한글</tspan></text><path class="edge" d="M0 0L20 20"/></g>');
    const clean = sanitizeMermaidSvg(source);
    expect(clean).toContain("한글");
    expect(clean).toContain("url(#arrow)");
    expect(clean).toContain("<marker");
  });
  test("accepts the pinned engine font variables while retaining URL checks", () => {
    expect(() => sanitizeMermaidSvg(svg('<style>:root{--mermaid-font-family:system-ui,sans-serif}text{font-family:var(--mermaid-font-family)}</style><text>한글</text>'))).not.toThrow();
    expect(() => sanitizeMermaidSvg(svg('<style>:root{--mermaid-font-family:url(https://example.invalid)}</style>'))).toThrow();
  });
  test.each([
    '<script>alert(1)</script>', '<g onload="alert(1)"/>',
    '<foreignObject><div xmlns="http://www.w3.org/1999/xhtml">label</div></foreignObject>',
    '<a href="https://example.invalid"><text>link</text></a>',
    '<image href="https://example.invalid/image.png"/>',
    '<use href="https://example.invalid/image.svg#x"/>',
    '<animate attributeName="href" values="https://example.invalid"/>',
    '<style>@import "https://example.invalid/style";</style>',
    '<style>text{fill:url(https://example.invalid/image)}</style>',
    '<style>text{fill:u\\72l(https://example.invalid/image)}</style>',
    '<style>text{background-image:image-set("https://example.invalid/image" 1x)}</style>',
    '<path style="fill:var(--remote)"/>',
    '<path fill="url(https://example.invalid/image)"/>',
    '<g xml:base="https://example.invalid"><path fill="url(#x)"/></g>',
    '<g xmlns="http://www.w3.org/1999/xhtml"><script/></g>',
  ])("rejects active or externally referenced SVG: %s", (content) => {
    expect(() => sanitizeMermaidSvg(svg(content))).toThrow("mermaid_unsafe_svg");
  });
  test("rejects malformed XML, entities and excessive output", () => {
    expect(() => sanitizeMermaidSvg('<svg><')).toThrow();
    expect(() => sanitizeMermaidSvg('<!DOCTYPE svg [<!ENTITY x "test">]>' + svg('<text>&x;</text>'))).toThrow();
    expect(() => sanitizeMermaidSvg(svg(' '.repeat(2 * 1024 * 1024)))).toThrow();
  });
});
