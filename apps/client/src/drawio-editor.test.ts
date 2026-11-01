// @vitest-environment jsdom
import { readFileSync } from "node:fs";
import { afterEach, beforeEach, expect, test, vi, type MockInstance } from "vitest";

const { invoke } = vi.hoisted(() => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke }));

const html = readFileSync("apps/client/drawio.html", "utf8");
const latestXml = '<mxfile><diagram>닫기 직전 입력</diagram></mxfile>';
let sent: Array<{ action: string; requestId?: string; xml?: string }>;
let receive: (value: object) => void;
let listeners: MockInstance<typeof window.addEventListener>;

function button(id: string): HTMLButtonElement { return document.querySelector(`#${id}`)!; }
function dialog(): HTMLDialogElement { return document.querySelector("#discard")!; }
const settle = () => vi.advanceTimersByTimeAsync(0);

beforeEach(() => {
  vi.resetModules();
  vi.useFakeTimers();
  sent = [];
  listeners = vi.spyOn(window, "addEventListener");
  vi.stubGlobal("MessageChannel", class {
    port1 = {
      onmessage: undefined as ((event: MessageEvent) => void) | undefined,
      postMessage: (data: string) => { sent.push(JSON.parse(data)); },
      close: () => {},
    };
    port2 = {};
    constructor() { receive = value => this.port1.onmessage?.({ data: JSON.stringify(value) } as MessageEvent); }
  });
  document.body.innerHTML = new DOMParser().parseFromString(html, "text/html").body.innerHTML;
  dialog().showModal = () => { dialog().open = true; };
  dialog().close = () => { dialog().open = false; };
  invoke.mockReset().mockImplementation(async (command: string) => {
    if (command === "drawio_load") return {
      title: "Synthetic.drawio", xml: "<mxfile/>", generation: "0", restored: false, source_changed: false,
      can_save: true,
    };
    if (command === "drawio_checkpoint") return { generation: "1" };
    if (command === "drawio_save_status") return { generation: null, matches_draft: false, status: "idle" };
    if (command === "drawio_save") return { generation: "1", matches_draft: true, status: "pending" };
    return undefined;
  });
});

test("server save queues only the durable latest snapshot and keeps the window open", async () => {
  await open();
  button("save-server").click();
  expect(invoke.mock.calls.some(([command]) => command === "drawio_save")).toBe(false);
  receive({ event: "snapshot", requestId: snapshot(), xml: latestXml });
  await settle();
  expect(invoke).toHaveBeenCalledWith("drawio_save", { request: { generation: "1", png_base64: null } });
  expect(invoke).not.toHaveBeenCalledWith("drawio_close");
});

test("failed local checkpoint never queues a server write", async () => {
  await open();
  invoke.mockImplementation(async (command: string) => {
    if (command === "drawio_checkpoint") throw new Error("disk full");
  });
  button("save-server").click();
  receive({ event: "snapshot", requestId: snapshot(), xml: latestXml });
  await settle();
  expect(invoke.mock.calls.some(([command]) => command === "drawio_save")).toBe(false);
  expect(button("close").disabled).toBe(false);
  expect(document.querySelector("#status")!.textContent).toContain("초안을 저장하지 못했습니다");
});

test("a committed older snapshot does not label subsequent edits as saved to server", async () => {
  await open();
  invoke.mockImplementation(async (command: string) => {
    if (command === "drawio_checkpoint") return { generation: "1" };
    return { generation: "1", matches_draft: true, status: "committed" };
  });
  button("save-server").click();
  receive({ event: "snapshot", requestId: snapshot(), xml: latestXml });
  await settle();
  expect(document.querySelector("#server-status")!.textContent).toBe("서버에 저장했습니다.");
  receive({ event: "autosave", xml: "<mxfile><diagram>추가 입력</diagram></mxfile>" });
  expect(document.querySelector("#server-status")!.textContent).toContain("이후 편집");
});

test("a conflict preserves the open editor and reports both copies are retained", async () => {
  await open();
  invoke.mockImplementation(async (command: string) => command === "drawio_checkpoint"
    ? { generation: "1" } : { generation: "1", matches_draft: true, status: "conflict" });
  button("save-server").click();
  receive({ event: "snapshot", requestId: snapshot(), xml: latestXml });
  await settle();
  expect(document.querySelector("#server-status")!.textContent).toContain("서버 원본이 바뀌어");
  expect(document.querySelector("#diagram")).not.toBeNull();
  expect(invoke).not.toHaveBeenCalledWith("drawio_close");
});

test("PNG server save exports the durable snapshot before queuing original PNG bytes", async () => {
  const previous = invoke.getMockImplementation()!;
  invoke.mockImplementation((command: string) => command === "drawio_load"
    ? Promise.resolve({ title: "Image.drawio.png", xml: "<mxfile/>", generation: "0",
      restored: false, source_changed: false, can_save: true }) : previous(command));
  await open();
  button("save-server").click();
  expect(document.querySelector("#server-status")!.textContent).toContain("서버 저장을 준비");
  receive({ event: "snapshot", requestId: snapshot(), xml: latestXml });
  await settle();
  expect(invoke.mock.calls.some(([command]) => command === "drawio_save")).toBe(false);
  const request = sent.filter(value => value.action === "export").at(-1)!;
  expect(request.xml).toBe(latestXml);
  receive({ event: "export", requestId: request.requestId, data: "data:image/png;base64,aGVsbG8=" });
  await settle();
  expect(invoke).toHaveBeenCalledWith("drawio_save", { request: { generation: "1", png_base64: "aGVsbG8=" } });
  expect(invoke).not.toHaveBeenCalledWith("drawio_export_png", expect.anything());
  expect(button("close").disabled).toBe(false);
});

