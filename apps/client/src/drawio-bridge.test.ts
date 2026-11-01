import { expect, test } from "vitest";
import { canBindDiagram, diagramEvent } from "./drawio-bridge";

test("only binds a ready message from the opaque editor frame", () => {
  const frame = {} as Window;
  const event = { source: frame, origin: "null", data: { event: "notryx-drawio-ready" }, ports: [] } as unknown as MessageEvent;
  expect(canBindDiagram(event, frame)).toBe(true);
  expect(canBindDiagram(event, {} as Window)).toBe(false);
  expect(canBindDiagram(event, null)).toBe(false);
  expect(canBindDiagram({ ...event, origin: "tauri://localhost" }, frame)).toBe(false);
  expect(canBindDiagram({ ...event, ports: [{} as MessagePort] }, frame)).toBe(false);
});

test("ignores malformed, oversized and write requests from the editor", () => {
  for (const event of ["init", "load", "exit"]) expect(diagramEvent(JSON.stringify({ event }))).toEqual({ event });
  for (const data of [null, {}, "null", "{", JSON.stringify({ event: "save", xml: "<mxfile/>" }),
    JSON.stringify({ event: "openLink", href: "https://example.invalid" }), " ".repeat(4097)]) {
    expect(diagramEvent(data)).toBeNull();
  }
});
