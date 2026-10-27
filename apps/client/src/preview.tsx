import { useEffect, useState, type ReactNode } from "react";
import Markdown from "react-markdown";
import remarkGfm from "remark-gfm";

export type PreviewLink =
  | { kind: "external"; target: string }
  | { kind: "document"; target: string };

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
  loadImage?: (target: string) => Promise<Uint8Array>;
};

function PreviewImage({ target, alt, load }: { target: string; alt: string; load: (target: string) => Promise<Uint8Array> }) {
  const [url, setUrl] = useState<string>();
  useEffect(() => {
    let active = true;
    let created: string | undefined;
    setUrl(undefined);
    void load(target).then((bytes) => {
      if (!active) return;
      created = URL.createObjectURL(new Blob([Uint8Array.from(bytes).buffer], { type: "image/png" }));
      setUrl(created);
    }).catch(() => {});
    return () => { active = false; if (created) URL.revokeObjectURL(created); };
  }, [target, load]);
  return url ? <img src={url} alt={alt} /> : <span>{alt || "이미지"}</span>;
}

export function MarkdownPreview({ source, onOpenLink, loadImage }: MarkdownPreviewProps) {
  return <article className="markdown-preview">
    <Markdown
      remarkPlugins={[remarkGfm]}
      skipHtml
      urlTransform={(url) => url}
      components={{
        a({ href, children }: { href?: string; children?: ReactNode }) {
          const link = href ? classifyPreviewLink(href) : null;
          return link && onOpenLink
            ? <button type="button" onClick={() => onOpenLink(link)}>{children}</button>
            : <span>{children}</span>;
        },
        img({ src, alt }: { src?: string; alt?: string }) {
          const link = src ? classifyPreviewLink(src) : null;
          return link?.kind === "document" && loadImage
            ? <PreviewImage target={link.target} alt={alt || "이미지"} load={loadImage} />
            : <span>{alt || "이미지"}</span>;
        },
      }}
    >{source}</Markdown>
  </article>;
}