test("PNG generation failure keeps the checkpoint and never requests server storage", async () => {
  const previous = invoke.getMockImplementation()!;
  invoke.mockImplementation((command: string) => command === "drawio_load"
    ? Promise.resolve({ title: "Image.drawio.png", xml: "<mxfile/>", generation: "0",
      restored: false, source_changed: false, can_save: true }) : previous(command));
  await open();
  button("save-server").click();
  receive({ event: "snapshot", requestId: snapshot(), xml: latestXml });
  await settle();
  const request = sent.filter(value => value.action === "export").at(-1)!;
  receive({ event: "export-error", requestId: request?.requestId });
  await settle();
  expect(invoke).toHaveBeenCalledWith("drawio_checkpoint", { request: { generation: "0", xml: latestXml } });
  expect(invoke.mock.calls.some(([command]) => command === "drawio_save")).toBe(false);
  expect(button("close").disabled).toBe(false);
  expect(document.querySelector("#server-status")!.textContent).toContain("기기 초안");
  await vi.advanceTimersByTimeAsync(2000);
  expect(document.querySelector("#server-status")!.textContent).toContain("서버 저장을 요청하지 못했습니다.");
});

afterEach(() => {
  window.dispatchEvent(new Event("pagehide"));
  for (const [type, listener, options] of listeners.mock.calls) window.removeEventListener(type, listener, options);
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
  vi.useRealTimers();
  document.body.innerHTML = "";
});

async function open(loaded = true): Promise<void> {
  await import("./drawio-editor");
  await settle();
  if (!loaded) return;
  const frame = document.querySelector<HTMLIFrameElement>("#diagram")!;
  vi.spyOn(frame.contentWindow!, "postMessage").mockImplementation(() => {});
  window.dispatchEvent(new MessageEvent("message", {
    source: frame.contentWindow, origin: "null", data: { event: "notryx-drawio-ready" },
  }));
  receive({ event: "init" });
  receive({ event: "load" });
}

async function confirmClose(): Promise<void> {
  button("close").click();
  button("discard-confirm").click();
  await settle();
}

function snapshot(): string {
  const request = sent.filter(value => value.action === "snapshot").at(-1);
  expect(request).toBeDefined();
  return request!.requestId!;
}

test("close checkpoints active label input before closing and prevents overlapping export", async () => {
  await open();
  let finish!: (value: { generation: string }) => void;
  invoke.mockImplementation((command: string) => command === "drawio_checkpoint"
    ? new Promise(resolve => { finish = resolve; }) : Promise.resolve());
  await confirmClose();
  expect(invoke).not.toHaveBeenCalledWith("drawio_close");
  expect(button("export-png").disabled).toBe(true);
  const requestId = snapshot();
  receive({ event: "snapshot", requestId: "unrelated", xml: "<stale/>" });
  await settle();
  expect(invoke.mock.calls.filter(([command]) => command === "drawio_checkpoint")).toHaveLength(0);
  receive({ event: "snapshot", requestId, xml: latestXml });
  await settle();
  expect(invoke).toHaveBeenCalledWith("drawio_checkpoint", { request: { generation: "0", xml: latestXml } });
  expect(invoke).not.toHaveBeenCalledWith("drawio_close");
  finish({ generation: "1" });
  await settle();
  expect(invoke).toHaveBeenCalledWith("drawio_close");
});

test("snapshot failure keeps the editor open and permits another close attempt", async () => {
  await open();
  await confirmClose();
  receive({ event: "export-error", requestId: snapshot() });
  await settle();
  expect(invoke).not.toHaveBeenCalledWith("drawio_close");
  expect(dialog().open).toBe(false);
  expect(document.querySelector("#status")!.textContent).toContain("초안을 저장하지 못했습니다.");
  expect(document.querySelector<HTMLIFrameElement>("#diagram")!.inert).toBe(false);
  await confirmClose();
  receive({ event: "snapshot", requestId: snapshot(), xml: latestXml });
  await settle();
  expect(invoke).toHaveBeenCalledWith("drawio_close");
});

test("checkpoint failure preserves the captured input and leaves the editor open", async () => {
  await open();
  invoke.mockImplementation(async (command: string) => {
    if (command === "drawio_checkpoint") throw new Error("disk full");
  });
  await confirmClose();
  receive({ event: "snapshot", requestId: snapshot(), xml: latestXml });
  await settle();
  expect(invoke).not.toHaveBeenCalledWith("drawio_close");
  expect(dialog().open).toBe(false);
  invoke.mockResolvedValue({ generation: "1" });
  button("retry").click();
  await settle();
  expect(invoke).toHaveBeenLastCalledWith("drawio_checkpoint", { request: { generation: "0", xml: latestXml } });
  expect(invoke).not.toHaveBeenCalledWith("drawio_close");
});

test("a not-yet-loaded editor can close without waiting for a snapshot", async () => {
  await open(false);
  await confirmClose();
  expect(sent).toHaveLength(0);
  expect(invoke).toHaveBeenCalledWith("drawio_close");
});

test("PNG export still uses the same saved XML and durable generation", async () => {
  await open();
  button("export-png").click();
  receive({ event: "snapshot", requestId: snapshot(), xml: latestXml });
  await settle();
  const request = sent.filter(value => value.action === "export").at(-1)!;
  expect(request.xml).toBe(latestXml);
  receive({ event: "export", requestId: request.requestId, data: "data:image/png;base64,aGVsbG8=" });
  await settle();
  expect(invoke).toHaveBeenCalledWith("drawio_export_png", { request: { generation: "1", png_base64: "aGVsbG8=" } });
  expect(invoke).not.toHaveBeenCalledWith("drawio_close");
  expect(button("close").disabled).toBe(false);
});
