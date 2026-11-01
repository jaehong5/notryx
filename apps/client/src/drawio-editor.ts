import { invoke } from "@tauri-apps/api/core";
import type { DrawioCheckpoint, DrawioEditorInput, DrawioExportRequest } from "../../../packages/protocol-types";
import { canBindDiagram, diagramEvent, MAX_DIAGRAM_XML_BYTES, type DiagramEvent } from "./drawio-bridge";
import { DiagramDraftController, type DiagramDraftState } from "./drawio-draft-controller";
import "./drawio-editor.css";

const frame = document.querySelector<HTMLIFrameElement>("#diagram")!;
const status = document.querySelector<HTMLElement>("#status")!;
const discard = document.querySelector<HTMLDialogElement>("#discard")!;
const retry = document.querySelector<HTMLButtonElement>("#retry")!;
const exportPng = document.querySelector<HTMLButtonElement>("#export-png")!;
const closeButton = document.querySelector<HTMLButtonElement>("#close")!;
const confirmClose = document.querySelector<HTMLButtonElement>("#discard-confirm")!;
let channel: MessageChannel | undefined;
let input: DrawioEditorInput | undefined;
let draft: DiagramDraftController | undefined;
let recoveryNotice = "";
let loaded = false;
let exporting = false;
let exportSequence = 0;
let pendingExport: { requestId: string; event: "snapshot" | "export"; resolve: (value: DiagramEvent) => void;
  reject: (reason: Error) => void; timer: ReturnType<typeof setTimeout> } | undefined;
const deadline = setTimeout(() => fail("편집기를 열지 못했습니다. 창을 닫고 다시 시도해 주세요."), 30_000);

function fail(message: string): void {
  clearTimeout(deadline);
  channel?.port1.close();
  frame.remove();
  status.textContent = message;
}
function draftStatus(state: DiagramDraftState): void {
  const messages = {
    pending: "변경 내용을 기기에 저장할 예정입니다.",
    saving: "기기에 초안을 저장하는 중입니다.",
    saved: "기기에 초안을 저장했습니다. 서버 원본은 변경하지 않았습니다.",
    error: "초안을 저장하지 못했습니다. 창을 유지하고 재시도해 주세요. 지원 범위를 넘긴 편집은 되돌려 주세요.",
  };
  status.textContent = recoveryNotice + messages[state];
  retry.hidden = state !== "error";
}
function askClose(): void { if (!exporting && !discard.open) discard.showModal(); }
closeButton.addEventListener("click", askClose);
document.querySelector("#keep")!.addEventListener("click", () => discard.close());
confirmClose.addEventListener("click", () => {
  confirmClose.disabled = true;
  frame.inert = true;
  void (draft?.close() ?? invoke("drawio_close")).catch(() => {
    discard.close();
    draftStatus("error");
  }).finally(() => { confirmClose.disabled = false; frame.inert = false; });
});
retry.addEventListener("click", () => { void draft?.flush().catch(() => draftStatus("error")); });
function requestExport(event: "snapshot" | "export", xml?: string): Promise<DiagramEvent> {
  const requestId = `export-${++exportSequence}`;
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      pendingExport = undefined;
      reject(new Error("export timed out"));
    }, 30_000);
    pendingExport = { requestId, event, resolve, reject, timer };
    channel!.port1.postMessage(JSON.stringify(event === "snapshot" ? { action: "snapshot", requestId }
      : { action: "export", format: "xmlpng", requestId, xml }));
  });
}
exportPng.addEventListener("click", () => {
  if (exporting || !loaded || !draft || !channel) return;
  exporting = true;
  exportPng.disabled = closeButton.disabled = retry.disabled = true;
  frame.inert = true;
  void (async () => {
    const snapshot = await requestExport("snapshot");
    if (snapshot.event !== "snapshot") throw new Error("invalid snapshot");
    draft!.change(snapshot.xml);
    const generation = await draft!.flush();
    status.textContent = recoveryNotice + "초안을 저장했습니다. PNG 파일을 만드는 중입니다.";
    const png = await requestExport("export", snapshot.xml);
    if (png.event !== "export") throw new Error("invalid PNG");
    const request: DrawioExportRequest = { generation, png_base64: png.png };
    const saved = await invoke<boolean>("drawio_export_png", { request });
    status.textContent = recoveryNotice + (saved ? "편집 가능한 PNG 사본을 내보냈습니다. 기기 초안과 서버 원본을 유지합니다."
      : "PNG 내보내기를 취소했습니다. 기기 초안은 유지합니다.");
  })().catch((error: unknown) => {
    status.textContent = recoveryNotice + (error === "drawio_export_exists"
      ? "같은 이름의 파일이 있습니다. 다른 이름을 선택해 다시 내보내 주세요. 기기 초안은 유지합니다."
      : "PNG를 내보내지 못했습니다. 초안과 편집 창은 유지합니다. 초안 저장 상태를 확인한 뒤 다시 내보내 주세요.");
  }).finally(() => {
    if (pendingExport) clearTimeout(pendingExport.timer);
    pendingExport = undefined;
    channel?.port1.postMessage(JSON.stringify({ action: "cancel-export" }));
    exporting = false;
    exportPng.disabled = closeButton.disabled = retry.disabled = false;
    frame.inert = false;
  });
});
window.addEventListener("notryx-close-requested", askClose);
window.addEventListener("pagehide", () => {
  clearTimeout(deadline);
  if (pendingExport) clearTimeout(pendingExport.timer);
  channel?.port1.close();
  draft?.dispose();
});

