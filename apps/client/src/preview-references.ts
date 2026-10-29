import Markdown from "react-markdown";
import remarkGfm from "remark-gfm";

export const PREVIEW_REFERENCE_SOURCE_LIMIT = 256 * 1024;

export type PreviewReference = { kind: "link" | "image"; target: string };
export type PreviewReferences =
  | { status: "checked"; references: PreviewReference[] }
  | { status: "too_large" | "failed"; references: [] };

type PreviewTreeNode = {
  type?: string;
  tagName?: string;
  properties?: Record<string, unknown>;
  children?: PreviewTreeNode[];
};

export function collectPreviewReferences(source: string): PreviewReferences {
  if (source.length > PREVIEW_REFERENCE_SOURCE_LIMIT) return { status: "too_large", references: [] };
  const references: PreviewReference[] = [];
  const capture = () => (tree: unknown) => {
    const remaining = [tree as PreviewTreeNode];
    while (remaining.length) {
      const node = remaining.pop()!;
      if (node.type === "element" && (node.tagName === "a" || node.tagName === "img")
        && !(node.properties && ("dataFootnoteRef" in node.properties || "dataFootnoteBackref" in node.properties))) {
        const target = node.properties?.[node.tagName === "a" ? "href" : "src"];
        if (typeof target === "string") references.push({ kind: node.tagName === "a" ? "link" : "image", target });
      }
      if (node.children) {
        for (let index = node.children.length - 1; index >= 0; index--) remaining.push(node.children[index]);
      }
    }
  };
  try {
    Markdown({ children: source, remarkPlugins: [remarkGfm], rehypePlugins: [capture], skipHtml: true, urlTransform: (url) => url });
  } catch {
    return { status: "failed", references: [] };
  }
  return { status: "checked", references };
}
