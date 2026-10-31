import { useEffect, useMemo, useRef, useState, type Dispatch, type RefObject, type SetStateAction } from "react";
import type { EditorController } from "./editor-controller";
import type { AutoTransmit, TransmitScope } from "./auto-transmit";
import type { ConflictReviewPorts, ConflictReviewSnapshot } from "./conflict-review";
import type { WorkspaceContentConflictMergeFollowup, WorkspaceRegistration } from "../../../packages/protocol-types";
import { prepareWorkspaceContentConflict, chooseWorkspaceRemoteContentConflict, confirmWorkspaceContentConflictCopy, retryWorkspaceContentConflictCopy, workspaceContentConflictCopyStatus, progressWorkspaceContentConflictCopy, continueWorkspaceRemoteContentConflict, completeWorkspacePendingRemoteContentConflict, workspaceContentConflictResolutionStatus, reopenWorkspaceContentConflictCheckpoint, confirmWorkspaceContentConflictMerge, queueWorkspaceContentConflictMerge, workspaceContentConflictMergeStatus, workspaceContentConflictMergeFollowupStatus, requestWorkspaceContentConflictMergeFollowupAbandon, workspaceDocumentOutbox } from "./ipc";

type ConflictIdentity = Pick<ConflictReviewSnapshot, "binding" | "node_id" | "operation_id" | "operation_seq" | "generation">;
type ConflictChoiceContext = { instance: EditorController; snapshot: ConflictIdentity; sessionId: string; uiId: string; version: number; merge?: boolean };
type PendingConflictContext = ConflictChoiceContext & { intentVersion: string; generation: string; revision: number };
type PendingMergeContext = ConflictChoiceContext & { intentVersion: string; generation: string; revision: number; queuedOperationId: string | null; queuedOperationSeq: string | null; followup?: WorkspaceContentConflictMergeFollowup | null; approval?: WorkspaceContentConflictMergeFollowup | null };
type CopyDetails = NonNullable<Awaited<ReturnType<typeof workspaceContentConflictCopyStatus>>["copy"]>;
type PendingCopyContext = ConflictChoiceContext & { intentVersion: string; copy: CopyDetails; approval: string | null; running: boolean; awaitingFollowup: string | null };

type EditorSession = { mode: "fixture" | "workspace"; documentId: string; uiId: string; scope?: TransmitScope; binding?: WorkspaceRegistration; orphanDraft?: boolean };
type ContentConflictOptions = {
  controller: RefObject<EditorController | null>;
  transmitter: RefObject<AutoTransmit | null>;
  registrationVersion: RefObject<number>;
  workspaceBindingRef: RefObject<WorkspaceRegistration | null>;
  sessionModes: RefObject<Map<string, EditorSession>>;
  syncRevision: RefObject<number>;
  copyRevision: number;
  setCopyRevision: Dispatch<SetStateAction<number>>;
  refreshSyncStatus(): Promise<void>;
  setConnectionError: Dispatch<SetStateAction<string>>;
};
type ConflictStatusObservation = {
  result: Awaited<ReturnType<typeof workspaceDocumentOutbox>>;
  binding: WorkspaceRegistration;
  session: EditorSession;
  sessionId: string;
  version: number;
  revision: number;
  actionVersion: number;
};

