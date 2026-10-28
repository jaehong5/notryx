import { createRoot } from "react-dom/client";
import { useCallback, useEffect, useMemo, useRef, useState, type FormEvent } from "react";
import { Compartment, EditorState } from "@codemirror/state";
import { EditorView, keymap, lineNumbers, drawSelection } from "@codemirror/view";
import { defaultKeymap, history, historyKeymap } from "@codemirror/commands";
import { markdown } from "@codemirror/lang-markdown";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { EditorController, initialEditorState } from "./editor-controller";
import { openDocument, saveDocument, closeDocument, pairWorkspace, workspaceInventory, openWorkspaceDocument, saveWorkspaceDocument, closeWorkspaceDocument, localDrafts, openLocalDraft, adoptPendingPair, discardPendingPair, pendingPairStatus, queueWorkspaceDocument, sendNextWorkspaceOperation, previewWorkspaceImage, workspaceDocumentOutbox, openWorkspaceExternal, recoverWorkspaceSource } from "./ipc";
import { MarkdownPreview } from "./preview";
import { PreviewImageQueue } from "./preview-image-queue";
import { WorkspaceListLoader, workspacePath, draftKey, draftSelectionId } from "./workspace-list";
import { resolveWorkspaceReference } from "./workspace-reference";
import { AutoTransmit, QueueStorageError, type TransmitScope } from "./auto-transmit";
import { clearQueueError, putQueueError, QueueAlerts, type QueueError } from "./queue-errors";
import { receiptWaitResult, staleTransmissionResult } from "./send-result";
import { documentSizeGuard } from "./document-size-guard";
import { StartupGate } from "./migration-recovery";
import type { NodeState, WorkspaceDraft, WorkspaceRegistration } from "../../../packages/protocol-types";
import "./style.css";

const documents = [
  ["welcome", "시작하기"], ["unicode", "한글과 원문 보존"], ["100k", "100 KiB 문서"],
  ["100k-b", "100 KiB 문서 B"], ["5m", "5 MiB 문서"], ["5m-b", "5 MiB 문서 B"],
  ["mixed", "혼합 줄바꿈"], ["non-utf8", "비UTF-8 원문"],
];

