import { invoke } from "@tauri-apps/api/core";
import type { DrawioEditorInput } from "../../../packages/protocol-types";
import { canBindDiagram, diagramEvent } from "./drawio-bridge";
import "./drawio-editor.css";

const frame = document.querySelector<HTMLIFrameElement>("#diagram")!;
const status = document.querySelector<HTMLElement>("#status")!;
const discard = document.querySelector<HTMLDialogElement>("#discard")!;
let channel: MessageChannel | undefined;
let input: DrawioEditorInput | undefined;
let loaded = false;
const deadline = setTimeout(() => fail("편집기를 열지 못했습니다. 창을 닫고 다시 시도해 주세요."), 30_000);

function fail(message: string): void {
  clearTimeout(deadline);
  channel?.port1.close();
  frame.remove();
  status.textContent = message;
}
function askClose(): void { if (!discard.open) discard.showModal(); }
document.querySelector("#close")!.addEventListener("click", askClose);
document.querySelector("#keep")!.addEventListener("click", () => discard.close());
document.querySelector("#discard-confirm")!.addEventListener("click", () => {
  void invoke("drawio_close").catch(() => { status.textContent = "창을 닫지 못했습니다. 다시 시도해 주세요."; });
});
window.addEventListener("notryx-close-requested", askClose);
window.addEventListener("pagehide", () => { clearTimeout(deadline); channel?.port1.close(); });

window.addEventListener("message", (event: MessageEvent) => {
  if (channel || !input || !canBindDiagram(event, frame.contentWindow)) return;
  channel = new MessageChannel();
  channel.port1.onmessage = (message) => {
    const value = diagramEvent(message.data);
    if (!value) return;
    if (value.event === "init" && input) {
      channel!.port1.postMessage(JSON.stringify({ action: "load", xml: input.xml, title: input.title,
        autosave: 0, saveAndExit: 0, noSaveBtn: 1, noExitBtn: 1 }));
      input = undefined;
    } else if (value.event === "load" && !loaded) {
      loaded = true;
      clearTimeout(deadline);
      status.textContent = "시험 편집 중입니다. 저장 기능은 아직 제공하지 않습니다.";
    } else if (value.event === "exit") askClose();
  };
  frame.contentWindow!.postMessage({ event: "notryx-drawio-bind" }, "*", [channel.port2]);
});

void invoke<DrawioEditorInput>("drawio_load").then((value) => {
  if (!value || typeof value.title !== "string" || typeof value.xml !== "string"
    || !value.xml || new TextEncoder().encode(value.xml).length > 5 * 1024 * 1024) throw new Error("invalid input");
  input = value;
  frame.src = "/drawio/index.html?embed=1&proto=json&offline=1&local=1&noSaveBtn=1&saveAndExit=0&noExitBtn=1&libraries=1&lang=ko&ui=kennedy&spin=1";
}).catch(() => fail("다이어그램을 불러오지 못했습니다. 원래 문서에서 다시 열어 주세요."));
