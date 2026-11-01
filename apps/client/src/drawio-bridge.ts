export type DiagramEvent = { event: "init" | "load" | "exit" };

export function diagramEvent(data: unknown): DiagramEvent | null {
  if (typeof data !== "string" || data.length > 4096) return null;
  try {
    const value: unknown = JSON.parse(data);
    if (!value || typeof value !== "object" || !("event" in value)) return null;
    return value.event === "init" || value.event === "load" || value.event === "exit"
      ? { event: value.event } : null;
  } catch { return null; }
}

export function canBindDiagram(event: MessageEvent, frame: Window | null): boolean {
  return frame !== null && event.source === frame && event.origin === "null"
    && event.data?.event === "notryx-drawio-ready" && event.ports.length === 0;
}