export function useContentConflict({ controller, transmitter, registrationVersion, workspaceBindingRef, sessionModes, syncRevision, copyRevision, setCopyRevision, refreshSyncStatus, setConnectionError }: ContentConflictOptions) {
  const [conflictPreparing, setConflictPreparing] = useState(false);
  const [conflictMessage, setConflictMessage] = useState("");
  const [conflictSnapshot, setConflictSnapshot] = useState<{ snapshot: ConflictReviewSnapshot; sessionId: string; revision: number; canChoose: boolean; intentVersion: string | null } | null>(null);
  const [conflictRecovery, setConflictRecovery] = useState<ConflictChoiceContext | null>(null);
  const [pendingConflict, setPendingConflict] = useState<PendingConflictContext | null>(null);
  const [pendingMerge, setPendingMerge] = useState<PendingMergeContext | null>(null);
  const pendingMergeRef = useRef<PendingMergeContext | null>(null);
  pendingMergeRef.current = pendingMerge;
  const [pendingCopy, setPendingCopy] = useState<PendingCopyContext | null>(null);
  const [copyRetryOpen, setCopyRetryOpen] = useState(false);
  const pendingCopyRef = useRef<PendingCopyContext | null>(null);
  pendingCopyRef.current = pendingCopy;
  const copyWakePending = useRef(false);
  const copyStatusRequest = useRef(0);
  const [conflictRecovering, setConflictRecovering] = useState(false);
  const conflictPreparingRef = useRef(false);
  const conflictActionRef = useRef(false);
  const conflictActionRevision = useRef(0);
  function startConflictAction() { conflictActionRef.current = true; conflictActionRevision.current++; }

  const conflictReviewPorts = useMemo<ConflictReviewPorts>(() => ({
    load: async (binding, documentId, sessionId, generation, operationId) => {
      const current = conflictSnapshot;
      if (!current || current.sessionId !== sessionId || current.snapshot.node_id !== documentId || current.snapshot.generation !== generation || current.snapshot.operation_id !== operationId || JSON.stringify(current.snapshot.binding) !== JSON.stringify(binding)) throw new Error("stale_conflict");
      return current.snapshot;
    },
  }), [conflictSnapshot]);

  async function inspectConflict() {
    if (conflictPreparingRef.current) return;
    conflictPreparingRef.current = true;
    setConflictPreparing(true); setConflictMessage(""); setConflictSnapshot(null);
    let origin: { instance: EditorController; sessionId: string; bindingKey: string; version: number } | undefined;
    try {
      const instance = controller.current;
      if (!instance) throw new Error("열린 문서를 다시 확인해 주세요.");
      const snapshot = instance.recoverySnapshot();
      const session = sessionModes.current.get(snapshot.session);
      if (!session || session.mode !== "workspace" || !session.scope || !session.binding || session.uiId !== snapshot.id) throw new Error("열린 문서를 다시 확인해 주세요.");
      const binding = session.binding;
      const key = JSON.stringify(binding), version = registrationVersion.current;
      origin = { instance, sessionId:snapshot.session, bindingKey:key, version };
      await instance.flush();
      if (controller.current !== instance || !instance.isCurrentSnapshot(snapshot.session, snapshot.generation, snapshot.revision) || version !== registrationVersion.current || !workspaceBindingRef.current || JSON.stringify(workspaceBindingRef.current) !== key) throw new Error("문서가 변경됐습니다. 다시 확인해 주세요.");
      const result = await prepareWorkspaceContentConflict(binding, session.documentId, snapshot.session, snapshot.generation);
      if (controller.current !== instance || !instance.isCurrentSnapshot(snapshot.session, snapshot.generation, snapshot.revision) || version !== registrationVersion.current || !workspaceBindingRef.current || JSON.stringify(workspaceBindingRef.current) !== key) throw new Error("문서가 변경됐습니다. 다시 확인해 주세요.");
      if (!result) { setConflictMessage("비교할 충돌을 찾지 못했습니다. 현재 편집과 초안은 유지됩니다."); return; }
      setConflictSnapshot({ snapshot:result, sessionId:snapshot.session, revision:snapshot.revision, canChoose:false, intentVersion:null });
      try {
        const resolution = await workspaceContentConflictResolutionStatus(binding, result.node_id, result.operation_id);
        if (controller.current !== instance || !instance.isCurrentSnapshot(snapshot.session, snapshot.generation, snapshot.revision) || version !== registrationVersion.current || !workspaceBindingRef.current || JSON.stringify(workspaceBindingRef.current) !== key) return;
        if (JSON.stringify(resolution.binding) !== key || resolution.document_id !== result.node_id || resolution.operation_id !== result.operation_id) throw new Error("stale_conflict");
        const state = resolution.state;
        if (state.status === "not_found") {
          setPendingMerge(null);
          setConflictSnapshot((current) => current?.snapshot === result ? { ...current, canChoose:true } : current);
        } else if (state.status === "pending" && !state.frozen && state.choice === "choose_remote") {
          setPendingMerge(null);
          if (state.needs_review) {
            setPendingConflict(null);
            setConflictSnapshot((current) => current?.snapshot === result ? { ...current, canChoose:true, intentVersion:state.version } : current);
            setConflictMessage("저장된 선택 이후 문서가 변경됐습니다. 다시 비교하고 선택을 확인해 주세요.");
          } else if (state.generation === snapshot.generation) {
            setPendingConflict({ instance, snapshot:result, sessionId:snapshot.session, uiId:snapshot.id, version, intentVersion:state.version, generation:snapshot.generation, revision:snapshot.revision });
            setConflictSnapshot((current) => current?.snapshot === result ? { ...current, canChoose:true, intentVersion:state.version } : current);
            setConflictMessage("저장된 선택이 보류 중입니다. 후속 처리를 진행하거나 서버 본문을 다시 확인해 선택할 수 있습니다.");
          } else throw new Error("stale_conflict");
        } else if (state.status === "pending" && !state.frozen && state.choice === "keep_local_copy") {
          setPendingConflict(null);
          setPendingMerge(null);
          await loadCopyStatus({ instance, snapshot: result, sessionId: snapshot.session, uiId: snapshot.id, version }, state.version);
          if (state.needs_review) setConflictMessage("저장된 초안이 바뀌었습니다. 사본과 원래 충돌을 다시 확인해 주세요.");
        } else if (state.status === "pending" && !state.frozen && state.choice === "save_merged") {
          setPendingConflict(null);
          setPendingMerge({ instance, snapshot:result, sessionId:snapshot.session, uiId:snapshot.id, version, merge:true, intentVersion:state.version, generation:snapshot.generation, revision:snapshot.revision, queuedOperationId:null, queuedOperationSeq:null });
          setConflictSnapshot((current) => current?.snapshot === result ? { ...current, canChoose:state.needs_review, intentVersion:state.version } : current);
          setConflictMessage(state.needs_review ? "초안 또는 서버 본문이 바뀌었습니다. 다시 비교하고 합본을 확인해 주세요." : "합본 선택이 저장됐습니다. 서버 저장은 아직 대기 중입니다.");
        } else {
          setPendingConflict(null);
          setConflictMessage("이전 선택 상태를 대조해 주세요. 현재 문서와 초안은 보존됩니다.");
        }
      } catch {
        if (controller.current === instance && instance.isCurrentSession(snapshot.session) && version === registrationVersion.current && workspaceBindingRef.current && JSON.stringify(workspaceBindingRef.current) === key) {
          setConflictMessage("저장된 선택 상태를 확인하지 못했습니다. 다시 비교해 주세요.");
        }
      }
    } catch (cause) {
      if (origin && (controller.current !== origin.instance || !origin.instance.isCurrentSession(origin.sessionId) || registrationVersion.current !== origin.version || !workspaceBindingRef.current || JSON.stringify(workspaceBindingRef.current) !== origin.bindingKey)) return;
      setConflictMessage(String(cause).includes("조합") ? "한글 조합을 마친 뒤 다시 확인해 주세요." : String(cause).includes("문서가 변경") ? "문서가 변경됐습니다. 다시 확인해 주세요." : "비교 자료를 불러오지 못했습니다. 현재 편집과 초안은 유지됩니다.");
    } finally {
      conflictPreparingRef.current = false;
      setConflictPreparing(false);
    }
  }

  function conflictContextCurrent(context: ConflictChoiceContext): boolean {
    return controller.current === context.instance && context.instance.isCurrentSession(context.sessionId) &&
      registrationVersion.current === context.version && !!workspaceBindingRef.current &&
      JSON.stringify(workspaceBindingRef.current) === JSON.stringify(context.snapshot.binding);
  }

  async function loadCopyStatus(context: ConflictChoiceContext, intentVersion: string, expected?: { nodeId: string; operationId: string; parentId: string; name: string }): Promise<CopyDetails> {
    const attempt = ++copyStatusRequest.current;
    const response = await workspaceContentConflictCopyStatus(context.snapshot.binding, context.snapshot.node_id, context.snapshot.operation_id);
    if (attempt !== copyStatusRequest.current) throw new Error("stale_copy_query");
    if (!conflictContextCurrent(context) || JSON.stringify(response.binding) !== JSON.stringify(context.snapshot.binding) || response.document_id !== context.snapshot.node_id || response.operation_id !== context.snapshot.operation_id || !response.copy || response.copy.version !== intentVersion || (expected && (response.copy.node_id !== expected.nodeId || response.copy.create_operation_id !== expected.operationId || response.copy.parent_id !== expected.parentId || response.copy.name !== expected.name))) throw new Error("stale_copy");
    setPendingCopy((previous) => ({ ...context, intentVersion, copy: response.copy!, approval: previous?.approval ? response.copy!.followup_operation_id : null, running: previous?.intentVersion === intentVersion && previous.snapshot.operation_id === context.snapshot.operation_id ? previous.running : false, awaitingFollowup: previous?.intentVersion === intentVersion && previous.snapshot.operation_id === context.snapshot.operation_id ? previous.awaitingFollowup : null }));
    if (response.copy.content_status === "saved") {
      setConflictSnapshot((current) => current?.snapshot === context.snapshot && current.sessionId === context.sessionId && context.instance.isCurrentSnapshot(context.sessionId, context.snapshot.generation, current.revision) ? { ...current, canChoose: true, intentVersion } : current);
    }
    return response.copy;
  }

  async function reopenConflict(context: ConflictChoiceContext, checkStatus: boolean): Promise<boolean> {
    let reopenStarted = false;
    let reopenedCanSync = false;
    try {
      if (!conflictContextCurrent(context)) throw new Error("작업 공간이 변경됐습니다.");
      if (checkStatus) {
        await workspaceContentConflictResolutionStatus(context.snapshot.binding, context.snapshot.node_id, context.snapshot.operation_id);
        if (!conflictContextCurrent(context)) throw new Error("작업 공간이 변경됐습니다.");
      }
      const state = await context.instance.reopenConflictCheckpoint(async (id, previousSessionId) => {
        if (id !== context.uiId || previousSessionId !== context.sessionId || !conflictContextCurrent(context)) throw new Error("문서가 변경됐습니다.");
        reopenStarted = true;
        const result = await reopenWorkspaceContentConflictCheckpoint(context.snapshot.binding, context.snapshot.node_id, context.snapshot.operation_id, previousSessionId);
        const document = result.document;
        if (JSON.stringify(document.binding) !== JSON.stringify(context.snapshot.binding) || document.document_id !== context.snapshot.node_id || !document.edit_session_id || document.edit_session_id === previousSessionId) throw new Error("다른 충돌 복구 결과입니다.");
        const scope = document.has_session && !document.readonly ? { binding: JSON.stringify(document.binding), documentId: document.document_id, editSessionId: document.edit_session_id } : undefined;
        reopenedCanSync = !!scope;
        sessionModes.current.set(document.edit_session_id, { mode: "workspace", documentId: document.document_id, uiId: id, scope, binding: document.binding });
        return { state: result.state, document: { ...document, document_id: id } };
      });
      setConflictRecovery(null);
      setConflictSnapshot(null);
      if (state.status === "pending" && state.choice === "choose_remote" && !state.needs_review && !state.frozen && reopenedCanSync) {
        const current = context.instance.recoverySnapshot();
        setPendingConflict({ ...context, sessionId: current.session, uiId: current.id, generation: current.generation, revision: current.revision, intentVersion: state.version });
        setPendingMerge(null);
      } else if (state.status === "pending" && state.choice === "save_merged" && !state.frozen && reopenedCanSync) {
        const current = context.instance.recoverySnapshot();
        setPendingConflict(null);
        setPendingCopy(null);
        setPendingMerge({ ...context, merge:true, sessionId:current.session, uiId:current.id, generation:current.generation, revision:current.revision, intentVersion:state.version, queuedOperationId:null, queuedOperationSeq:null });
      } else {
        setPendingConflict(null);
        setPendingMerge(null);
        if (state.status === "pending" && state.choice === "keep_local_copy" && !state.frozen && reopenedCanSync) {
          const current = context.instance.recoverySnapshot();
          await loadCopyStatus({ ...context, sessionId: current.session, uiId: current.id }, state.version);
        } else setPendingCopy(null);
      }
      setConflictMessage(state.status === "completed" ? (context.merge ? "충돌 선택 상태를 확인했습니다. 기기 초안을 확인해 주세요." : "서버 본문을 선택했습니다. 보존된 충돌 자료는 유지됩니다.") : state.status === "pending" ? (!reopenedCanSync ? "기기 인증 또는 작업 공간 연결을 확인해 주세요. 저장된 초안은 보존됩니다." : state.needs_review ? "문서가 변경됐습니다. 충돌을 다시 비교해 주세요." : state.choice === "save_merged" && !state.frozen ? "합본 선택이 저장됐습니다. 서버 저장은 아직 대기 중입니다." : state.frozen || state.choice !== "choose_remote" ? "이전 선택 상태를 대조해 주세요. 초안은 보존됩니다." : "선택을 저장했습니다. 충돌 처리가 보류 중입니다. 후속 처리를 직접 진행해 주세요.") : "선택이 기록되지 않았습니다. 보존된 초안을 다시 확인해 주세요.");
      void refreshSyncStatus().catch((cause) => setConnectionError(String(cause)));
      return true;
    } catch {
      if (reopenStarted) {
        setConflictRecovery(null);
        setConflictMessage("재열기 결과를 확인할 수 없습니다. 저장된 초안은 보존됩니다. 앱을 다시 시작해 주세요.");
      } else {
        if (context.merge) { setPendingMerge(null); setConflictRecovery(context); }
        setConflictMessage("충돌 처리 상태를 확인하지 못했습니다. 저장된 초안은 보존됩니다. 다시 확인해 주세요.");
      }
      return false;
    }
  }

  async function chooseRemote(snapshot: ConflictReviewSnapshot): Promise<void> {
    if (conflictActionRef.current) return;
    startConflictAction();
    let started = false;
    let context: ConflictChoiceContext | undefined;
    try {
      const shown = conflictSnapshot, instance = controller.current;
      if (!shown || shown.snapshot !== snapshot || !instance) throw new Error("문서가 변경됐습니다.");
      const current = instance.recoverySnapshot();
      const session = sessionModes.current.get(current.session);
      if (!session?.scope || session.uiId !== current.id || current.session !== shown.sessionId || current.generation !== snapshot.generation || current.revision !== shown.revision) throw new Error("문서가 변경됐습니다.");
      context = { instance, snapshot, sessionId: current.session, uiId: current.id, version: registrationVersion.current };
      if (!conflictContextCurrent(context)) throw new Error("작업 공간이 변경됐습니다.");
      const result = await instance.withConflictSnapshot(current, async () => {
        started = true;
        return chooseWorkspaceRemoteContentConflict(snapshot, current.session, shown.intentVersion);
      });
      setConflictSnapshot(null);
      setPendingMerge(null);
      if (result.status === "pending") {
        setPendingConflict({ ...context, intentVersion: result.version, generation: current.generation, revision: current.revision });
        setConflictMessage("선택을 저장했습니다. 충돌 처리가 보류 중입니다. 후속 처리를 직접 진행해 주세요.");
      }
      else await reopenConflict(context, false);
    } catch (cause) {
      if (started && context) {
        setConflictSnapshot(null);
        setConflictRecovery(context);
        setConflictMessage("선택 결과를 확인할 수 없습니다. 저장된 초안은 보존됩니다.");
      } else setConflictMessage(String(cause).includes("조합") ? "한글 조합을 마친 뒤 다시 선택해 주세요." : "문서가 변경됐거나 저장하지 못했습니다. 비교 자료를 다시 확인해 주세요.");
    } finally {
      conflictActionRef.current = false;
    }
  }

  async function saveMerged(snapshot: ConflictReviewSnapshot): Promise<void> {
    if (conflictActionRef.current) return;
    startConflictAction();
    let started = false;
    let context: ConflictChoiceContext | undefined;
    try {
      const shown = conflictSnapshot, instance = controller.current;
      if (!shown || shown.snapshot !== snapshot || !instance) throw new Error("문서가 변경됐습니다.");
      const current = instance.recoverySnapshot();
      const session = sessionModes.current.get(current.session);
      if (!session?.scope || session.uiId !== current.id || current.session !== shown.sessionId || current.generation !== snapshot.generation || current.revision !== shown.revision) throw new Error("문서가 변경됐습니다.");
      context = { instance, snapshot, sessionId:current.session, uiId:current.id, version:registrationVersion.current, merge:true };
      if (!conflictContextCurrent(context)) throw new Error("작업 공간이 변경됐습니다.");
      const result = await instance.withConflictSnapshot(current, async () => {
        started = true;
        return confirmWorkspaceContentConflictMerge(snapshot, current.session, shown.intentVersion);
      });
      setConflictSnapshot(null);
      setPendingConflict(null);
      setPendingMerge(null);
      setPendingCopy(null);
      if (result.status === "pending") {
        setPendingMerge({ ...context, intentVersion:result.version, generation:current.generation, revision:current.revision, queuedOperationId:null, queuedOperationSeq:null });
        setConflictMessage("합본 선택을 기기에 저장했습니다. 서버 저장은 아직 대기 중입니다.");
      } else await reopenConflict(context, false);
    } catch (cause) {
      if (started && context) {
        setConflictSnapshot(null);
        setPendingMerge(null);
        setConflictRecovery(context);
        setConflictMessage("합본 선택 결과를 확인할 수 없습니다. 저장된 초안은 보존됩니다.");
      } else setConflictMessage(String(cause).includes("조합") ? "한글 조합을 마친 뒤 다시 선택해 주세요." : "문서가 변경됐거나 저장하지 못했습니다. 비교 자료를 다시 확인해 주세요.");
    } finally { conflictActionRef.current = false; }
  }

  async function refreshMerge(): Promise<void> {
    const pending = pendingMerge;
    if (!pending || conflictActionRef.current || !conflictContextCurrent(pending)) return;
    startConflictAction(); setConflictRecovering(true);
    try {
      const response = await workspaceContentConflictResolutionStatus(pending.snapshot.binding, pending.snapshot.node_id, pending.snapshot.operation_id);
      if (!conflictContextCurrent(pending) || JSON.stringify(response.binding) !== JSON.stringify(pending.snapshot.binding) || response.document_id !== pending.snapshot.node_id || response.operation_id !== pending.snapshot.operation_id) throw new Error("stale_conflict");
      const result = response.state;
      if (result.status === "completed" && !result.frozen && BigInt(result.generation) === BigInt(pending.snapshot.generation) + 1n) {
        if (!await hasConfirmedMergeIdentity(pending) || !conflictContextCurrent(pending) || pendingMergeRef.current !== pending) throw new Error("stale_merge_operation");
        setPendingMerge(null);
        setConflictMessage("합본 저장 완료. 이후 기기 초안과 충돌 자료는 보존됩니다.");
        void refreshSyncStatus().catch((cause) => setConnectionError(String(cause)));
      } else if (result.status === "pending" && result.choice === "save_merged" && result.version === pending.intentVersion && result.generation === pending.snapshot.generation && !result.frozen) {
        setConflictMessage(result.needs_review ? "초안 또는 서버 본문이 바뀌었습니다. 충돌을 다시 비교해 주세요." : "합본 선택이 저장됐습니다. 서버 결과와 ACK를 확인하기 전까지 대기 중입니다.");
      } else throw new Error("stale_conflict");
    } catch {
      if (conflictContextCurrent(pending)) setConflictMessage("합본 상태를 확인하지 못했습니다. 저장된 초안은 보존됩니다.");
    } finally { conflictActionRef.current = false; setConflictRecovering(false); }
  }

  async function hasConfirmedMergeIdentity(pending: PendingMergeContext): Promise<boolean> {
    const status = await workspaceContentConflictMergeStatus(pending.snapshot.binding, pending.snapshot.node_id, pending.snapshot.operation_id, pending.intentVersion);
    const operation = status.merge_operation;
    return JSON.stringify(status.binding) === JSON.stringify(pending.snapshot.binding) && status.document_id === pending.snapshot.node_id && status.operation_id === pending.snapshot.operation_id && status.intent_version === pending.intentVersion && !!operation && (!pending.queuedOperationId || operation.operation_id === pending.queuedOperationId) && (!pending.queuedOperationSeq || operation.operation_seq === pending.queuedOperationSeq);
  }

  async function queueMerge(): Promise<void> {
    const pending = pendingMerge;
    if (!pending || conflictActionRef.current || !conflictContextCurrent(pending)) return;
    startConflictAction(); setConflictRecovering(true);
    let started = false;
    try {
      const resolution = await workspaceContentConflictResolutionStatus(pending.snapshot.binding, pending.snapshot.node_id, pending.snapshot.operation_id);
      if (!conflictContextCurrent(pending) || JSON.stringify(resolution.binding) !== JSON.stringify(pending.snapshot.binding) || resolution.document_id !== pending.snapshot.node_id || resolution.operation_id !== pending.snapshot.operation_id) throw new Error("stale_conflict");
      if (resolution.state.status === "completed" && !resolution.state.frozen && BigInt(resolution.state.generation) === BigInt(pending.snapshot.generation) + 1n) {
        if (!await hasConfirmedMergeIdentity(pending) || !conflictContextCurrent(pending) || pendingMergeRef.current !== pending) throw new Error("stale_merge_operation");
        setPendingMerge(null);
        setConflictMessage("합본 저장 완료. 이후 기기 초안과 충돌 자료는 보존됩니다.");
        return;
      }
      if (resolution.state.status !== "pending" || resolution.state.choice !== "save_merged" || resolution.state.version !== pending.intentVersion || resolution.state.generation !== pending.snapshot.generation || resolution.state.frozen || resolution.state.needs_review) throw new Error("stale_conflict");
      const status = await workspaceContentConflictMergeStatus(pending.snapshot.binding, pending.snapshot.node_id, pending.snapshot.operation_id, pending.intentVersion);
      if (!conflictContextCurrent(pending)) throw new Error("stale_conflict");
      if (status.merge_operation) {
        setPendingMerge((current) => current === pending ? { ...current, queuedOperationId:status.merge_operation!.operation_id, queuedOperationSeq:status.merge_operation!.operation_seq } : current);
        transmitter.current?.kick();
        setConflictMessage("이미 준비된 합본 작업을 확인했습니다. 서버 결과와 ACK를 기다립니다.");
        return;
      }
      const outbox = await workspaceDocumentOutbox(pending.snapshot.binding, pending.snapshot.node_id);
      if (!conflictContextCurrent(pending) || JSON.stringify(outbox.binding) !== JSON.stringify(pending.snapshot.binding) || outbox.document_id !== pending.snapshot.node_id) throw new Error("stale_conflict");
      const latest = outbox.latest;
      const followup = await workspaceContentConflictMergeFollowupStatus(pending.snapshot.binding, pending.snapshot.node_id, pending.snapshot.operation_id, pending.intentVersion);
      if (!conflictContextCurrent(pending)) throw new Error("stale_conflict");
      if (followup.followup) {
        const waiting = latest?.operation_id === followup.followup.operation_id && (latest.status === "abandoning" || latest.status === "abandoned" || latest.abandon_state !== "none");
        setPendingMerge((current) => current === pending ? { ...current, followup:followup.followup, approval:waiting ? null : followup.followup } : current);
        setConflictMessage(waiting ? "후속 작업의 서버 영수증과 ACK를 기다립니다." : "합본 저장을 위해 후속 작업의 취소 대상을 확인해 주세요.");
        return;
      }
      if (latest && latest.operation_id !== pending.snapshot.operation_id && (latest.status !== "abandoned" || BigInt(outbox.confirmed_through) < BigInt(latest.operation_seq))) {
        setConflictMessage("선행 후속 작업을 먼저 확인해야 합니다. 기기 초안과 합본 선택은 보존됩니다.");
        return;
      }
      if (BigInt(outbox.confirmed_through) < BigInt(pending.snapshot.operation_seq)) {
        setConflictMessage("이전 충돌 결과의 서버 ACK를 확인한 뒤 다시 진행해 주세요.");
        return;
      }
      const current = pending.instance.recoverySnapshot();
      if (current.id !== pending.uiId || current.session !== pending.sessionId || current.generation !== pending.generation || current.revision !== pending.revision) throw new Error("stale_checkpoint");
      const result = await pending.instance.withMergeQueueSnapshot(current, async () => {
        started = true;
        return queueWorkspaceContentConflictMerge(pending.snapshot.binding, pending.snapshot.node_id, pending.sessionId, pending.snapshot.operation_id, pending.intentVersion);
      });
      const operationId = result.requested_operation_id;
      if (!operationId || operationId === pending.snapshot.operation_id || result.version !== pending.intentVersion) throw new Error("stale_merge_operation");
      if (!await reopenConflict(pending, false)) return;
      const confirmed = await workspaceContentConflictMergeStatus(pending.snapshot.binding, pending.snapshot.node_id, pending.snapshot.operation_id, pending.intentVersion);
      if (confirmed.merge_operation?.operation_id !== operationId || registrationVersion.current !== pending.version || !workspaceBindingRef.current || JSON.stringify(workspaceBindingRef.current) !== JSON.stringify(pending.snapshot.binding)) throw new Error("stale_merge_operation");
      setPendingMerge((current) => current?.snapshot.operation_id === pending.snapshot.operation_id ? { ...current, queuedOperationId:operationId, queuedOperationSeq:confirmed.merge_operation!.operation_seq } : current);
      transmitter.current?.kick();
      setConflictMessage("합본 전송을 준비했습니다. 서버 결과와 ACK를 확인합니다.");
    } catch {
      if (started && conflictContextCurrent(pending)) {
        setPendingMerge(null);
        setConflictRecovery(pending);
        setConflictMessage("합본 전송 준비 결과를 확인하지 못했습니다. 저장된 초안은 보존됩니다.");
      } else if (controller.current === pending.instance) setConflictMessage("합본 처리 상태를 확인하지 못했습니다. 저장된 초안은 보존됩니다.");
    } finally { conflictActionRef.current = false; setConflictRecovering(false); }
  }

  function sameMergeFollowup(a: WorkspaceContentConflictMergeFollowup | null | undefined, b: WorkspaceContentConflictMergeFollowup | null | undefined): boolean {
    return !!a && !!b && a.operation_id === b.operation_id && a.operation_seq === b.operation_seq && a.generation === b.generation && a.payload_hash === b.payload_hash;
  }

  async function refreshMergeFollowup(): Promise<void> {
    const pending = pendingMergeRef.current;
    if (!pending || conflictActionRef.current || !conflictContextCurrent(pending)) return;
    startConflictAction(); setConflictRecovering(true);
    try {
      const response = await workspaceContentConflictMergeFollowupStatus(pending.snapshot.binding, pending.snapshot.node_id, pending.snapshot.operation_id, pending.intentVersion);
      if (!conflictContextCurrent(pending) || pendingMergeRef.current !== pending) throw new Error("stale_conflict");
      setPendingMerge((current) => current === pending ? { ...current, followup:response.followup, approval:sameMergeFollowup(current.approval, response.followup) ? current.approval : null } : current);
      setConflictMessage(response.followup ? "후속 작업의 현재 내용을 확인했습니다. 내용이 바뀌었다면 다시 승인해 주세요." : "현재 확인할 후속 작업이 없습니다. 서버 결과와 ACK를 확인해 주세요.");
    } catch {
      if (conflictContextCurrent(pending)) setConflictMessage("후속 작업을 확인하지 못했습니다. 저장된 초안은 보존됩니다.");
    } finally { conflictActionRef.current = false; setConflictRecovering(false); }
  }

  async function approveMergeFollowup(): Promise<void> {
    const pending = pendingMergeRef.current, approval = pending?.approval;
    if (!pending || !approval || conflictActionRef.current || !conflictContextCurrent(pending) || !sameMergeFollowup(pending.followup, approval)) return;
    startConflictAction(); setConflictRecovering(true);
    let started = false;
    try {
      const editor = pending.instance.recoverySnapshot();
      if (editor.id !== pending.uiId || editor.session !== pending.sessionId || editor.generation !== pending.snapshot.generation || editor.revision !== pending.revision) throw new Error("stale_checkpoint");
      const result = await pending.instance.withDeletedFollowupSnapshot(editor, () => {
        if (!conflictContextCurrent(pending) || pendingMergeRef.current !== pending || !sameMergeFollowup(pendingMergeRef.current?.approval, approval)) throw new Error("stale_conflict");
      }, async () => {
        started = true;
        return requestWorkspaceContentConflictMergeFollowupAbandon(pending.snapshot.binding, pending.snapshot.node_id, pending.sessionId, pending.snapshot.operation_id, pending.intentVersion, approval);
      });
      if (!conflictContextCurrent(pending) || result.version !== pending.intentVersion || result.requested_operation_id !== approval.operation_id) throw new Error("stale_conflict");
      setPendingMerge((current) => current === pending ? { ...current, followup:null, approval:null } : current);
      const scheduler = transmitter.current;
      if (scheduler && !scheduler.resumeContentAfterAbandon(JSON.stringify(pending.snapshot.binding), approval.operation_id)) scheduler.kick();
      setConflictMessage("승인한 후속 작업의 처리를 요청했습니다. 서버 영수증과 ACK를 확인해 주세요.");
    } catch (cause) {
      if (started && conflictContextCurrent(pending)) {
        setPendingMerge(null); setConflictRecovery(pending);
        setConflictMessage("후속 작업 처리 결과를 확인하지 못했습니다. 기기 초안은 보존됩니다.");
      } else if (controller.current === pending.instance) {
        setPendingMerge((current) => current === pending ? { ...current, approval:null } : current);
        setConflictMessage(String(cause).includes("조합") ? "한글 조합을 마친 뒤 다시 확인해 주세요." : "기기 초안을 저장한 뒤 충돌과 후속 작업을 다시 확인해 주세요.");
      }
    } finally { conflictActionRef.current = false; setConflictRecovering(false); }
  }

  async function keepConflictCopy(snapshot: ConflictReviewSnapshot, parentId: string, name: string): Promise<void> {
    if (conflictActionRef.current) return;
    startConflictAction();
    let started = false;
    let context: ConflictChoiceContext | undefined;
    try {
      const shown = conflictSnapshot, instance = controller.current;
      if (!shown || shown.snapshot !== snapshot || !shown.canChoose || !instance) throw new Error("문서가 변경됐습니다.");
      const current = instance.recoverySnapshot();
      const session = sessionModes.current.get(current.session);
      if (!session?.scope || session.uiId !== current.id || current.session !== shown.sessionId || current.generation !== snapshot.generation || current.revision !== shown.revision) throw new Error("문서가 변경됐습니다.");
      context = { instance, snapshot, sessionId: current.session, uiId: current.id, version: registrationVersion.current };
      if (!conflictContextCurrent(context)) throw new Error("작업 공간이 변경됐습니다.");
      const result = await instance.withConflictSnapshot(current, async () => {
        started = true;
        return confirmWorkspaceContentConflictCopy(snapshot, current.session, shown.intentVersion, parentId, name);
      });
      setConflictSnapshot(null);
      setPendingConflict(null);
      setPendingMerge(null);
      setConflictMessage("사본 보존 선택을 저장했습니다. 사본 저장은 보류 중이며 원래 문서의 충돌도 남아 있습니다.");
      try { await loadCopyStatus(context, result.version, { nodeId: result.node_id, operationId: result.create_operation_id, parentId, name }); }
      catch { if (conflictContextCurrent(context)) setConflictMessage("사본 선택을 저장했지만 상태를 확인하지 못했습니다. 사본 저장은 보류 중입니다. 충돌을 다시 비교해 주세요."); }
    } catch (cause) {
      if (started && context) {
        setConflictSnapshot(null);
        setConflictRecovery(context);
        setConflictMessage("사본 선택 결과를 확인할 수 없습니다. 저장된 초안은 보존됩니다.");
      } else setConflictMessage(String(cause).includes("조합") ? "한글 조합을 마친 뒤 다시 선택해 주세요." : "문서가 변경됐거나 선택을 저장하지 못했습니다. 비교 자료를 다시 확인해 주세요.");
    } finally {
      conflictActionRef.current = false;
    }
  }

  async function refreshCopy(): Promise<void> {
    const context = pendingCopyRef.current;
    if (!context || conflictActionRef.current) return;
    startConflictAction(); setConflictRecovering(true);
    try {
      await loadCopyStatus(context, context.intentVersion);
      if (conflictContextCurrent(context)) setConflictMessage("사본 상태를 확인했습니다. 원래 문서의 충돌은 유지됩니다.");
    } catch {
      if (conflictContextCurrent(context)) setConflictMessage("사본 상태를 확인하지 못했습니다. 저장된 초안은 보존됩니다.");
    } finally { conflictActionRef.current = false; setConflictRecovering(false); }
  }

  async function retryCopy(parentId: string, name: string): Promise<void> {
    const context = pendingCopyRef.current;
    if (!context || context.copy.create_failure !== "destination_exists" || conflictActionRef.current || !conflictContextCurrent(context)) return;
    startConflictAction(); setConflictRecovering(true);
    let started = false;
    try {
      if (parentId === context.copy.parent_id && name === context.copy.name) throw new Error("same_copy_destination");
      const current = context.instance.recoverySnapshot();
      if (current.id !== context.uiId || current.session !== context.sessionId || current.generation !== context.snapshot.generation) throw new Error("stale_conflict");
      const result = await context.instance.withConflictSnapshot(current, async () => {
        started = true;
        const response = await retryWorkspaceContentConflictCopy(context.snapshot.binding, context.snapshot.node_id, context.sessionId, context.snapshot.operation_id, context.intentVersion, context.copy.create_operation_id, parentId, name);
        if (response.version !== context.intentVersion || response.node_id === context.copy.node_id || response.create_operation_id === context.copy.create_operation_id) throw new Error("stale_copy");
        return response;
      });
      if (!conflictContextCurrent(context)) return;
      setCopyRetryOpen(false);
      setConflictMessage("새 사본 대상을 저장했습니다. 사본 저장은 대기 중이며 원래 충돌은 유지됩니다.");
      try { await loadCopyStatus(context, result.version, { nodeId: result.node_id, operationId: result.create_operation_id, parentId, name }); }
      catch { if (conflictContextCurrent(context)) setConflictMessage("새 대상을 저장했지만 사본 상태를 확인하지 못했습니다. 상태를 다시 확인해 주세요."); }
    } catch (cause) {
      if (!conflictContextCurrent(context)) return;
      if (started) {
        setCopyRetryOpen(false);
        setConflictRecovery(context);
        setConflictMessage("사본 재승인 결과를 확인하지 못했습니다. 저장된 초안은 보존됩니다. 상태를 다시 확인해 주세요.");
      } else setConflictMessage(String(cause).includes("same_copy_destination") ? "이전과 다른 폴더나 이름을 선택해 주세요." : String(cause).includes("조합") ? "한글 조합을 마친 뒤 다시 확인해 주세요." : "문서가 변경됐습니다. 충돌을 다시 비교해 주세요.");
    } finally { conflictActionRef.current = false; setConflictRecovering(false); }
  }

  async function progressCopy(context: PendingCopyContext, approval: string | null): Promise<void> {
    if (conflictActionRef.current || !conflictContextCurrent(context)) return;
    startConflictAction(); setConflictRecovering(true);
    try {
      const result = await progressWorkspaceContentConflictCopy(context.snapshot.binding, context.snapshot.node_id, context.sessionId, context.snapshot.operation_id, context.intentVersion, approval);
      if (!conflictContextCurrent(context)) return;
      if (result.status === "approval_required") {
        setPendingCopy((current) => current?.snapshot.operation_id === context.snapshot.operation_id ? { ...current, running: false, awaitingFollowup: null, approval: result.followup_operation_id, copy: { ...current.copy, followup_operation_id: result.followup_operation_id } } : current);
        setConflictMessage("사본 저장을 위해 후속 작업의 처리를 확인해 주세요. 원래 충돌은 유지됩니다.");
      } else if (result.status === "needs_review") {
        setPendingCopy((current) => current?.snapshot.operation_id === context.snapshot.operation_id ? { ...current, running: false, approval: null } : current);
        setConflictMessage("사본 저장 상태를 다시 확인해 주세요. 초안과 원래 충돌은 유지됩니다.");
      } else if (result.status === "saved") {
        await loadCopyStatus(context, context.intentVersion);
        setPendingCopy((current) => current?.snapshot.operation_id === context.snapshot.operation_id ? { ...current, running: false, approval: null } : current);
      } else {
        const requested = result.requested_operation_id;
        if (requested && requested !== approval) throw new Error("stale_approval");
        setPendingCopy((current) => current?.snapshot.operation_id === context.snapshot.operation_id ? { ...current, running: true, awaitingFollowup: requested, approval: null } : current);
        const scheduler = transmitter.current;
        if (requested) {
          if (scheduler && !scheduler.resumeContentAfterAbandon(JSON.stringify(context.snapshot.binding), requested)) scheduler.kick();
        } else scheduler?.kick();
        setConflictMessage("사본 저장을 진행 중입니다. 서버 결과와 ACK를 확인합니다. 원래 충돌은 유지됩니다.");
      }
    } catch {
      if (conflictContextCurrent(context)) {
        setPendingCopy((current) => current?.snapshot.operation_id === context.snapshot.operation_id ? { ...current, running: false, approval: null } : current);
        setConflictMessage("사본 처리 결과를 확인하지 못했습니다. 저장된 초안은 보존됩니다. 상태를 다시 확인해 주세요.");
      }
    } finally { conflictActionRef.current = false; setConflictRecovering(false); if (copyWakePending.current) { copyWakePending.current = false; setCopyRevision((revision) => revision + 1); } }
  }

  async function observeCopy(): Promise<void> {
    const context = pendingCopyRef.current;
    if (!context || !context.running || !conflictContextCurrent(context)) return;
    if (conflictActionRef.current) { copyWakePending.current = true; return; }
    try {
      const copy = await loadCopyStatus(context, context.intentVersion);
      if (!conflictContextCurrent(context)) return;
      if (copy.create_status === "needs_review" || copy.content_status === "needs_review" || copy.content_status === "saved") {
        setPendingCopy((current) => current?.snapshot.operation_id === context.snapshot.operation_id ? { ...current, running: false } : current);
      } else if ((context.awaitingFollowup && copy.followup_operation_id !== context.awaitingFollowup) || (copy.create_status === "created" && copy.content_status === "not_queued")) {
        await progressCopy(context, null);
      }
    } catch (cause) {
      if (String(cause).includes("stale_copy_query")) return;
      if (conflictContextCurrent(context)) {
        setPendingCopy((current) => current?.snapshot.operation_id === context.snapshot.operation_id ? { ...current, running: false } : current);
        setConflictMessage("사본 상태를 확인하지 못했습니다. 저장된 초안은 보존됩니다.");
      }
    }
  }

  useEffect(() => { if (copyRevision > 0) void observeCopy(); }, [copyRevision]);

  async function recoverConflictResult(): Promise<void> {
    if (!conflictRecovery || conflictActionRef.current) return;
    startConflictAction(); setConflictRecovering(true);
    try { await reopenConflict(conflictRecovery, true); }
    finally { conflictActionRef.current = false; setConflictRecovering(false); }
  }

  async function continueConflict(): Promise<void> {
    const pending = pendingConflict;
    if (!pending || conflictActionRef.current) return;
    startConflictAction(); setConflictRecovering(true);
    let started = false;
    try {
      if (!conflictContextCurrent(pending)) throw new Error("작업 공간이 변경됐습니다.");
      const current = pending.instance.recoverySnapshot();
      if (current.id !== pending.uiId || current.session !== pending.sessionId || current.generation !== pending.generation || current.revision !== pending.revision) throw new Error("문서가 변경됐습니다.");
      let requested: string | null = null;
      await pending.instance.withConflictSnapshot(current, async () => {
        started = true;
        const result = await continueWorkspaceRemoteContentConflict(pending.snapshot.binding, pending.snapshot.node_id, pending.sessionId, pending.snapshot.operation_id, pending.intentVersion);
        requested = result.requested_operation_id;
        return result;
      });
      if (!conflictContextCurrent(pending)) throw new Error("작업 공간이 변경됐습니다.");
      if (requested) {
        const scheduler = transmitter.current;
        if (scheduler && !scheduler.resumeContentAfterAbandon(JSON.stringify(pending.snapshot.binding), requested)) scheduler.kick();
      }
      setConflictMessage(requested ? "후속 처리를 요청했습니다. 서버 영수증 확인과 선택 완료가 남아 있습니다." : "추가로 요청할 후속 작업이 없습니다. 서버 영수증을 확인한 뒤 완료를 선택해 주세요.");
    } catch (cause) {
      if (started) {
        setPendingConflict(null);
        setConflictRecovery(pending);
        setConflictMessage("후속 처리 결과를 확인할 수 없습니다. 저장된 초안은 보존됩니다.");
      } else {
        setPendingConflict(null);
        setConflictMessage(String(cause).includes("조합") ? "한글 조합을 마친 뒤 다시 확인해 주세요." : "문서가 변경됐습니다. 충돌을 다시 비교해 주세요.");
      }
    } finally {
      conflictActionRef.current = false; setConflictRecovering(false);
    }
  }

  async function completeConflict(): Promise<void> {
    const pending = pendingConflict;
    if (!pending || conflictActionRef.current) return;
    startConflictAction(); setConflictRecovering(true);
    let started = false;
    try {
      if (!conflictContextCurrent(pending)) throw new Error("작업 공간이 변경됐습니다.");
      const current = pending.instance.recoverySnapshot();
      if (current.id !== pending.uiId || current.session !== pending.sessionId || current.generation !== pending.generation || current.revision !== pending.revision) throw new Error("문서가 변경됐습니다.");
      await pending.instance.withConflictSnapshot(current, async () => {
        started = true;
        return completeWorkspacePendingRemoteContentConflict(pending.snapshot.binding, pending.snapshot.node_id, pending.sessionId, pending.snapshot.operation_id, pending.intentVersion);
      });
      setPendingConflict(null);
      await reopenConflict(pending, false);
    } catch (cause) {
      if (started) {
        setPendingConflict(null);
        setConflictRecovery(pending);
        setConflictMessage("서버 영수증 또는 선택 결과를 확인하지 못했습니다. 저장된 초안은 보존됩니다.");
      } else {
        setPendingConflict(null);
        setConflictMessage(String(cause).includes("조합") ? "한글 조합을 마친 뒤 다시 확인해 주세요." : "문서가 변경됐습니다. 충돌을 다시 비교해 주세요.");
      }
    } finally {
      conflictActionRef.current = false; setConflictRecovering(false);
    }
  }

  async function observeConflictStatus({ result, binding, session, sessionId, version, revision, actionVersion }: ConflictStatusObservation): Promise<void> {
    const bindingKey = JSON.stringify(binding);
    const first = result.first_content_conflict;
    const instance = controller.current;
    if (!instance || conflictActionRef.current) return;
    const stillCurrent = () => revision === syncRevision.current && actionVersion === conflictActionRevision.current && version === registrationVersion.current && controller.current === instance && instance.isCurrentSession(sessionId) && !!workspaceBindingRef.current && JSON.stringify(workspaceBindingRef.current) === bindingKey && !conflictActionRef.current;
    if (!first) {
      const tracked = pendingMergeRef.current;
      if (!tracked || tracked.instance !== instance || tracked.sessionId !== sessionId || JSON.stringify(tracked.snapshot.binding) !== bindingKey || tracked.snapshot.node_id !== session.documentId) return;
      const response = await workspaceContentConflictResolutionStatus(binding, session.documentId, tracked.snapshot.operation_id);
      if (!stillCurrent() || pendingMergeRef.current !== tracked) return;
      if (JSON.stringify(response.binding) !== bindingKey || response.document_id !== session.documentId || response.operation_id !== tracked.snapshot.operation_id) throw new Error("stale_conflict");
      if (response.state.status === "completed" && !response.state.frozen && BigInt(response.state.generation) === BigInt(tracked.snapshot.generation) + 1n) {
        if (!await hasConfirmedMergeIdentity(tracked) || !stillCurrent() || pendingMergeRef.current !== tracked) return;
        setPendingMerge((current) => current === tracked ? null : current);
        setConflictMessage("합본 저장 완료. 이후 기기 초안과 충돌 자료는 보존됩니다.");
      }
      return;
    }
    const resolution = await workspaceContentConflictResolutionStatus(binding, session.documentId, first.operation_id);
    if (!stillCurrent()) return;
    if (JSON.stringify(resolution.binding) !== bindingKey || resolution.document_id !== session.documentId || resolution.operation_id !== first.operation_id) throw new Error("stale_conflict");
    const selected = resolution.state;
    if (selected.status !== "pending" || selected.choice !== "save_merged" || selected.frozen) return;
    const merge = await workspaceContentConflictMergeStatus(binding, session.documentId, first.operation_id, selected.version);
    if (!stillCurrent()) return;
    if (JSON.stringify(merge.binding) !== bindingKey || merge.document_id !== session.documentId || merge.operation_id !== first.operation_id || merge.intent_version !== selected.version) throw new Error("stale_merge_operation");
    let editor: ReturnType<EditorController["recoverySnapshot"]>;
    try { editor = instance.recoverySnapshot(); } catch { return; }
    if (!stillCurrent() || editor.session !== sessionId || editor.id !== session.uiId) return;
    const identity: ConflictIdentity = { binding, node_id:session.documentId, operation_id:first.operation_id, operation_seq:first.operation_seq, generation:selected.generation };
    setPendingMerge((previous) => {
      if (previous && previous.instance === instance && previous.sessionId === sessionId && previous.snapshot.operation_id === first.operation_id && previous.intentVersion === selected.version) {
        return { ...previous, queuedOperationId:merge.merge_operation?.operation_id ?? null, queuedOperationSeq:merge.merge_operation?.operation_seq ?? null };
      }
      return { instance, snapshot:identity, sessionId, uiId:editor.id, version, merge:true, intentVersion:selected.version, generation:editor.generation, revision:editor.revision, queuedOperationId:merge.merge_operation?.operation_id ?? null, queuedOperationSeq:merge.merge_operation?.operation_seq ?? null };
    });
    if (selected.needs_review) setConflictMessage("합본을 확인한 뒤 초안 또는 서버 상태가 변경됐습니다. 충돌을 다시 비교해 주세요.");
  }

  return {
    conflictPreparing, conflictMessage, conflictSnapshot, conflictRecovery, conflictRecovering,
    pendingConflict, pendingMerge, pendingCopy, copyRetryOpen, conflictReviewPorts, conflictActionRevision,
    setConflictSnapshot, setConflictMessage, setPendingConflict, setPendingMerge, setPendingCopy, setCopyRetryOpen,
    inspectConflict, conflictContextCurrent, chooseRemote, saveMerged, refreshMerge, queueMerge,
    refreshMergeFollowup, approveMergeFollowup, keepConflictCopy, refreshCopy, retryCopy, progressCopy,
    recoverConflictResult, continueConflict, completeConflict, observeConflictStatus,
  };
}
