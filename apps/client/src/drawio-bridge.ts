export const MAX_DIAGRAM_XML_BYTES = 5 * 1024 * 1024;
const MAX_PNG_BASE64 = 4 * Math.ceil(25 * 1024 * 1024 / 3);
export type DiagramEvent = { event: "init" | "load" | "exit" | "draft-error" }
  | { event: "autosave"; xml: string }
  | { event: "snapshot"; requestId: string; xml: string }
  | { event: "export"; requestId: string; png: string }
  | { event: "export-error"; requestId: string };

export function diagramEvent(data: unknown): DiagramEvent | null {
  if (typeof data !== "string") return null;
  if (data.length > MAX_PNG_BASE64 + 1024) return { event: "draft-error" };
  try {
    const value: unknown = JSON.parse(data);
    if (!value || typeof value !== "object" || !("event" in value)) return null;
    if (value.event === "autosave" || value.event === "snapshot") {
      if (!("xml" in value) || typeof value.xml !== "string" || value.xml.length === 0
        || new TextEncoder().encode(value.xml).length > MAX_DIAGRAM_XML_BYTES) return { event: "draft-error" };
      if (value.event === "autosave") return { event: "autosave", xml: value.xml };
      return "requestId" in value && validRequestId(value.requestId)
        ? { event: "snapshot", requestId: value.requestId, xml: value.xml } : null;
    }
    if (value.event === "export" && "requestId" in value && validRequestId(value.requestId)
      && "data" in value && typeof value.data === "string" && value.data.startsWith("data:image/png;base64,")) {
      const png = value.data.slice(22);
      return png.length > 0 && png.length <= MAX_PNG_BASE64 && png.length % 4 === 0
        && /^[A-Za-z0-9+/]*={0,2}$/u.test(png) ? { event: "export", requestId: value.requestId, png } : null;
    }
    if (data.length > 4096) return null;
    if (value.event === "export-error" && "requestId" in value && validRequestId(value.requestId)) {
      return { event: "export-error", requestId: value.requestId };
    }
    return value.event === "init" || value.event === "load" || value.event === "exit" || value.event === "draft-error"
      ? { event: value.event } : null;
  } catch { return null; }
}

function validRequestId(value: unknown): value is string {
  return typeof value === "string" && /^[a-zA-Z0-9-]{1,64}$/u.test(value);
}

export function canBindDiagram(event: MessageEvent, frame: Window | null): boolean {
  return frame !== null && event.source === frame && event.origin === "null"
    && event.data?.event === "notryx-drawio-ready" && event.ports.length === 0;
}
