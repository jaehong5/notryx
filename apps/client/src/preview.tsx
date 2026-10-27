import { createContext, useContext, useEffect, useRef, useState, type ReactNode, type RefObject } from "react";
import Markdown from "react-markdown";
import remarkGfm from "remark-gfm";

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
  onOpenLink?: (link: PreviewLink) => void;
  loadImage?: (target: string, signal: AbortSignal) => Promise<Uint8Array>;
  fragment?: { fragment: string } | null;
};

const PreviewContext = createContext<{
  article: RefObject<HTMLElement | null>;
  onOpenLink?: MarkdownPreviewProps["onOpenLink"];
  loadImage?: MarkdownPreviewProps["loadImage"];
} | null>(null);

function PreviewImage({ target, alt, load }: { target: string; alt: string; load: (target: string, signal: AbortSignal) => Promise<Uint8Array> }) {
  const [url, setUrl] = useState<string>();
  useEffect(() => {
    let active = true;
    let created: string | undefined;
    const controller = new AbortController();
    setUrl(undefined);
    void load(target, controller.signal).then((bytes) => {
      if (!active) return;
      created = URL.createObjectURL(new Blob([Uint8Array.from(bytes).buffer], { type: "image/png" }));
      setUrl(created);
    }).catch(() => {});
    return () => { active = false; controller.abort(); if (created) URL.revokeObjectURL(created); };
  }, [target, load]);
  return url ? <img src={url} alt={alt} /> : <span>{alt || "이미지"}</span>;
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

export function MarkdownPreview({ source, onOpenLink, loadImage, fragment }: MarkdownPreviewProps) {
  const article = useRef<HTMLElement>(null);
  useEffect(() => { if (fragment) scrollToHeading(article.current, fragment.fragment); }, [fragment]);
  return <article ref={article} className="markdown-preview">
    <PreviewContext.Provider value={{ article, onOpenLink, loadImage }}>
    <Markdown
      remarkPlugins={[remarkGfm]}
      rehypePlugins={[headingAnchors]}
      skipHtml
      urlTransform={(url) => url}
      components={{
        a: MarkdownLink,
        img: MarkdownImage,
      }}
    >{source}</Markdown>
    </PreviewContext.Provider>
  </article>;
}
