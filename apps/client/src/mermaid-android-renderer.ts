import { renderMermaid, type MermaidResult } from "./mermaid-render";

declare global {
  interface Window {
    __NOTRYX_ANDROID_MERMAID_START__: (request: unknown) => void;
    __NOTRYX_ANDROID_MERMAID_RESULT__: MermaidResult | null;
  }
}

window.__NOTRYX_ANDROID_MERMAID_RESULT__ = null;
let started = false;
window.__NOTRYX_ANDROID_MERMAID_START__ = (request) => {
  if (started) return;
  started = true;
  if (!request || typeof request !== "object" || !("source" in request) || !("theme" in request)
    || typeof request.source !== "string" || (request.theme !== "light" && request.theme !== "dark")) {
    window.__NOTRYX_ANDROID_MERMAID_RESULT__ = { svg: null, error: "mermaid_invalid_request" };
    return;
  }
  void renderMermaid(request.source, request.theme).then((result) => {
    window.__NOTRYX_ANDROID_MERMAID_RESULT__ = result;
  });
};
