import { invoke } from "@tauri-apps/api/core";
import type { DrawioCheckpoint, DrawioConflictChoice, DrawioEditorInput, DrawioExportRequest, DrawioSaveRequest, DrawioSaveState } from "../../../packages/protocol-types";
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
const saveServer = document.querySelector<HTMLButtonElement>("#save-server")!;
const serverStatus = document.querySelector<HTMLElement>("#server-status")!;
const chooseRemote = document.querySelector<HTMLButtonElement>("#choose-remote")!;
const conflictRemote = document.querySelector<HTMLDialogElement>("#conflict-remote")!;
const remoteCancel = document.querySelector<HTMLButtonElement>("#remote-cancel")!;
const remoteConfirm = document.querySelector<HTMLButtonElement>("#remote-confirm")!;
let channel: MessageChannel | undefined;
let input: DrawioEditorInput | undefined;
let draft: DiagramDraftController | undefined;
let recoveryNotice = "";
let loaded = false;
let busy = false;
let canSave = false;
let pngSource = false;
let currentGeneration = "0";
let localState: DiagramDraftState = "saved";
let savedState: DrawioSaveState | undefined;
let saveRequestFailed = false;
let statusTimer: ReturnType<typeof setInterval> | undefined;
let readingStatus = false;
let remoteChoice: DrawioConflictChoice | undefined;
let choosingRemote = false;
let remoteLoading = false;
let disposed = false;
let exportSequence = 0;
let pendingExport: { requestId: string; event: "snapshot" | "export"; resolve: (value: DiagramEvent) => void;
  reject: (reason: Error) => void; timer: ReturnType<typeof setTimeout> } | undefined;
let deadline = setTimeout(() => fail("편집기를 열지 못했습니다. 창을 닫고 다시 시도해 주세요."), 30_000);

