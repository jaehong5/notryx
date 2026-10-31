import { useEffect, useRef, useState, type Dispatch, type RefObject, type SetStateAction } from "react";
import { EditorController } from "./editor-controller";
import { AutoTransmit, QueueStorageError, type TransmitScope } from "./auto-transmit";
import { prepareWorkspaceDeletedDraftCopy, confirmWorkspaceDeletedDraftCopy, workspaceDeletedDraftCopyStatus, progressWorkspaceDeletedDraftCopy, reopenWorkspaceDeletedDraftCopy, requestWorkspaceDeletedDraftCopyFollowupAbandon, reconfirmWorkspaceDeletedDraftCopy } from "./ipc";
import type { WorkspaceDeletedDraftCopySnapshot, WorkspaceDeletedDraftCopyStatus, WorkspaceRegistration } from "../../../packages/protocol-types";

type DeletedCopyReview = { instance: EditorController; snapshot: WorkspaceDeletedDraftCopySnapshot; bytes: Uint8Array; revision: number; version: number; uiId: string };
type DeletedCopyStatusContext = { instance: EditorController; binding: WorkspaceRegistration; documentId: string; sessionId: string; uiId: string; version: number; status: WorkspaceDeletedDraftCopyStatus };

type EditorSession = { mode: "fixture" | "workspace"; documentId: string; uiId: string; scope?: TransmitScope; binding?: WorkspaceRegistration; orphanDraft?: boolean };
type DeletedDraftCopyOptions = {
  controller: RefObject<EditorController | null>;
  transmitter: RefObject<AutoTransmit | null>;
  registrationVersion: RefObject<number>;
  workspaceBindingRef: RefObject<WorkspaceRegistration | null>;
  sessionModes: RefObject<Map<string, EditorSession>>;
  copyRevision: number;
  setCopyRevision: Dispatch<SetStateAction<number>>;
};

