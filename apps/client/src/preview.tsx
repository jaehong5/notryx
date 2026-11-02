import { createContext, useContext, useEffect, useRef, useState, type ReactNode, type RefObject } from "react";
import Markdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { MermaidPreview } from "./mermaid-preview";
import type { PreviewImageData } from "./ipc";

export type PreviewLink =
  | { kind: "external"; target: string }
  | { kind: "document"; target: string }
  | { kind: "anchor"; target: string };

type PreviewNode = {
  type?: string;
  tagName?: string;
  value?: string;
  properties?: Record<string, unknown>;
  children?: PreviewNode[];
};

function textOf(node: PreviewNode): string {
  if (node.type === "text") return node.value ?? "";
  if (node.type === "element" && node.tagName === "img") return String(node.properties?.alt ?? "");
  return (node.children ?? []).map(textOf).join("");
}

function headingAnchors() {
  return (root: unknown) => {
    const used = new Set<string>();
    const visit = (node: PreviewNode) => {
      if (node.type === "element" && /^h[1-6]$/u.test(node.tagName ?? "")) {
        const base = textOf(node).toLowerCase().trim().replace(/[^\p{L}\p{M}\p{N}\s_-]/gu, "").replace(/\s+/gu, "-") || "section";
        let slug = base;
        let suffix = 1;
        while (used.has(slug)) slug = `${base}-${suffix++}`;
        used.add(slug);
        node.properties ??= {};
        node.properties.id = `notryx-heading-${slug}`;
        node.properties["data-notryx-heading"] = slug;
      }
      for (const child of node.children ?? []) visit(child);
    };
    visit(root as PreviewNode);
  };
}

function scrollToHeading(root: HTMLElement | null, fragment: string): void {
  const heading = [...(root?.querySelectorAll<HTMLElement>("[data-notryx-heading]") ?? [])]
    .find((element) => element.dataset.notryxHeading === fragment);
  heading?.scrollIntoView?.({ block: "start" });
}