window.addEventListener("message", (event: MessageEvent) => {
  if (channel || !input || !canBindDiagram(event, frame.contentWindow)) return;
  channel = new MessageChannel();
  channel.port1.onmessage = (message) => {
    const value = diagramEvent(message.data);
    if (!value) return;
    if (pendingExport && "requestId" in value && value.requestId === pendingExport.requestId
      && (value.event === pendingExport.event || value.event === "export-error")) {
      const pending = pendingExport;
      pendingExport = undefined;
      clearTimeout(pending.timer);
      if (value.event === "export-error") pending.reject(new Error("export failed"));
      else pending.resolve(value);
      return;
    }
    if (value.event === "init" && input) {
      channel!.port1.postMessage(JSON.stringify({ action: "load", xml: input.xml, title: input.title,
        autosave: 1, saveAndExit: 0, noSaveBtn: 1, noExitBtn: 1 }));
      input = undefined;
    } else if (value.event === "load" && !loaded) {
      loaded = true;
      exportPng.disabled = false;
      clearTimeout(deadline);
      status.textContent = recoveryNotice + "기기 초안을 편집할 수 있습니다. 서버 원본은 변경하지 않습니다.";
    } else if (value.event === "autosave" && loaded) {
      draft?.change(value.xml);
    } else if (value.event === "draft-error") {
      draft?.reject();
      if (pendingExport) {
        clearTimeout(pendingExport.timer);
        pendingExport.reject(new Error("invalid diagram draft"));
        pendingExport = undefined;
      }
    } else if (value.event === "exit") askClose();
  };
  frame.contentWindow!.postMessage({ event: "notryx-drawio-bind" }, "*", [channel.port2]);
});

void invoke<DrawioEditorInput>("drawio_load").then((value) => {
  if (!value || typeof value.title !== "string" || typeof value.xml !== "string"
    || !value.xml || new TextEncoder().encode(value.xml).length > MAX_DIAGRAM_XML_BYTES
    || typeof value.generation !== "string" || !/^(0|[1-9][0-9]*)$/u.test(value.generation)
    || typeof value.restored !== "boolean" || typeof value.source_changed !== "boolean") throw new Error("invalid input");
  recoveryNotice = value.source_changed ? "서버 원본이 바뀌었습니다. 이전 원본의 기기 초안을 복구했습니다. "
    : value.restored ? "기기 초안을 복구했습니다. " : "";
  draft = new DiagramDraftController(value.generation, {
    save: request => invoke<DrawioCheckpoint>("drawio_checkpoint", { request }),
    close: () => invoke("drawio_close"),
    status: draftStatus,
  });
  input = value;
  frame.src = "/drawio/index.html?embed=1&proto=json&offline=1&local=1&noSaveBtn=1&saveAndExit=0&noExitBtn=1&libraries=1&lang=ko&ui=kennedy&spin=1";
}).catch(() => fail("다이어그램을 불러오지 못했습니다. 원래 문서에서 다시 열어 주세요."));
