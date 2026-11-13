import { invoke } from "@tauri-apps/api/core";
import { renderMermaid } from "./mermaid-render";

declare global {
  interface Window { __NOTRYX_MERMAID__?: { source: string; theme: "light" | "dark" } }
}

async function render(): Promise<void> {
  const request = window.__NOTRYX_MERMAID__;
  delete window.__NOTRYX_MERMAID__;
  const result = request ? await renderMermaid(request.source, request.theme)
    : { svg: null, error: "mermaid_unsupported" };
  await invoke("mermaid_complete", result).catch(() => {});
}

void render();
