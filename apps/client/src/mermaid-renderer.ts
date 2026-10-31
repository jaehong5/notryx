import mermaid from "mermaid";
import { invoke } from "@tauri-apps/api/core";
import { prepareMermaid } from "./mermaid-engine";

declare global {
  interface Window { __NOTRYX_MERMAID__?: { source: string; theme: "light" | "dark" } }
}

async function render(): Promise<void> {
  try {
    const request = window.__NOTRYX_MERMAID__;
    delete window.__NOTRYX_MERMAID__;
    if (!request) throw new Error("mermaid_unsupported");
    await prepareMermaid(request.source, request.theme);
    const { svg } = await mermaid.render("notryx-diagram", request.source);
    await invoke("mermaid_complete", { svg, error: null });
  } catch (error) {
    const message = String(error);
    const code = /Edge limit exceeded/iu.test(message) ? "mermaid_edge_limit"
      : /mermaid_(?:source_limit|document_config|unsupported_diagram|unsupported|edge_limit)/u.exec(message)?.[0] ?? "mermaid_parse_error";
    await invoke("mermaid_complete", { svg: null, error: code }).catch(() => {});
  }
}

void render();