function App() {
  const parent = useRef<HTMLDivElement>(null);
  const controller = useRef<EditorController | null>(null);
  const transmitter = useRef<AutoTransmit | null>(null);
  const source = useRef("");
  const previewEnabled = useRef(false);
  const previewTimer = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);
  const modeRef = useRef<"fixture" | "workspace">("fixture");
  const nodesRef = useRef<NodeState[]>([]);
  const listLoader = useRef(new WorkspaceListLoader());
  const sessionModes = useRef(new Map<string, { mode: "fixture" | "workspace"; documentId: string; uiId: string; scope?: TransmitScope; binding?: WorkspaceRegistration }>());
  const syncRevision = useRef(0);
  const selectedRef = useRef("");
  const imageQueue = useRef(new PreviewImageQueue());
  const draftsRef = useRef<WorkspaceDraft[]>([]);
  const connecting = useRef(false);
  const registrationVersion = useRef(0);
  const [state, setState] = useState(initialEditorState);
  const [preview, setPreview] = useState(false);
  const [previewSource, setPreviewSource] = useState("");
  const [previewAnchor, setPreviewAnchor] = useState<{ documentId: string; fragment: string } | null>(null);
  const [probeError, setProbeError] = useState("");
  const [isolation, setIsolation] = useState(false);
  const [mode, setMode] = useState<"fixture" | "workspace">("fixture");
  const [nodes, setNodes] = useState<NodeState[]>([]);
  const [workspacePageStart, setWorkspacePageStart] = useState<string | null>(null);
  const [workspaceNextPage, setWorkspaceNextPage] = useState<string | null>(null);
  const [drafts, setDrafts] = useState<WorkspaceDraft[]>([]);
  const [connectionError, setConnectionError] = useState("");
  const [queueErrors, setQueueErrors] = useState<QueueError[]>([]);
  const [pendingPair, setPendingPair] = useState(false);
  const [busy, setBusy] = useState(true);
  const [syncStatus, setSyncStatus] = useState("");
  const [recoveryMessage, setRecoveryMessage] = useState("");
  const [recovering, setRecovering] = useState(false);
  const recoveringRef = useRef(false);

  function reportQueueError(scope: TransmitScope, message: string): void {
    setQueueErrors((errors) => putQueueError(errors, scope, message));
  }

  async function recoverSource(action: "copy" | "export") {
    if (recoveringRef.current) return;
    recoveringRef.current = true;
    setRecovering(true);
    setRecoveryMessage("");
    try {
      const snapshot = controller.current?.recoverySnapshot();
      const session = snapshot && sessionModes.current.get(snapshot.session);
      if (!snapshot || !session || session.mode !== "workspace" || session.uiId !== snapshot.id) throw new Error("열린 문서를 다시 확인해 주세요.");
      const completed = await recoverWorkspaceSource(session.documentId, snapshot.session, snapshot.bytes, action);
      if (completed) {
        const current = controller.current?.isCurrentSnapshot(snapshot.session, snapshot.generation);
        const sameDocument = controller.current?.isCurrentSession(snapshot.session);
        const previous = action === "copy" ? "시작 당시 원문을 복사했습니다." : "시작 당시 원문을 파일에 내보냈습니다.";
        setRecoveryMessage(current ? (action === "copy" ? "원문을 복사했습니다." : "선택한 파일에 원문을 내보냈습니다.")
          : sameDocument ? `${previous} 이후 입력은 포함되지 않았습니다.` : `이전에 열었던 문서의 ${previous}`);
      }
    } catch (cause) {
      const error = String(cause);
      if (/recovery_copy_not_utf8|recovery_copy_contains_nul/.test(error)) setRecoveryMessage("이 원문은 텍스트로 복사할 수 없습니다. 파일로 내보내세요.");
      else if (error.includes("recovery_too_large")) setRecoveryMessage("복구 원문이 64 MiB를 초과해 내보내거나 복사할 수 없습니다.");
      else if (error.includes("recovery_export_exists")) setRecoveryMessage("이미 있는 파일은 변경하지 않았습니다. 다른 이름으로 내보내세요.");
      else setRecoveryMessage(`원문 복구 작업에 실패했습니다: ${error}`);
    } finally {
      recoveringRef.current = false;
      setRecovering(false);
    }
  }

  async function refreshWorkspace(startAfter: string | null = null): Promise<void> {
    const result = await listLoader.current.load(workspaceInventory, startAfter);
    if (!listLoader.current.isCurrent(result.revision)) throw new Error("stale_workspace_list");
    nodesRef.current = result.nodes;
    setNodes(result.nodes);
    setWorkspacePageStart(startAfter);
    setWorkspaceNextPage(result.nextAfterId);
  }

  async function refreshDrafts(): Promise<void> {
    const result = await localDrafts();
    draftsRef.current = result;
    setDrafts(result);
  }

  async function refreshSyncStatus(): Promise<void> {
    const revision = ++syncRevision.current;
    const current = [...sessionModes.current.entries()].find(([session]) => controller.current?.isCurrentSession(session));
    if (!current || !current[1].binding) return;
    const [sessionId, session] = current;
    const binding = session.binding;
    if (!binding) return;
    const result = await workspaceDocumentOutbox(binding, session.documentId);
    if (revision !== syncRevision.current || !controller.current?.isCurrentSession(sessionId)) return;
    if (result.has_conflict) setSyncStatus("충돌 있음");
    else if (result.has_failure || result.latest?.frozen) setSyncStatus("서버 결과 대조 필요");
    else {
      const last = result.latest;
      if (!last) setSyncStatus("");
      else if (last.status === "committed" && controller.current?.isCurrentSaved(sessionId, last.generation)) {
        setSyncStatus(BigInt(last.operation_seq) <= BigInt(result.confirmed_through) ? "서버 반영됨" : "서버 저장됨, 영수증 확인 중");
      }
      else setSyncStatus("서버 반영 대기");
    }
  }

  async function connect(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (connecting.current) return;
    const form = event.currentTarget;
    const fields = new FormData(form);
    const origin = String(fields.get("origin") ?? "").trim();
    const workspaceId = String(fields.get("workspace_id") ?? "").trim().toLowerCase();
    const secret = String(fields.get("pairing_secret") ?? "");
    const secretInput = form.elements.namedItem("pairing_secret");
    if (secretInput instanceof HTMLInputElement) secretInput.value = "";
    connecting.current = true;
    registrationVersion.current++;
    listLoader.current.invalidate();
    setBusy(true);
    setConnectionError("");
    try {
      if (!await controller.current?.prepareForWorkspaceChange()) return;
      await pairWorkspace(origin, workspaceId, secret);
      modeRef.current = "workspace";
      nodesRef.current = [];
      setNodes([]);
      setWorkspacePageStart(null);
      setWorkspaceNextPage(null);
      setMode("workspace");
      await refreshDrafts();
      await refreshWorkspace();
      transmitter.current?.resume();
    } catch (cause) {
      if (String(cause).includes("pairing_pending_adoption") || String(cause).includes("pairing_in_progress")) setPendingPair(true);
      setConnectionError(String(cause));
    } finally {
      connecting.current = false;
      setBusy(false);
    }
  }

  async function resumePair(discard: boolean): Promise<void> {
    if (connecting.current) return;
    connecting.current = true;
    registrationVersion.current++;
    listLoader.current.invalidate();
    setBusy(true);
    setConnectionError("");
    try {
      if (!await controller.current?.prepareForWorkspaceChange()) return;
      if (discard) {
        await discardPendingPair();
      } else {
        await adoptPendingPair();
        setPendingPair(false);
        modeRef.current = "workspace";
        nodesRef.current = [];
        setNodes([]);
        setWorkspacePageStart(null);
        setWorkspaceNextPage(null);
        setMode("workspace");
        await refreshDrafts();
        await refreshWorkspace();
        transmitter.current?.resume();
      }
      setPendingPair(false);
    } catch (cause) {
      setConnectionError(String(cause));
    } finally {
      connecting.current = false;
      setBusy(false);
    }
  }

  useEffect(() => {
    const operations = new Map<string, { scope: TransmitScope; documentId: string; generation: string }>();
    let operationsBinding: string | undefined;
    const scheduler = new AutoTransmit({
      queue: async (scope, generation) => {
        const queued = await queueWorkspaceDocument(scope.documentId, scope.editSessionId, generation);
        if (queued.committed) setTimeout(() => scheduler.kick(), 0);
        if (!queued.committed && operationsBinding === scope.binding) operations.set(queued.operation_id, { scope, documentId: queued.document_id, generation: queued.generation });
        if (controller.current?.isCurrentSession(scope.editSessionId)) { syncRevision.current++; setSyncStatus("동기화 대기"); }
        return { operationId: queued.operation_id, documentId: queued.document_id, generation: queued.generation, committed: queued.committed };
      },
      transmit: async () => {
        const version = registrationVersion.current;
        let result;
        try { result = await sendNextWorkspaceOperation(); }
        catch (cause) {
          if (/OutboxBlocked|ReconciliationRequired/.test(String(cause))) {
            setSyncStatus("서버 결과 대조 필요");
            return { status: "needs_review" as const };
          }
          throw cause;
        }
        const binding = JSON.stringify(result.binding);
        const activeBindings = [...sessionModes.current.values()].flatMap((session) => session.scope ? [session.scope.binding] : []);
        const stale = staleTransmissionResult(version, registrationVersion.current, binding, activeBindings);
        if (stale) return stale;
        const operation = result.operation_id ? operations.get(result.operation_id) : undefined;
        const matchingOperation = operation && operation.scope.binding === binding && result.document_id === operation.documentId && result.generation === operation.generation;
        if (matchingOperation) {
          if (result.status === "committed") scheduler.acknowledge(operation.scope, { operationId: result.operation_id!, documentId: operation.documentId, generation: operation.generation }, true);
          if (controller.current?.isCurrentSession(operation.scope.editSessionId)) {
            if (result.status === "conflict") setSyncStatus("충돌 있음");
            else if (result.status === "failed" || result.status === "needs_review") setSyncStatus("서버 결과 대조 필요");
            else if (result.status === "ack_pending" || result.status === "pending") setSyncStatus("서버 결과 확인 중");
          }
          if (result.status === "committed" || result.status === "conflict" || result.status === "failed" || result.status === "abandoned") operations.delete(result.operation_id!);
        }
        if (result.status !== "pending") void refreshSyncStatus().catch((cause) => setConnectionError(String(cause)));
        if (result.status === "ack_pending" || result.status === "auth_required" || result.status === "gateway_auth_required") return receiptWaitResult(result.status, result, binding, operation);
        if (result.status === "idle" || result.status === "pending" || result.status === "needs_review") return { status: result.status };
        if (!result.operation_id) throw new Error("잘못된 서버 저장 응답입니다.");
        return { status: result.status, operationId: result.operation_id };
      },
      acknowledged: (scope, generation, current) => {
        if (current && controller.current?.isCurrentSaved(scope.editSessionId, generation)) {
          syncRevision.current++;
          void refreshSyncStatus().catch((cause) => setConnectionError(String(cause)));
        }
      },
      receiptConfirmed: (operationId) => { operations.delete(operationId); },
      authRequired: (reason) => {
        setConnectionError(reason === "auth_required" ? "기기 인증이 만료되었거나 취소됐습니다. 다시 등록해 주세요. 기기 초안은 보존됩니다." : "gateway 인증을 확인해 주세요. 기기 초안은 보존됩니다.");
      },
      failed: (cause) => { if (!(cause instanceof QueueStorageError)) setConnectionError(String(cause)); },
      queueStorageFailed: (scope) => {
        if (controller.current?.isCurrentSession(scope.editSessionId)) reportQueueError(scope, "기기 초안은 저장됐지만 전송을 준비하지 못했습니다. 자동으로 다시 시도합니다.");
      },
      queueRecovered: (scope) => {
        setQueueErrors((errors) => clearQueueError(errors, scope));
      },
    });
    const instance = new EditorController({
      openDocument: async (id) => {
        const local = draftsRef.current.find((draft) => draftSelectionId(draft, modeRef.current) === id);
        const selectedMode = local ? "workspace" : modeRef.current;
        const node = selectedMode === "workspace" ? nodesRef.current.find((item) => item.node_id === id && item.kind === "file") : undefined;
        if (selectedMode === "workspace" && !node && !local) throw new Error("문서 목록을 다시 불러와 주세요.");
        const opened = local ? await openLocalDraft(local) : selectedMode === "fixture" ? await openDocument(id) : await openWorkspaceDocument(node!);
        const canSync = selectedMode === "workspace" && "binding" in opened && "readonly" in opened && !opened.readonly && (!local || (local.current_registration && local.has_session));
        const scope = canSync && "binding" in opened ? { binding: JSON.stringify(opened.binding), documentId: opened.document_id, editSessionId: opened.edit_session_id } : undefined;
        const binding = "binding" in opened ? opened.binding as WorkspaceRegistration : undefined;
        sessionModes.current.set(opened.edit_session_id, { mode: selectedMode, documentId: opened.document_id, uiId: id, scope, binding });
        setSyncStatus("");
        return { ...opened, document_id: id };
      },
      saveDocument: async (request) => {
        const session = sessionModes.current.get(request.edit_session_id);
        if (!session) throw new Error("편집 세션을 다시 열어 주세요.");
        const original = { ...request, document_id: session.documentId };
        const saved = session.mode === "fixture" ? await saveDocument(original) : await saveWorkspaceDocument(original);
        if (saved.edit_session_id !== request.edit_session_id || saved.generation !== request.generation) throw new Error("저장 응답이 현재 편집 요청과 일치하지 않습니다.");
        try {
          if (session.scope) scheduler.durable(session.scope, saved.generation);
          else scheduler.unscopedDurable(saved.edit_session_id, saved.generation);
        } catch (cause) { setConnectionError(String(cause)); }
        return saved;
      },
      closeDocument: async (request) => {
        const session = sessionModes.current.get(request.edit_session_id);
        if (!session) throw new Error("편집 세션을 다시 열어 주세요.");
        const original = { ...request, document_id: session.documentId };
        const close = () => session.mode === "fixture" ? closeDocument(original) : closeWorkspaceDocument(original);
        if (session.scope) {
          let queued = true;
          try { await scheduler.flushDocument(session.scope); }
          catch (cause) {
            if (/checkpoint_pending|composition_in_progress/.test(String(cause))) throw cause;
            queued = false;
            if (cause instanceof QueueStorageError) reportQueueError(session.scope, "기기 초안은 저장됐지만 전송을 준비하지 못했습니다. 초안을 다시 열어 전송해 주세요.");
            else setConnectionError(`기기 초안은 저장됐지만 전송을 준비하지 못했습니다: ${String(cause)}`);
          }
          if (queued) await scheduler.deactivate(session.scope, close);
          else await scheduler.deactivatePreservingDraft(session.scope, close);
        } else await close();
        sessionModes.current.delete(request.edit_session_id);
      },
      exit: async () => {
        for (const session of sessionModes.current.values()) if (session.scope) {
          try { await scheduler.flushDocument(session.scope); }
          catch (cause) {
            if (/checkpoint_pending|composition_in_progress/.test(String(cause))) throw cause;
            if (!(cause instanceof QueueStorageError)) setConnectionError(`기기 초안은 저장됐지만 전송을 준비하지 못했습니다: ${String(cause)}`);
          }
        }
        await invoke<void>("m0_exit");
      },
      stateChanged: (next) => {
        setState(next);
        if (selectedRef.current !== next.selected) {
          selectedRef.current = next.selected;
          setPreviewAnchor(null);
          setRecoveryMessage("");
          syncRevision.current++;
          if (next.selected) void refreshSyncStatus().catch((cause) => setConnectionError(String(cause)));
        }
        if (next.error.includes("pairing_in_progress")) setPendingPair(true);
      },
      sourceChanged: (text, documentChanged) => {
        source.current = text;
        clearTimeout(previewTimer.current);
        if (previewEnabled.current) {
          if (documentChanged) setPreviewSource(text);
          else previewTimer.current = setTimeout(() => setPreviewSource(source.current), 400);
        }
      },
      edited: (_id, sessionId, generation) => {
        const scope = sessionModes.current.get(sessionId)?.scope;
        try {
          if (scope) { scheduler.edit(scope, generation); syncRevision.current++; setSyncStatus("서버 반영 대기"); }
          else scheduler.unscopedEdit(sessionId, generation);
        } catch (cause) { setConnectionError(String(cause)); }
      },
      compositionChanged: (_id, sessionId, active) => {
        const scope = sessionModes.current.get(sessionId)?.scope;
        try {
          if (scope) scheduler.composition(scope, active);
          else scheduler.inputComposition(active);
        } catch (cause) { setConnectionError(String(cause)); }
      },
      opened: (_id, sessionId, generation, localDraft) => {
        const scope = sessionModes.current.get(sessionId)?.scope;
        if (scope) {
          if (operationsBinding !== scope.binding) {
            operations.clear();
            operationsBinding = scope.binding;
          }
          scheduler.activateBinding(scope.binding);
          scheduler.openSession(scope);
          if (localDraft) void scheduler.restore(scope, generation).catch((cause) => { if (!(cause instanceof QueueStorageError)) setConnectionError(String(cause)); });
        }
      },
      manualSync: async (id, sessionId) => {
        const session = sessionModes.current.get(sessionId);
        if (!session || session.uiId !== id) throw new Error("편집 세션을 다시 열어 주세요.");
        if (session.scope) {
          try { await scheduler.syncNow(session.scope); }
          catch (cause) { if (!(cause instanceof QueueStorageError)) throw cause; }
        }
      },
      createEditor: ({ source, change, inputRejected, composition, save }) => {
        const editing = new Compartment();
        let compositionTimer: ReturnType<typeof setTimeout> | undefined;
        let compositionPending = false;
        const view = new EditorView({
          state: EditorState.create({
            doc: source.text,
            extensions: [
              lineNumbers(), history(), drawSelection(),
              keymap.of([{ key: "Mod-s", run: () => { save(); return true; } }, ...defaultKeymap, ...historyKeymap]),
              editing.of([EditorState.readOnly.of(source.readonly), EditorView.editable.of(!source.readonly)]),
              ...(source.readonly ? [] : [documentSizeGuard(source.bom, inputRejected)]),
              EditorView.contentAttributes.of({ "aria-label": "Markdown 원문" }),
              EditorView.domEventHandlers({
                compositionstart: () => { clearTimeout(compositionTimer); compositionPending = true; composition(true); return false; },
                compositionend: () => { clearTimeout(compositionTimer); compositionTimer = setTimeout(() => { compositionPending = false; composition(false); }, 250); return false; },
              }),
              EditorView.updateListener.of((update) => {
                if (update.docChanged) change(update.state.doc.toString());
              }),
              ...(source.original.length > 1024 * 1024 ? [] : [markdown()]),
            ],
          }),
        });
        return {
          show: () => {
            if (!parent.current) throw new Error("편집기 화면이 닫혔습니다.");
            parent.current.replaceChildren(view.dom);
          },
          destroy: () => { clearTimeout(compositionTimer); view.destroy(); },
          setEditable: (enabled) => view.dispatch({ effects: editing.reconfigure([EditorState.readOnly.of(!enabled), EditorView.editable.of(enabled)]) }),
          isComposing: () => view.composing || compositionPending,
        };
      },
    });
    controller.current = instance;
    transmitter.current = scheduler;
    let disposed = false;
    void (async () => {
      try {
        await refreshDrafts();
        await refreshWorkspace();
        if (!disposed) {
          modeRef.current = "workspace";
          setMode("workspace");
          scheduler.resume();
        }
      } catch (cause) {
        if (!disposed && String(cause).includes("not_connected")) {
          await instance.open("welcome");
          if (draftsRef.current.length) setConnectionError("이전에 저장한 기기 초안을 열 수 있습니다.");
        }
        else if (!disposed && !String(cause).includes("stale_workspace_list")) {
          modeRef.current = "workspace";
          setMode("workspace");
          if (String(cause).includes("pairing_in_progress")) setPendingPair(true);
          setConnectionError(String(cause));
        }
      } finally {
        if (!disposed) setBusy(false);
      }
    })();
    const stop = listen("close-requested", () => { void instance.close(); });
    return () => {
      disposed = true;
      listLoader.current.invalidate();
      controller.current = null;
      transmitter.current = null;
      clearTimeout(previewTimer.current);
      void instance.dispose().then(() => scheduler.dispose()).catch(() => {});
      void stop.then((unlisten) => unlisten()).catch(() => {});
    };
  }, []);

  const loadPreviewImage = useCallback((target: string, signal: AbortSignal): Promise<Uint8Array> => {
    if (mode !== "workspace") return Promise.reject(new Error("작업 공간에 연결되어 있지 않습니다."));
    const found = [...sessionModes.current.entries()].find(([, session]) => session.uiId === state.selected && session.mode === "workspace");
    const resolved = found && resolveWorkspaceReference(nodesRef.current, found[1].documentId, target);
    if (!found || !resolved) return Promise.reject(new Error("이미지를 불러올 수 없습니다."));
    const [sessionId, session] = found;
    return imageQueue.current.load(() => {
      if (sessionModes.current.get(sessionId) !== session) throw new Error("편집 세션이 변경되었습니다.");
      return previewWorkspaceImage(session.documentId, sessionId, resolved.node);
    }, signal);
  }, [mode, state.selected, nodes]);

  useEffect(() => {
    let disposed = false;
    let timer: ReturnType<typeof setTimeout> | undefined;
    const check = async () => {
      try {
        const status = await pendingPairStatus();
        if (disposed) return;
        setPendingPair(status === "pending_adoption");
        if (status === "in_progress") timer = setTimeout(() => void check(), 1000);
      } catch (cause) {
        if (!disposed) setConnectionError(String(cause));
      }
    };
    void check();
    return () => { disposed = true; clearTimeout(timer); };
  }, []);

  const nodeIndex = useMemo(() => new Map(nodes.map((node) => [node.node_id, node])), [nodes]);
  const workspaceButtons = useMemo(() => nodes.filter((node) => node.kind === "file" && !node.deleted).map((node) => <button key={node.node_id} disabled={busy} data-document={node.node_id} aria-current={state.selected === node.node_id ? "page" : undefined} onClick={() => void controller.current?.open(node.node_id)}>{workspacePath(node, nodeIndex)}</button>), [nodes, nodeIndex, busy, state.selected]);
  const hasWorkspaceDocument = [...sessionModes.current.values()].some((session) => session.mode === "workspace" && session.uiId === state.selected);

  return <main>
    <header><strong>Notryx</strong><span className="badge">{mode === "fixture" ? "M0" : "M2"}</span><span className="caption">{mode === "fixture" ? "로컬 편집 시험" : "작업 공간"}</span></header>
    <div className="layout">
      <nav aria-label="문서 탐색">
        <form className="workspace-connect" onSubmit={(event) => void connect(event)}>
          <label>서버 주소<input name="origin" type="url" placeholder="https://example.org" required /></label>
          <label>작업 공간 ID<input name="workspace_id" required /></label>
          <label>등록 비밀<input name="pairing_secret" type="password" autoComplete="off" required /></label>
          <button type="submit" disabled={busy}>작업 공간 연결</button>
        </form>
        {pendingPair && <div><button disabled={busy} onClick={() => void resumePair(false)}>대기 중인 등록 이어가기</button><button disabled={busy} onClick={() => void resumePair(true)}>대기 중인 등록 버리기</button></div>}
        <p>{mode === "fixture" ? "합성 문서" : "작업 공간 문서"}</p>
        {mode === "fixture" ? documents.map(([id, label]) => <button key={id} disabled={busy} data-document={id} aria-current={state.selected === id ? "page" : undefined} onClick={() => void controller.current?.open(id)}>{label}</button>)
          : workspaceButtons}
        {mode === "workspace" && <button disabled={busy} onClick={() => { setConnectionError(""); void refreshWorkspace().catch((cause) => { if (!String(cause).includes("stale_workspace_list")) setConnectionError(String(cause)); }); }}>목록 새로고침</button>}
        {mode === "workspace" && (workspacePageStart !== null || workspaceNextPage !== null) && <p>목록을 나누어 표시합니다. 다른 구간에 있는 폴더나 첨부는 경로와 링크가 표시되지 않을 수 있습니다.</p>}
        {mode === "workspace" && workspacePageStart !== null && <button disabled={busy} onClick={() => { setConnectionError(""); void refreshWorkspace().catch((cause) => { if (!String(cause).includes("stale_workspace_list")) setConnectionError(String(cause)); }); }}>처음 목록</button>}
        {mode === "workspace" && workspaceNextPage !== null && <button disabled={busy} onClick={() => { setConnectionError(""); void refreshWorkspace(workspaceNextPage).catch((cause) => { if (!String(cause).includes("stale_workspace_list")) setConnectionError(String(cause)); }); }}>다음 목록</button>}
        <p>기기에 보존된 초안</p>
        {drafts.map((draft) => <button key={draftKey(draft)} disabled={busy} data-draft-document={draft.document_id} aria-current={state.selected === draftSelectionId(draft, mode) ? "page" : undefined} onClick={() => void controller.current?.open(draftSelectionId(draft, mode))}>{draft.binding.workspace_id.slice(0, 8)} / {draft.document_id}</button>)}
        <button disabled={busy} onClick={() => void refreshDrafts().catch((cause) => setConnectionError(String(cause)))}>초안 목록 새로고침</button>
        <small>{mode === "fixture" ? "연결 전에는 시험 문서가 표시됩니다." : "초안은 먼저 이 기기에 저장됩니다."}</small>
        <button id="isolation-open" onClick={() => { setIsolation(true); void invoke("m0_open_isolation").catch((cause) => setProbeError(String(cause))); }}>격리 시험 열기</button>
      </nav>
      <section className="document">
        <div className="toolbar"><span><span id="save-status" role="status">{state.status}</span>{syncStatus && <span id="sync-status">, {syncStatus}</span>}</span><div className="toolbar-actions"><button id="preview-toggle" aria-pressed={preview} onClick={() => {
          clearTimeout(previewTimer.current);
          previewEnabled.current = !previewEnabled.current;
          setPreview(previewEnabled.current);
          if (previewEnabled.current) setPreviewSource(source.current);
        }}>미리보기</button><button id="save" onClick={() => void controller.current?.save().catch(() => {})}>지금 저장</button>{hasWorkspaceDocument && <><button id="copy-source" disabled={recovering} onClick={() => void recoverSource("copy")}>원문 복사</button><button id="export-source" disabled={recovering} onClick={() => void recoverSource("export")}>파일 내보내기</button></>}</div></div>
        {state.notice && <p className="notice">{state.notice}</p>}
        {recoveryMessage && <p role="status">{recoveryMessage}</p>}
        <QueueAlerts error={state.error || probeError || connectionError} queueErrors={queueErrors} />
        <div className="document-content">
          <div id="editor" ref={parent} />
          {preview && <section className="preview-pane" aria-label="Markdown 미리보기">{new TextEncoder().encode(previewSource).length > 1024 * 1024
            ? <p>큰 문서는 미리보기를 표시하지 않습니다.</p>
            : <MarkdownPreview source={previewSource} fragment={previewAnchor?.documentId === state.selected ? previewAnchor : null} loadImage={mode === "workspace" ? loadPreviewImage : undefined} onOpenLink={mode === "workspace" ? (link) => {
              if (link.kind === "external") {
                const found = [...sessionModes.current.entries()].find(([, session]) => session.uiId === state.selected && session.mode === "workspace");
                if (!found) setConnectionError("문서를 다시 열어 주세요.");
                else void openWorkspaceExternal(found[1].documentId, found[0], link.target).catch((cause) => setConnectionError(String(cause)));
                return;
              }
              if (link.kind !== "document") return;
              const resolved = resolveWorkspaceReference(nodesRef.current, state.selected, link.target);
              if (!resolved) setConnectionError("연결된 문서를 찾지 못했습니다. 목록을 새로고침하거나 다른 구간을 확인해 주세요.");
              else void controller.current?.open(resolved.node.node_id).then(() => {
                if (resolved.fragment && selectedRef.current === resolved.node.node_id) {
                  setPreviewAnchor({ documentId: resolved.node.node_id, fragment: resolved.fragment });
                }
              });
            } : undefined} />}</section>}
        </div>
        {isolation && <iframe id="sandbox-probe" title="격리 시험" sandbox="allow-scripts" src="isolation.html" />}
      </section>
    </div>
  </main>;
}

createRoot(document.getElementById("root")!).render(<StartupGate ready={() => <App />} />);
