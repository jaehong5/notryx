import mermaid from "mermaid";
import { checkMermaidSource, MERMAID_SOURCE_BYTES } from "./mermaid-policy";

export async function prepareMermaid(source: string, theme: "light" | "dark"): Promise<void> {
  checkMermaidSource(source);
  mermaid.initialize({
    startOnLoad: false, securityLevel: "strict", htmlLabels: false,
    flowchart: { htmlLabels: false }, theme: theme === "dark" ? "dark" : "default",
    fontFamily: "system-ui, sans-serif", maxTextSize: MERMAID_SOURCE_BYTES, maxEdges: 500,
    suppressErrorRendering: true,
    secure: ["secure", "securityLevel", "startOnLoad", "htmlLabels", "flowchart", "maxTextSize", "maxEdges", "suppressErrorRendering"],
  });
  const diagram = await mermaid.mermaidAPI.getDiagramFromText(source);
  const db = diagram.db as {
    getEdges?: () => unknown; getMessages?: () => unknown;
    getRelations?: () => unknown; getRelationships?: () => unknown;
    getData?: () => { edges?: unknown };
  };
  let edges: unknown;
  // These accessors belong to the pinned engine. Unverified diagram types stay as source.
  switch (diagram.type) {
    case "flowchart-v2": case "flowchart": edges = db.getEdges?.(); break;
    case "sequence": edges = db.getMessages?.(); break;
    case "classDiagram": case "class": edges = db.getRelations?.(); break;
    case "stateDiagram": case "state": edges = db.getData?.().edges; break;
    case "er": edges = db.getRelationships?.(); break;
    default: throw new Error("mermaid_unsupported_diagram");
  }
  if (!Array.isArray(edges)) throw new Error("mermaid_unsupported_diagram");
  if (edges.length > 500) throw new Error("mermaid_edge_limit");
}