export function useDeletedDraftCopy({ controller, transmitter, registrationVersion, workspaceBindingRef, sessionModes, copyRevision, setCopyRevision }: DeletedDraftCopyOptions) {
  const [deletedCopyReview, setDeletedCopyReview] = useState<DeletedCopyReview | null>(null);
  const [deletedCopyRecovery, setDeletedCopyRecovery] = useState<DeletedCopyReview | null>(null);
  const [deletedCopyBusy, setDeletedCopyBusy] = useState(false);
  const [deletedCopyRecorded, setDeletedCopyRecorded] = useState(false);
  const [deletedCopyStatus, setDeletedCopyStatus] = useState<DeletedCopyStatusContext | null>(null);
  const deletedCopyStatusRef = useRef<DeletedCopyStatusContext | null>(null);
  deletedCopyStatusRef.current = deletedCopyStatus;
  const deletedCopyStatusRequest = useRef(0);
  const [deletedFollowupApproval, setDeletedFollowupApproval] = useState<WorkspaceDeletedDraftCopyStatus["blocking_followup"]>(null);
  const deletedCopyRun = useRef<{ context: DeletedCopyStatusContext; phase: "followup" | "create" | "content"; followupId?: string } | null>(null);
  const deletedCopyWakePending = useRef(false);
  const [deletedCopyMessage, setDeletedCopyMessage] = useState("");
  const [deletedCopyRetryOpen, setDeletedCopyRetryOpen] = useState(false);
  const deletedCopyAction = useRef(false);

  function deletedCopyCurrent(context: DeletedCopyReview): boolean {
    return controller.current === context.instance && registrationVersion.current === context.version &&
      !!workspaceBindingRef.current && JSON.stringify(workspaceBindingRef.current) === JSON.stringify(context.snapshot.binding) &&
      context.instance.isCurrentSnapshot(context.snapshot.edit_session_id, context.snapshot.generation, context.revision);
  }

  function deletedStatusCurrent(context: Omit<DeletedCopyStatusContext, "status">): boolean {
    const session = sessionModes.current.get(context.sessionId);
    return controller.current === context.instance && context.instance.isCurrentSession(context.sessionId) &&
      registrationVersion.current === context.version && !!workspaceBindingRef.current &&
      JSON.stringify(workspaceBindingRef.current) === JSON.stringify(context.binding) &&
      !!session?.scope && session.uiId === context.uiId && session.documentId === context.documentId &&
      session.scope.binding === JSON.stringify(context.binding);
  }

  async function loadDeletedCopyStatus(context: Omit<DeletedCopyStatusContext, "status">, expected?: { previous: WorkspaceDeletedDraftCopyStatus; nodeId: string; operationId: string; parentId: string; name: string }): Promise<WorkspaceDeletedDraftCopyStatus | null> {
    const attempt = ++deletedCopyStatusRequest.current;
    const status = await workspaceDeletedDraftCopyStatus(context.binding, context.documentId);
    if (attempt !== deletedCopyStatusRequest.current || !deletedStatusCurrent(context)) throw new Error("stale_deleted_copy_status");
    if (status && (JSON.stringify(status.binding) !== JSON.stringify(context.binding) || status.document_id !== context.documentId)) throw new Error("stale_deleted_copy_status");
    if (expected && (!status || status.node_id !== expected.nodeId || status.create_operation_id !== expected.operationId || status.parent_id !== expected.parentId || status.name !== expected.name || status.generation !== expected.previous.generation || status.local_hash !== expected.previous.local_hash || status.tombstone_seq !== expected.previous.tombstone_seq || status.terminal_operation_id !== expected.previous.terminal_operation_id)) throw new Error("stale_deleted_copy_status");
    const next = status ? { ...context, status } : null;
    deletedCopyStatusRef.current = next;
    setDeletedCopyStatus(next);
    setDeletedFollowupApproval((previous) => previous && status?.blocking_followup && previous.blocker_operation_id === status.blocking_followup.blocker_operation_id && previous.followup_operation_id === status.blocking_followup.followup_operation_id && previous.followup_operation_seq === status.blocking_followup.followup_operation_seq && previous.followup_generation === status.blocking_followup.followup_generation && previous.followup_hash === status.blocking_followup.followup_hash ? previous : null);
    if (status) setDeletedCopyRecorded(true);
    return status;
  }

  async function refreshDeletedCopyStatus(): Promise<void> {
    const context = deletedCopyStatusRef.current;
    if (!context || deletedCopyAction.current) return;
    deletedCopyAction.current = true; setDeletedCopyBusy(true);
    try {
      const status = await loadDeletedCopyStatus(context);
      if (!status) throw new Error("missing_deleted_copy");
      setDeletedCopyMessage("사본 상태를 확인했습니다. 원래 문서는 자동으로 복원하지 않습니다.");
    } catch {
      deletedCopyRun.current = null;
      deletedCopyWakePending.current = false;
      if (deletedStatusCurrent(context)) setDeletedCopyMessage("사본 상태를 확인하지 못했습니다. 기기 초안은 보존됩니다.");
    } finally { deletedCopyAction.current = false; setDeletedCopyBusy(false); if (deletedCopyWakePending.current) { deletedCopyWakePending.current = false; setCopyRevision((revision) => revision + 1); } }
  }

  async function reconfirmDeletedCopy(parentId: string, name: string): Promise<void> {
    const context = deletedCopyStatusRef.current;
    if (!context || deletedCopyAction.current || !deletedStatusCurrent(context)) return;
    const previous = context.status;
    if (previous.create_status !== "needs_review" || previous.create_failure !== "destination_exists" || (parentId === previous.parent_id && name === previous.name)) return;
    const requestRevision = deletedCopyStatusRequest.current;
    deletedCopyAction.current = true; setDeletedCopyBusy(true);
    try {
      const result = await reconfirmWorkspaceDeletedDraftCopy(context.binding, context.documentId, previous.create_operation_id, parentId, name);
      if (!deletedStatusCurrent(context) || requestRevision !== deletedCopyStatusRequest.current || deletedCopyStatusRef.current !== context) return;
      if (result.node_id === previous.node_id || result.create_operation_id === previous.create_operation_id) throw new Error("stale_deleted_copy");
      await loadDeletedCopyStatus(context, { previous, nodeId:result.node_id, operationId:result.create_operation_id, parentId, name });
      setDeletedCopyRetryOpen(false);
      setDeletedCopyMessage("새 사본 대상이 이 기기에 기록됐습니다. 서버 사본 저장은 아직 완료되지 않았습니다.");
    } catch {
      if (deletedStatusCurrent(context)) {
        setDeletedCopyRetryOpen(false);
        setDeletedCopyMessage("사본 재승인 결과를 확인하지 못했습니다. 기기 초안은 보존됩니다. 사본 상태를 다시 확인해 주세요.");
      }
    } finally { deletedCopyAction.current = false; setDeletedCopyBusy(false); if (deletedCopyWakePending.current) { deletedCopyWakePending.current = false; setCopyRevision((revision) => revision + 1); } }
  }

  async function progressDeletedCopy(): Promise<void> {
    const context = deletedCopyStatusRef.current;
    if (!context || deletedCopyAction.current || !deletedStatusCurrent(context)) return;
    const status = context.status;
    if (status.blocking_followup !== null || status.create_status === "needs_review" || status.content_status === "needs_review" || status.content_status === "saved") return;
    deletedCopyAction.current = true; setDeletedCopyBusy(true);
    try {
      const result = await progressWorkspaceDeletedDraftCopy(context.binding, context.documentId, status.node_id, status.create_operation_id);
      if (!deletedStatusCurrent(context)) return;
      if ((status.create_status !== "created" && result.operation_id !== status.create_operation_id) || (status.create_status === "created" && result.operation_id === status.create_operation_id) || (status.content_operation_id && result.operation_id !== status.content_operation_id)) throw new Error("stale_deleted_copy_operation");
      deletedCopyRun.current = { context, phase: status.create_status === "created" ? "content" : "create" };
      transmitter.current?.kick();
      setDeletedCopyMessage("사본 저장을 진행합니다. 서버 결과와 ACK를 확인해 주세요.");
      await loadDeletedCopyStatus(context);
    } catch {
      deletedCopyRun.current = null;
      if (deletedStatusCurrent(context)) setDeletedCopyMessage("사본 처리 결과를 확인하지 못했습니다. 기기 초안은 보존됩니다. 상태를 다시 확인해 주세요.");
    } finally { deletedCopyAction.current = false; setDeletedCopyBusy(false); if (deletedCopyWakePending.current) { deletedCopyWakePending.current = false; setCopyRevision((revision) => revision + 1); } }
  }

  async function approveDeletedFollowup(): Promise<void> {
    const context = deletedCopyStatusRef.current, approval = deletedFollowupApproval;
    if (!context || !approval || deletedCopyAction.current || !deletedStatusCurrent(context)) return;
    const shown = context.status.blocking_followup;
    if (!shown || shown.blocker_operation_id !== approval.blocker_operation_id || shown.followup_operation_id !== approval.followup_operation_id || shown.followup_operation_seq !== approval.followup_operation_seq || shown.followup_generation !== approval.followup_generation || shown.followup_hash !== approval.followup_hash) return;
    const requestRevision = deletedCopyStatusRequest.current;
    deletedCopyAction.current = true; setDeletedCopyBusy(true);
    let invoked = false;
    try {
      const snapshot = context.instance.recoverySnapshot();
      if (snapshot.id !== context.uiId || snapshot.session !== context.sessionId) throw new Error("stale_session");
      const result = await context.instance.withDeletedFollowupSnapshot(snapshot, () => {
        if (!deletedStatusCurrent(context) || requestRevision !== deletedCopyStatusRequest.current || deletedCopyStatusRef.current !== context || snapshot.generation !== approval.followup_generation) throw new Error("stale_followup");
      }, async () => {
        invoked = true;
        return requestWorkspaceDeletedDraftCopyFollowupAbandon(context.binding, context.documentId, context.status.node_id, context.status.create_operation_id, approval.blocker_operation_id, approval.followup_operation_id, approval.followup_operation_seq, approval.followup_generation, approval.followup_hash);
      });
      if (!deletedStatusCurrent(context) || requestRevision !== deletedCopyStatusRequest.current) return;
      if (result.operation_id !== approval.followup_operation_id) throw new Error("stale_deleted_followup");
      setDeletedFollowupApproval(null);
      deletedCopyRun.current = { context, phase:"followup", followupId:approval.followup_operation_id };
      const scheduler = transmitter.current;
      if (scheduler && !scheduler.resumeContentAfterAbandon(JSON.stringify(context.binding), approval.followup_operation_id)) scheduler.kick();
      setDeletedCopyMessage("승인한 후속 작업의 처리를 요청했습니다. 서버 결과와 ACK를 확인합니다.");
    } catch (cause) {
      deletedCopyRun.current = null;
      if (deletedStatusCurrent(context)) {
        setDeletedFollowupApproval(null);
        setDeletedCopyMessage(invoked ? "후속 처리 결과를 확인하지 못했습니다. 기기 초안은 보존됩니다. 앱을 다시 시작하고 상태를 확인해 주세요." : String(cause).includes("조합") ? "한글 조합을 마친 뒤 후속 작업을 다시 확인해 주세요." : "기기 초안을 저장한 뒤 후속 작업 상태를 다시 확인해 주세요.");
      }
    } finally { deletedCopyAction.current = false; setDeletedCopyBusy(false); if (deletedCopyWakePending.current) { deletedCopyWakePending.current = false; setCopyRevision((revision) => revision + 1); } }
  }

  async function observeDeletedCopy(): Promise<void> {
    const run = deletedCopyRun.current;
    if (!run || !deletedStatusCurrent(run.context)) return;
    if (deletedCopyAction.current) { deletedCopyWakePending.current = true; return; }
    deletedCopyAction.current = true;
    let advance = false;
    try {
      const status = await loadDeletedCopyStatus(run.context);
      if (!status || !deletedStatusCurrent(run.context) || status.node_id !== run.context.status.node_id || status.create_operation_id !== run.context.status.create_operation_id) throw new Error("stale_deleted_copy_status");
      if (status.content_status === "saved" || status.create_status === "needs_review" || status.content_status === "needs_review") deletedCopyRun.current = null;
      else if (status.blocking_followup) {
        if (run.phase !== "followup" || status.blocking_followup.followup_operation_id !== run.followupId) deletedCopyRun.current = null;
      } else if (run.phase === "followup" || (run.phase === "create" && status.create_status === "created" && status.content_status === "not_queued")) advance = true;
    } catch {
      deletedCopyRun.current = null;
      if (deletedStatusCurrent(run.context)) setDeletedCopyMessage("사본 상태를 확인하지 못했습니다. 기기 초안은 보존됩니다.");
    } finally { deletedCopyAction.current = false; }
    if (advance) await progressDeletedCopy();
    if (deletedCopyWakePending.current) { deletedCopyWakePending.current = false; setCopyRevision((revision) => revision + 1); }
  }

  async function inspectDeletedCopy(): Promise<void> {
    if (deletedCopyAction.current) return;
    deletedCopyAction.current = true; setDeletedCopyBusy(true); setDeletedCopyReview(null); setDeletedCopyMessage("");
    let origin: { instance: EditorController; session: string; binding: string; version: number } | undefined;
    try {
      const instance = controller.current;
      if (!instance) throw new Error("stale_session");
      const current = instance.recoverySnapshot();
      const session = sessionModes.current.get(current.session);
      if (!session?.scope || !session.binding || session.uiId !== current.id || current.generation === "0") throw new Error("stale_session");
      const version = registrationVersion.current, binding = session.binding;
      origin = { instance, session: current.session, binding: JSON.stringify(binding), version };
      const isCurrent = () => controller.current === instance && registrationVersion.current === version &&
        !!workspaceBindingRef.current && JSON.stringify(workspaceBindingRef.current) === JSON.stringify(binding) &&
        instance.isCurrentSnapshot(current.session, current.generation, current.revision);
      await instance.flush();
      if (!isCurrent()) throw new Error("stale_session");
      const snapshot = await prepareWorkspaceDeletedDraftCopy(binding, session.documentId, current.session, current.generation);
      if (!isCurrent() || JSON.stringify(snapshot.binding) !== JSON.stringify(binding) || snapshot.document_id !== session.documentId || snapshot.edit_session_id !== current.session || snapshot.generation !== current.generation) throw new Error("stale_session");
      const bytes = current.bytes.slice();
      const digest = await crypto.subtle.digest("SHA-256", bytes.buffer);
      const hash = "sha256:" + [...new Uint8Array(digest)].map((value) => value.toString(16).padStart(2, "0")).join("");
      if (!isCurrent() || hash !== snapshot.local_hash) throw new Error("stale_source");
      setDeletedCopyReview({ instance, snapshot, bytes, revision: current.revision, version, uiId: current.id });
    } catch (cause) {
      if (!origin || (controller.current === origin.instance && origin.instance.isCurrentSession(origin.session) && registrationVersion.current === origin.version && !!workspaceBindingRef.current && JSON.stringify(workspaceBindingRef.current) === origin.binding)) {
        setDeletedCopyMessage(String(cause).includes("조합") ? "한글 조합을 마친 뒤 다시 확인해 주세요." : "삭제 상태와 기기 초안을 확인하지 못했습니다. 연결과 원문을 다시 확인해 주세요.");
      }
    } finally { deletedCopyAction.current = false; setDeletedCopyBusy(false); }
  }

  async function confirmDeletedCopy(snapshot: WorkspaceDeletedDraftCopySnapshot, parentId: string, name: string): Promise<void> {
    const context = deletedCopyReview;
    if (!context || context.snapshot !== snapshot || deletedCopyAction.current) return;
    deletedCopyAction.current = true; setDeletedCopyBusy(true);
    let invoked = false;
    try {
      const current = context.instance.recoverySnapshot();
      if (!deletedCopyCurrent(context) || current.id !== context.uiId || current.session !== snapshot.edit_session_id || current.generation !== snapshot.generation || current.revision !== context.revision) throw new Error("stale_session");
      await context.instance.withDeletedDraftSnapshot(current, async () => {
        invoked = true;
        return confirmWorkspaceDeletedDraftCopy(snapshot, parentId, name);
      });
      setDeletedCopyReview(null);
      setDeletedCopyRecovery(null);
      setDeletedCopyRecorded(true);
      setDeletedCopyMessage("사본 선택은 보류 상태로 이 기기에 기록됐습니다. 서버 사본 저장은 아직 시작되지 않았습니다.");
      try {
        const status = await loadDeletedCopyStatus({ instance: context.instance, binding: snapshot.binding, documentId: snapshot.document_id, sessionId: snapshot.edit_session_id, uiId: context.uiId, version: context.version });
        if (!status) throw new Error("missing_deleted_copy");
      } catch {
        if (controller.current === context.instance && context.instance.isCurrentSession(snapshot.edit_session_id) && registrationVersion.current === context.version) setDeletedCopyMessage("사본 선택은 보류 상태로 기록됐지만 상태를 확인하지 못했습니다. 기기 초안은 보존됩니다.");
      }
    } catch (cause) {
      if (controller.current !== context.instance || !context.instance.isCurrentSession(snapshot.edit_session_id) || registrationVersion.current !== context.version || !workspaceBindingRef.current || JSON.stringify(workspaceBindingRef.current) !== JSON.stringify(snapshot.binding)) return;
      if (invoked) {
        setDeletedCopyReview(null);
        setDeletedCopyRecovery(context);
        setDeletedCopyMessage("사본 선택 결과를 확인하지 못했습니다. 기기 초안은 보존됩니다. 상태를 확인하고 문서를 다시 열어 주세요.");
      } else setDeletedCopyMessage(String(cause).includes("조합") ? "한글 조합을 마친 뒤 다시 확인해 주세요." : "문서가 변경됐습니다. 초안을 다시 확인해 주세요.");
    } finally { deletedCopyAction.current = false; setDeletedCopyBusy(false); }
  }

  async function recoverDeletedCopy(): Promise<void> {
    const context = deletedCopyRecovery;
    if (!context || deletedCopyAction.current) return;
    const current = () => controller.current === context.instance && context.instance.isCurrentSession(context.snapshot.edit_session_id) &&
      registrationVersion.current === context.version && !!workspaceBindingRef.current && JSON.stringify(workspaceBindingRef.current) === JSON.stringify(context.snapshot.binding);
    if (!current()) return;
    deletedCopyAction.current = true; setDeletedCopyBusy(true);
    let reopenStarted = false;
    try {
      const observed = await workspaceDeletedDraftCopyStatus(context.snapshot.binding, context.snapshot.document_id);
      if (!current() || (observed && (JSON.stringify(observed.binding) !== JSON.stringify(context.snapshot.binding) || observed.document_id !== context.snapshot.document_id))) throw new Error("stale_deleted_copy_status");
      let reopenedBinding: WorkspaceRegistration | undefined;
      let reopenedScope: TransmitScope | undefined;
      const state = await context.instance.reopenDeletedDraftCheckpoint(async (id, previousSessionId) => {
        if (id !== context.uiId || previousSessionId !== context.snapshot.edit_session_id || !current()) throw new Error("stale_session");
        reopenStarted = true;
        const result = await reopenWorkspaceDeletedDraftCopy(context.snapshot.binding, context.snapshot.document_id, previousSessionId);
        const document = result.document;
        if (JSON.stringify(document.binding) !== JSON.stringify(context.snapshot.binding) || document.document_id !== context.snapshot.document_id || !document.edit_session_id || document.edit_session_id === previousSessionId || document.readonly || !document.local_draft || (result.state && (JSON.stringify(result.state.binding) !== JSON.stringify(context.snapshot.binding) || result.state.document_id !== context.snapshot.document_id))) throw new Error("stale_deleted_copy_document");
        reopenedBinding = document.binding;
        reopenedScope = document.has_session ? { binding: JSON.stringify(document.binding), documentId: document.document_id, editSessionId: document.edit_session_id } : undefined;
        sessionModes.current.set(document.edit_session_id, { mode:"workspace", documentId:document.document_id, uiId:id, binding:document.binding, scope:reopenedScope, orphanDraft:true });
        return { state:result.state, document:{ ...document, document_id:id } };
      });
      setDeletedCopyRecovery(null);
      const reopened = context.instance.recoverySnapshot();
      if (state && reopenedBinding && reopenedScope) {
        setDeletedCopyStatus({ instance:context.instance, binding:reopenedBinding, documentId:context.snapshot.document_id, sessionId:reopened.session, uiId:reopened.id, version:context.version, status:state });
        setDeletedCopyRecorded(true);
        setDeletedCopyMessage("사본 선택 상태를 확인하고 기기 초안을 다시 열었습니다.");
      } else {
        setDeletedCopyStatus(null); setDeletedCopyRecorded(!!state);
        setDeletedCopyMessage(state ? "기기 인증 또는 작업 공간 연결을 확인해 주세요. 초안은 보존됩니다." : "사본 선택이 기록되지 않았습니다. 기기 초안을 다시 확인해 주세요.");
      }
    } catch {
      if (registrationVersion.current !== context.version || !workspaceBindingRef.current || JSON.stringify(workspaceBindingRef.current) !== JSON.stringify(context.snapshot.binding)) return;
      if (reopenStarted) { setDeletedCopyRecovery(null); setDeletedCopyMessage("재열기 결과를 확인하지 못했습니다. 기기 초안은 보존됩니다. 앱을 다시 시작해 주세요."); }
      else if (current()) setDeletedCopyMessage("사본 상태를 확인하지 못했습니다. 기기 초안은 보존됩니다. 다시 확인해 주세요.");
    } finally { deletedCopyAction.current = false; setDeletedCopyBusy(false); }
  }

  useEffect(() => { if (copyRevision > 0) void observeDeletedCopy(); }, [copyRevision]);

  function resetDeletedCopy(): void {
    setDeletedCopyReview(null);
    setDeletedCopyRecovery(null);
    setDeletedCopyMessage("");
    setDeletedCopyRecorded(false);
    setDeletedCopyStatus(null);
    setDeletedCopyRetryOpen(false);
    setDeletedFollowupApproval(null);
    deletedCopyRun.current = null;
  }

  function editedDeletedCopy(sessionId: string): void {
    setDeletedCopyReview((current) => current?.snapshot.edit_session_id === sessionId ? null : current);
  }

  function restoreDeletedCopy(sessionId: string, generation: string, scheduler: AutoTransmit): boolean {
    const session = sessionModes.current.get(sessionId);
    const instance = controller.current;
    if (!session?.orphanDraft || !session.binding || !session.scope || !instance) return false;
    const scope = session.scope;
    const context = { instance, binding: session.binding, documentId: session.documentId, sessionId, uiId: session.uiId, version: registrationVersion.current };
    void loadDeletedCopyStatus(context).then((status) => {
      if (!status && deletedStatusCurrent(context)) return scheduler.restore(scope, generation);
    }).catch((cause) => {
      if (deletedStatusCurrent(context) && !(cause instanceof QueueStorageError)) setDeletedCopyMessage("삭제 사본 상태를 확인하지 못했습니다. 기기 초안은 보존됩니다.");
    });
    return true;
  }

  return {
    deletedCopyReview, deletedCopyRecovery, deletedCopyBusy, deletedCopyRecorded, deletedCopyStatus,
    deletedFollowupApproval, deletedCopyMessage, deletedCopyRetryOpen,
    setDeletedCopyReview, setDeletedCopyRetryOpen, setDeletedFollowupApproval,
    deletedCopyCurrent, deletedStatusCurrent, refreshDeletedCopyStatus, reconfirmDeletedCopy,
    progressDeletedCopy, approveDeletedFollowup, inspectDeletedCopy, confirmDeletedCopy, recoverDeletedCopy,
    resetDeletedCopy, editedDeletedCopy, restoreDeletedCopy,
  };
}