function fail(message: string): void {
  clearTimeout(deadline);
  loaded = remoteLoading = false;
  channel?.port1.close();
  frame.remove();
  setBusy(false);
  status.textContent = message;
}
function draftStatus(state: DiagramDraftState): void {
  localState = state;
  const messages = {
    pending: "변경 내용을 기기에 저장할 예정입니다.",
    saving: "기기에 초안을 저장하는 중입니다.",
    saved: "기기에 초안을 저장했습니다.",
    error: "초안을 저장하지 못했습니다. 창을 유지하고 재시도해 주세요. 지원 범위를 넘긴 편집은 되돌려 주세요.",
  };
  status.textContent = recoveryNotice + messages[state];
  retry.hidden = state !== "error";
  renderServerStatus();
  if (state === "saved" && savedState?.status === "conflict") void refreshSaveStatus();
}
function renderServerStatus(): void {
  chooseRemote.disabled = busy || !loaded || !canSave || localState !== "saved"
    || savedState?.status !== "conflict" || !savedState.can_choose_remote;
  if (!canSave || !savedState || saveRequestFailed) return;
  const { status: result, generation } = savedState;
  serverStatus.textContent = result === "committed"
    ? savedState.matches_draft && generation === currentGeneration && localState === "saved" ? "서버에 저장했습니다."
      : "이전 저장 요청은 서버에 반영됐습니다. 이후 편집은 서버에 저장을 눌러 주세요."
    : result === "conflict" ? "서버 원본이 바뀌어 저장하지 못했습니다. 내 편집을 PNG 사본으로 내보낸 뒤 서버 원본으로 다시 열 수 있습니다."
    : result === "failed" || result === "needs_review" ? "서버 저장에 확인이 필요합니다. 기기 초안을 보존합니다."
    : result === "auth_required" ? "기기 인증을 확인해야 전송할 수 있습니다. 기기 초안은 유지합니다. 편집 창을 닫고 메인 창에서 다시 연결해 주세요."
    : result === "sync_pending" ? "동기화 확인을 기다리고 있습니다. 기기 초안은 유지합니다."
    : result === "pending" ? "서버 전송 대기 중입니다. 이후 편집은 전송 완료 후 다시 저장해 주세요."
    : "서버에 저장을 누르면 현재 초안을 전송합니다.";
}
async function refreshSaveStatus(): Promise<void> {
  if (!canSave || !loaded || disposed || readingStatus || busy) return;
  readingStatus = true;
  try {
    const value = await invoke<DrawioSaveState>("drawio_save_status");
    if (!disposed && !busy) { savedState = value; renderServerStatus(); }
  } catch {
    if (!disposed) serverStatus.textContent = "서버 저장 상태를 확인하지 못했습니다. 기기 초안은 유지합니다.";
  } finally { readingStatus = false; }
}
function setBusy(value: boolean): void {
  busy = value;
  exportPng.disabled = value || !loaded;
  saveServer.disabled = value || !loaded || !canSave;
  closeButton.disabled = retry.disabled = confirmClose.disabled = value;
  frame.inert = value || !loaded;
  renderServerStatus();
}
function askClose(): void { if (!busy && !discard.open) discard.showModal(); }
closeButton.addEventListener("click", askClose);
document.querySelector("#keep")!.addEventListener("click", () => discard.close());
confirmClose.addEventListener("click", () => {
  if (busy) return;
  setBusy(true);
  void (async () => {
    if (draft && loaded) await checkpointSnapshot();
    await (draft?.close() ?? invoke("drawio_close"));
  })().catch(() => {
    discard.close();
    draftStatus("error");
  }).finally(() => setBusy(false));
});
retry.addEventListener("click", () => { void draft?.flush().catch(() => draftStatus("error")); });
function cancelRemote(): void {
  if (choosingRemote) return;
  remoteChoice = undefined;
  conflictRemote.close();
  setBusy(false);
  void refreshSaveStatus();
}
remoteCancel.addEventListener("click", cancelRemote);
conflictRemote.addEventListener("cancel", event => { event.preventDefault(); cancelRemote(); });
chooseRemote.addEventListener("click", () => {
  if (busy || !loaded || chooseRemote.disabled) return;
  setBusy(true);
  void (async () => {
    const { generation } = await checkpointSnapshot();
    remoteChoice = await invoke<DrawioConflictChoice>("drawio_prepare_conflict_remote", { request: { generation } });
    conflictRemote.showModal();
  })().catch(() => {
    status.textContent = "서버 원본을 다시 열 준비를 마치지 못했습니다. 기기 초안은 유지합니다. 최신 편집을 PNG 사본으로 내보낸 뒤 다시 시도해 주세요.";
    setBusy(false);
    void refreshSaveStatus();
  });
});
remoteConfirm.addEventListener("click", () => {
  if (!remoteChoice || choosingRemote) return;
  choosingRemote = true;
  remoteCancel.disabled = remoteConfirm.disabled = true;
  // The previous frame must not checkpoint XML into the replacement generation.
  loaded = false;
  let replaced = false;
  void (async () => {
    const value = await invoke<DrawioEditorInput>("drawio_choose_conflict_remote", { request: remoteChoice });
    replaced = true;
    draft?.dispose();
    draft = undefined;
    validateInput(value);
    setSaveMode(value.title);
    draft = createDraft(value.generation);
    currentGeneration = value.generation;
    localState = "saved";
    savedState = undefined;
    saveRequestFailed = false;
    recoveryNotice = "서버 원본을 다시 불러왔습니다. 내 편집은 PNG 사본에 보존했습니다. ";
    remoteLoading = true;
    status.textContent = "서버 원본을 편집기에 불러오는 중입니다.";
    serverStatus.textContent = "서버 저장 상태를 확인하고 있습니다.";
    deadline = setTimeout(() => fail("서버 원본을 편집기에 불러오지 못했습니다. 기기 초안과 PNG 사본은 보존했습니다. 창을 닫고 다시 열어 주세요."), 30_000);
    loadDiagram(value);
  })().catch(() => {
    if (replaced) {
      fail("서버 원본을 편집기에 불러오지 못했습니다. PNG 사본을 보존하고 창을 닫은 뒤 다시 열어 주세요.");
    } else {
      loaded = true;
      status.textContent = "서버 원본을 다시 열지 못했습니다. 기기 초안과 충돌 상태를 유지합니다. 서버 상태를 확인한 뒤 다시 시도해 주세요.";
    }
  }).finally(() => {
    remoteChoice = undefined;
    choosingRemote = false;
    remoteCancel.disabled = remoteConfirm.disabled = false;
    conflictRemote.close();
    if (!remoteLoading) { setBusy(false); void refreshSaveStatus(); }
  });
});
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
async function checkpointSnapshot(): Promise<{ xml: string; generation: string }> {
  const snapshot = await requestExport("snapshot");
  if (snapshot.event !== "snapshot" || !draft) throw new Error("invalid snapshot");
  draft.change(snapshot.xml);
  const generation = await draft.flush();
  return { xml: snapshot.xml, generation };
}
saveServer.addEventListener("click", () => {
  if (busy || !loaded || !draft || !channel || !canSave) return;
  saveRequestFailed = false;
  setBusy(true);
  // Status text changes the viewport dimensions serialized into draw.io XML.
  serverStatus.textContent = "서버 저장을 준비하고 있습니다.";
  void (async () => {
    const { xml, generation } = await checkpointSnapshot();
    const request: DrawioSaveRequest = { generation, png_base64: null };
    if (pngSource) {
      const png = await requestExport("export", xml);
      if (png.event !== "export") throw new Error("invalid PNG");
      request.png_base64 = png.png;
    }
    savedState = await invoke<DrawioSaveState>("drawio_save", { request });
    recoveryNotice = "";
    renderServerStatus();
  })().catch(() => {
    saveRequestFailed = true;
    serverStatus.textContent = "서버 저장을 요청하지 못했습니다. 기기 초안을 유지하고 다시 시도해 주세요.";
  }).finally(() => {
    if (pendingExport) clearTimeout(pendingExport.timer);
    pendingExport = undefined;
    if (pngSource) channel?.port1.postMessage(JSON.stringify({ action: "cancel-export" }));
    setBusy(false);
  });
});
exportPng.addEventListener("click", () => {
  if (busy || !loaded || !draft || !channel) return;
  setBusy(true);
  void (async () => {
    const { xml, generation } = await checkpointSnapshot();
    status.textContent = recoveryNotice + "초안을 저장했습니다. PNG 파일을 만드는 중입니다.";
    const png = await requestExport("export", xml);
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
    setBusy(false);
    void refreshSaveStatus();
  });
});
window.addEventListener("notryx-close-requested", askClose);
window.addEventListener("pagehide", () => {
  disposed = true;
  clearInterval(statusTimer);
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
      loadDiagram(input);
      input = undefined;
    } else if (value.event === "load" && !loaded && frame.isConnected && (!choosingRemote || remoteLoading)) {
      loaded = true;
      remoteLoading = false;
      setBusy(false);
      clearTimeout(deadline);
      status.textContent = recoveryNotice + "기기 초안을 편집할 수 있습니다.";
      void refreshSaveStatus();
    } else if (value.event === "autosave" && loaded) {
      draft?.change(value.xml);
    } else if (value.event === "draft-error" && remoteLoading) {
      fail("서버 원본을 편집기에 불러오지 못했습니다. 기기 초안과 PNG 사본은 보존했습니다. 창을 닫고 다시 열어 주세요.");
    } else if (value.event === "draft-error" && loaded) {
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

function loadDiagram(value: DrawioEditorInput): void {
  channel!.port1.postMessage(JSON.stringify({ action: "load", xml: value.xml, title: value.title,
    autosave: 1, saveAndExit: 0, noSaveBtn: 1, noExitBtn: 1 }));
}
function validateInput(value: DrawioEditorInput): void {
  if (!value || typeof value.title !== "string" || typeof value.xml !== "string"
    || !value.xml || new TextEncoder().encode(value.xml).length > MAX_DIAGRAM_XML_BYTES
    || typeof value.generation !== "string" || !/^(0|[1-9][0-9]*)$/u.test(value.generation)
    || typeof value.restored !== "boolean" || typeof value.source_changed !== "boolean"
    || typeof value.can_save !== "boolean") throw new Error("invalid input");
}
function createDraft(generation: string): DiagramDraftController {
  return new DiagramDraftController(generation, {
    save: async request => {
      const result = await invoke<DrawioCheckpoint>("drawio_checkpoint", { request });
      currentGeneration = result.generation;
      return result;
    },
    close: () => invoke("drawio_close"),
    status: draftStatus,
  });
}
function setSaveMode(title: string): void {
  pngSource = title.toLowerCase().endsWith(".png");
  document.querySelector("#save-mode")!.textContent = pngSource
    ? "편집 가능한 PNG 원본은 서버에 저장을 눌러 반영합니다."
    : "XML 원본은 서버에 저장을 눌러 반영합니다.";
}
void invoke<DrawioEditorInput>("drawio_load").then((value) => {
  validateInput(value);
  canSave = value.can_save;
  setSaveMode(value.title);
  currentGeneration = value.generation;
  saveServer.hidden = serverStatus.hidden = chooseRemote.hidden = !canSave;
  if (canSave) serverStatus.textContent = "서버 저장 상태를 확인하고 있습니다.";
  if (canSave) statusTimer = setInterval(() => { void refreshSaveStatus(); }, 2000);
  recoveryNotice = value.source_changed ? "기기에 받은 서버 정보와 초안의 기준이 다릅니다. 기기 초안을 복구했습니다. "
    : value.restored ? "기기 초안을 복구했습니다. " : "";
  draft = createDraft(value.generation);
  input = value;
  frame.src = "/drawio/index.html?embed=1&proto=json&offline=1&local=1&noSaveBtn=1&saveAndExit=0&noExitBtn=1&libraries=1&lang=ko&ui=kennedy&spin=1";
}).catch(() => fail("다이어그램을 불러오지 못했습니다. 원래 문서에서 다시 열어 주세요."));