export function classifyPreviewLink(target: string): PreviewLink | null {
  let decoded: string;
  try {
    decoded = decodeURIComponent(target);
  } catch {
    return null;
  }
  if (!target || [...decoded].some((character) => {
    const code = character.codePointAt(0)!;
    return code < 32 || code === 127 || character === "\\";
  })) return null;
  if (target.startsWith("#")) {
    const fragment = decoded.slice(1);
    return fragment ? { kind: "anchor", target: fragment } : null;
  }
  if (/^https?:\/\//iu.test(target)) {
    try {
      const url = new URL(target);
      if ((url.protocol !== "http:" && url.protocol !== "https:") || !url.hostname || url.username || url.password) return null;
      return { kind: "external", target: url.href };
    } catch {
      return null;
    }
  }
  if (/^(?:[a-z][a-z0-9+.-]*:|\/|#)/iu.test(decoded)) return null;
  const path = decoded.split(/[?#]/u, 1)[0];
  if (!path || path.split("/").some((part) => !part)) return null;
  if (/%2f/iu.test(target)) return null;
  return { kind: "document", target };
}

export type MarkdownPreviewProps = {
  source: string;
  themeBase?: "light" | "dark";
  onOpenLink?: (link: PreviewLink) => void;
  onOpenDiagram?: (target: string) => void;
  loadImage?: (target: string, signal: AbortSignal) => Promise<PreviewImageData>;
  fragment?: { fragment: string } | null;
};

const PreviewContext = createContext<{
  article: RefObject<HTMLElement | null>;
  themeBase: "light" | "dark";
  onOpenLink?: MarkdownPreviewProps["onOpenLink"];
  onOpenDiagram?: MarkdownPreviewProps["onOpenDiagram"];
  loadImage?: MarkdownPreviewProps["loadImage"];
} | null>(null);

function diagramNotice(target: string, status: PreviewImageData["drawioStatus"]): string | null {
  switch (status) {
    case "embedded_xml": return "편집 데이터 포함. 현재 미리보기만 지원합니다.";
    case "invalid_xml": return "편집 데이터가 손상되었습니다. 이미지만 표시합니다.";
    case "unsupported_xml": return "지원하지 않는 편집 데이터입니다. 이미지만 표시합니다.";
    case "limit_exceeded": return "편집 데이터가 검사 한도를 초과했습니다. 이미지만 표시합니다.";
    case "none": return /\.drawio\.png$/iu.test(decodeURIComponent(target).split(/[?#]/u, 1)[0])
      ? "편집 데이터가 없습니다. 일반 PNG로 표시합니다." : null;
  }
}

function PreviewImage({ target, alt, load }: { target: string; alt: string; load: NonNullable<MarkdownPreviewProps["loadImage"]> }) {
  const openDiagram = useContext(PreviewContext)?.onOpenDiagram;
  const [image, setImage] = useState<{ url: string; notice: string | null; editable: boolean }>();
  const [failed, setFailed] = useState(false);
  useEffect(() => {
    let active = true;
    let created: string | undefined;
    const controller = new AbortController();
    setImage(undefined);
    setFailed(false);
    void load(target, controller.signal).then(({ bytes, drawioStatus }) => {
      if (!active) return;
      created = URL.createObjectURL(new Blob([Uint8Array.from(bytes).buffer], { type: "image/png" }));
      setImage({ url: created, notice: diagramNotice(target, drawioStatus), editable: drawioStatus === "embedded_xml" });
    }).catch(() => { if (active) setFailed(true); });
    return () => { active = false; controller.abort(); if (created) URL.revokeObjectURL(created); };
  }, [target, load]);
  return image ? <span className="preview-image"><img src={image.url} alt={alt} />
    {image.notice && <small className="diagram-notice">{image.editable && openDiagram ? "편집 데이터 포함" : image.notice}</small>}
    {image.editable && openDiagram && <button type="button" className="diagram-trial" onClick={() => openDiagram(target)}>편집 (기기 초안)</button>}
  </span> : <span>{alt || "이미지"}{failed ? " (이미지를 불러올 수 없습니다)" : ""}
    {failed && openDiagram && /\.drawio\.png$/iu.test(decodeURIComponent(target).split(/[?#]/u, 1)[0])
      && <button type="button" className="diagram-trial" onClick={() => openDiagram(target)}>기기 초안 열기</button>}
  </span>;
}

function MarkdownImage({ src, alt }: { src?: string; alt?: string }) {
  const loadImage = useContext(PreviewContext)?.loadImage;
  const link = src ? classifyPreviewLink(src) : null;
  return link?.kind === "document" && loadImage
    ? <PreviewImage target={link.target} alt={alt || "이미지"} load={loadImage} />
    : <span>{alt || "이미지"}</span>;
}

function MarkdownLink({ href, children }: { href?: string; children?: ReactNode }) {
  const context = useContext(PreviewContext);
  const link = href ? classifyPreviewLink(href) : null;
  return link && (link.kind === "anchor" || context?.onOpenLink)
    ? <button type="button" onClick={() => {
      if (link.kind === "anchor") scrollToHeading(context?.article.current ?? null, link.target);
      else context?.onOpenLink?.(link);
    }}>{children}</button>
    : <span>{children}</span>;
}

function MarkdownCodeBlock({ node, children }: { node?: PreviewNode; children?: ReactNode }) {
  const themeBase = useContext(PreviewContext)?.themeBase ?? "light";
  const code = node?.children?.[0];
  const classes = code?.properties?.className;
  return node?.children?.length === 1 && code?.tagName === "code" && Array.isArray(classes) && classes.includes("language-mermaid")
    ? <MermaidPreview source={textOf(code)} theme={themeBase} />
    : <pre>{children}</pre>;
}

export function MarkdownPreview({ source, themeBase = "light", onOpenLink, onOpenDiagram, loadImage, fragment }: MarkdownPreviewProps) {
  const article = useRef<HTMLElement>(null);
  useEffect(() => { if (fragment) scrollToHeading(article.current, fragment.fragment); }, [fragment]);
  return <article ref={article} className="markdown-preview">
    <PreviewContext.Provider value={{ article, themeBase, onOpenLink, onOpenDiagram, loadImage }}>
    <Markdown
      remarkPlugins={[remarkGfm]}
      rehypePlugins={[headingAnchors]}
      skipHtml
      urlTransform={(url) => url}
      components={{
        a: MarkdownLink,
        img: MarkdownImage,
        pre: MarkdownCodeBlock,
      }}
    >{source}</Markdown>
    </PreviewContext.Provider>
  </article>;
}
