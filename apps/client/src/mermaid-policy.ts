import { parse, walk } from "css-tree";

export const MERMAID_SOURCE_BYTES = 64 * 1024;
export const MERMAID_SVG_BYTES = 2 * 1024 * 1024;
export const MERMAID_POLICY_VERSION = 1;

export function checkMermaidSource(source: string): void {
  if (new TextEncoder().encode(source).length > MERMAID_SOURCE_BYTES) throw new Error("mermaid_source_limit");
  if (/^---(?:\s|$)/u.test(source.trimStart()) || /%%\s*\{/u.test(source)) throw new Error("mermaid_document_config");
}

const elements = new Set("svg g defs marker path rect circle ellipse line polyline polygon text tspan textPath title desc style clipPath linearGradient radialGradient stop pattern symbol use filter feDropShadow".split(" "));
const properties = new Set(("color background background-color fill fill-opacity fill-rule clip-rule rx ry stroke stroke-width stroke-opacity stroke-dasharray stroke-dashoffset stroke-linecap stroke-linejoin stroke-miterlimit " +
  "font font-family font-size font-weight font-style font-variant text-anchor text-decoration text-transform text-overflow white-space word-wrap overflow-wrap word-break line-height letter-spacing dominant-baseline alignment-baseline baseline-shift " +
  "opacity visibility display overflow width height min-width max-width min-height max-height padding margin border border-bottom border-radius box-sizing box-shadow vertical-align text-align position z-index flood-color flood-opacity " +
  "marker-start marker-mid marker-end clip-path filter transform transform-origin paint-order stop-color stop-opacity shape-rendering vector-effect cursor pointer-events " +
  "animation animation-name animation-duration animation-timing-function animation-iteration-count").split(" "));
const attributes = new Set(("id name class role aria-label aria-labelledby aria-describedby aria-roledescription tabindex style xmlns xmlns:xlink version viewBox preserveAspectRatio " +
  "x y x1 y1 x2 y2 cx cy r rx ry d points dx dy rotate textLength lengthAdjust startOffset " +
  "markerWidth markerHeight markerUnits orient refX refY gradientUnits gradientTransform spreadMethod offset patternUnits patternContentUnits patternTransform clipPathUnits " +
  "href xlink:href stdDeviation filterUnits").split(" "));
const localReference = /^#[a-zA-Z_][\w:.-]*$/u;
const cssFunctions = new Set(["rgb", "rgba", "hsl", "hsla", "calc", "min", "max", "clamp", "translate", "translateX", "translateY", "rotate", "scale", "matrix", "cubic-bezier", "steps", "drop-shadow"]);
const fontVariables = new Set(["--mermaid-font-family", "--mermaid-alt-font-family"]);

function unsafe(): never { throw new Error("mermaid_unsafe_svg"); }

function checkCss(css: string, context: "stylesheet" | "declarationList" | "value"): void {
  // Reject escapes so identifiers cannot conceal URLs or unsupported CSS features.
  if (css.includes("\\")) unsafe();
  const tree = parse(css, { context, parseCustomProperty: true, onParseError: unsafe });
  walk(tree, (node) => {
    if (node.type === "Raw") unsafe();
    if (node.type === "Atrule" && node.name !== "keyframes") unsafe();
    if (node.type === "Declaration" && !properties.has(node.property) && !fontVariables.has(node.property)) unsafe();
    if (node.type === "Url" && !localReference.test(node.value)) unsafe();
    if (node.type === "Function" && !cssFunctions.has(node.name)) {
      const argument = node.children.first;
      if (node.name !== "var" || node.children.size !== 1 || argument?.type !== "Identifier" || !fontVariables.has(argument.name)) unsafe();
    }
  });
}

export function sanitizeMermaidSvg(svg: string): string {
  try {
    if (new TextEncoder().encode(svg).length > MERMAID_SVG_BYTES || /<!DOCTYPE|<!ENTITY|<\?/iu.test(svg)) unsafe();
    const document = new DOMParser().parseFromString(svg, "image/svg+xml");
    if (document.querySelector("parsererror") || document.documentElement.localName !== "svg") unsafe();
    for (const element of [document.documentElement, ...document.documentElement.querySelectorAll("*")]) {
      if (element.namespaceURI !== "http://www.w3.org/2000/svg" || !elements.has(element.localName)) unsafe();
      for (const { name, value } of element.attributes) {
        if (name === "xmlns" && value === "http://www.w3.org/2000/svg") continue;
        if (name === "xmlns:xlink" && value === "http://www.w3.org/1999/xlink") continue;
        if (name === "xmlns" || name === "xmlns:xlink") unsafe();
        if (name.startsWith("data-")) continue;
        if (!attributes.has(name) && !properties.has(name)) unsafe();
        if ((name === "href" || name === "xlink:href") && !localReference.test(value)) unsafe();
        if (name === "style") checkCss(value, "declarationList");
        else if (properties.has(name)) checkCss(value, "value");
      }
      if (element.localName === "style") checkCss(element.textContent ?? "", "stylesheet");
    }
    return new XMLSerializer().serializeToString(document.documentElement);
  } catch { return unsafe(); }
}

export function mermaidFailureMessage(error: unknown): string {
  const code = String(error);
  if (/mermaid_source_(?:limit|too_large)/u.test(code)) return "64 KiB를 넘는 다이어그램은 원문으로 표시합니다.";
  if (code.includes("mermaid_document_config")) return "다이어그램 안의 설정 변경은 지원하지 않습니다. 원문을 표시합니다.";
  if (code.includes("mermaid_unsafe_svg")) return "안전하게 표시할 수 없는 SVG 구조가 있어 원문을 표시합니다.";
  if (code.includes("mermaid_render_timeout")) return "다이어그램 생성 시간이 초과되어 원문을 표시합니다.";
  if (code.includes("mermaid_unsupported_diagram")) return "현재 미리보기는 흐름도, 시퀀스, 클래스, 상태, ER 다이어그램을 지원합니다. 이 유형은 원문으로 표시합니다.";
  if (/mermaid_(?:unsupported|unavailable)/u.test(code)) return "이 환경에서는 다이어그램을 원문으로 표시합니다.";
  if (code.includes("mermaid_busy")) return "다이어그램 처리 중입니다. 미리보기를 다시 열어 주세요.";
  if (code.includes("mermaid_edge_limit")) return "연결 500개를 넘는 다이어그램은 원문으로 표시합니다.";
  return "다이어그램을 그릴 수 없습니다. Mermaid 문법을 확인해 주세요.";
}
