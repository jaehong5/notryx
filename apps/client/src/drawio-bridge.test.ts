import { expect, test } from "vitest";
import { canBindDiagram, diagramEvent, MAX_DIAGRAM_XML_BYTES } from "./drawio-bridge";

test("only binds a ready message from the opaque editor frame", () => {
  const frame = {} as Window;
  const event = { source: frame, origin: "null", data: { event: "notryx-drawio-ready" }, ports: [] } as unknown as MessageEvent;
  expect(canBindDiagram(event, frame)).toBe(true);
  expect(canBindDiagram(event, {} as Window)).toBe(false);
  expect(canBindDiagram(event, null)).toBe(false);
  expect(canBindDiagram({ ...event, origin: "tauri://localhost" }, frame)).toBe(false);
  expect(canBindDiagram({ ...event, ports: [{} as MessagePort] }, frame)).toBe(false);
});

test("accepts bounded autosave XML while keeping control messages small", () => {
  const xml = "<mxfile>" + " ".repeat(6000) + "</mxfile>";
  expect(diagramEvent(JSON.stringify({ event: "autosave", xml }))).toEqual({ event: "autosave", xml });
  expect(diagramEvent(JSON.stringify({ event: "load", xml }))).toBeNull();
  for (const value of [undefined, "", 5, "한".repeat(Math.floor(MAX_DIAGRAM_XML_BYTES / 3) + 1)]) {
    expect(diagramEvent(JSON.stringify({ event: "autosave", xml: value }))).toEqual({ event: "draft-error" });
  }
});

test("ignores malformed, oversized and write requests from the editor", () => {
  for (const event of ["init", "load", "exit"]) expect(diagramEvent(JSON.stringify({ event }))).toEqual({ event });
  for (const data of [null, {}, "null", "{", JSON.stringify({ event: "save", xml: "<mxfile/>" }),
    JSON.stringify({ event: "openLink", href: "https://example.invalid" }), " ".repeat(4097)]) {
    expect(diagramEvent(data)).toBeNull();
  }
});

test("accepts only correlated bounded PNG export and XML snapshot responses", () => {
  expect(diagramEvent(JSON.stringify({ event: "snapshot", requestId: "export-1", xml: "<mxfile/>" })))
    .toEqual({ event: "snapshot", requestId: "export-1", xml: "<mxfile/>" });
  expect(diagramEvent(JSON.stringify({ event: "export", requestId: "export-1", data: "data:image/png;base64,aGVsbG8=" })))
    .toEqual({ event: "export", requestId: "export-1", png: "aGVsbG8=" });
  for (const data of ["https://example.invalid/a.png", "data:image/svg+xml;base64,aGVsbG8=", "data:image/png;base64,!"]) {
    expect(diagramEvent(JSON.stringify({ event: "export", requestId: "export-1", data }))).toBeNull();
  }
  expect(diagramEvent(JSON.stringify({ event: "export", data: "data:image/png;base64,aGVsbG8=" }))).toBeNull();
});
