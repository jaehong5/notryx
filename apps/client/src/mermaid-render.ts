import mermaid from "mermaid";
import { prepareMermaid } from "./mermaid-engine";

export type MermaidResult = { svg: string | null; error: string | null };

export async function renderMermaid(source: string, theme: "light" | "dark"): Promise<MermaidResult> {
  try {
    await prepareMermaid(source, theme);
    const { svg } = await mermaid.render("notryx-diagram", source);
    return { svg, error: null };
  } catch (error) {
    const message = String(error);
    const code = /Edge limit exceeded/iu.test(message) ? "mermaid_edge_limit"
      : /mermaid_(?:source_limit|document_config|unsupported_diagram|unsupported|edge_limit)/u.exec(message)?.[0] ?? "mermaid_parse_error";
    return { svg: null, error: code };
  }
}
