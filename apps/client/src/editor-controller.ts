import type { M0CloseRequest, M0OpenedDocument, M0SavedCheckpoint, M0SaveRequest, WorkspaceCacheWarning, WorkspaceConflictResolutionState, WorkspaceContentConflictChooseRemoteResponse, WorkspaceContentConflictProgressResponse, WorkspaceDeletedDraftCopyConfirmResult, WorkspaceDeletedDraftCopyStatus } from "../../../packages/protocol-types";
import { Draft } from "./draft";
import { fromBase64, toBase64 } from "./ipc";
import { openSource, type Source } from "./source";

export type EditorViewHandle = {
  show(): void;
  destroy(): void;
  setEditable(enabled: boolean): void;
  isComposing(): boolean;
  replaceSource(source: Source): void;
};

export type RemoteRefreshCandidate = { token: string; bytes_base64: string };

export type EditorOptions = {
  source: Source;
  change(text: string): void;
  inputRejected(): void;
  composition(active: boolean): void;
  save(): void;
};

export type EditorState = {
  selected: string;
  status: string;
  error: string;
  notice: string;
  remoteChanged: boolean;
  remoteDeleted: boolean;
};

export const initialEditorState: EditorState = { selected: "", status: "문서를 여는 중", error: "", notice: "", remoteChanged: false, remoteDeleted: false };

type OpenedDocument = M0OpenedDocument & { readonly?: boolean; local_draft?: boolean; cache_warning?: WorkspaceCacheWarning | null; open_notice?: "prior_draft_preserved" | null };
type RecoveryOpenState = WorkspaceConflictResolutionState | { kind: "deleted_draft_copy"; state: WorkspaceDeletedDraftCopyStatus | null };

export type EditorPorts = {
  openDocument(id: string): Promise<OpenedDocument>;
  saveDocument(request: M0SaveRequest): Promise<M0SavedCheckpoint>;
  closeDocument(request: M0CloseRequest): Promise<void>;
  exit(): Promise<void>;
  createEditor(options: EditorOptions): EditorViewHandle;
  stateChanged(state: EditorState): void;
  manualSync?(documentId: string, sessionId: string): Promise<void>;
  sourceChanged?(text: string, documentChanged: boolean): void;
  edited?(documentId: string, sessionId: string, generation: string): void;
  opened?(documentId: string, sessionId: string, generation: string, localDraft: boolean, conflictRecovery?: RecoveryOpenState): void;
  compositionChanged?(documentId: string, sessionId: string, active: boolean): void;
  prepareRemoteRefresh?(documentId: string, sessionId: string): Promise<RemoteRefreshCandidate | null>;
  commitRemoteRefresh?(documentId: string, sessionId: string, token: string): Promise<boolean>;
  inspectRemoteChange?(documentId: string, sessionId: string, generation: string): Promise<"same" | "changed" | "deleted" | "pending">;
  remoteInspectionVersion?(): number | string;
  holdConflictSession?(documentId: string, sessionId: string): Promise<void>;
  releaseConflictSession?(documentId: string, sessionId: string): void;
  retireConflictSession?(documentId: string, sessionId: string): void;
};

type ActiveDocument = {
  id: string;
  draft: Draft;
  view: EditorViewHandle;
  closing?: Promise<void>;
  failedAttempt?: number;
  retryBlocked?: boolean;
  remoteBlocked?: boolean;
  conflict?: { phase: "preparing" | "invoked" | "retired" | "save_blocked" | "retire_blocked"; prepared: Promise<void>; settle(): void; held: boolean; registration: number | string | undefined };
};

export class EditorController {
  private current?: ActiveDocument;
  private timer?: ReturnType<typeof setTimeout>;
  private firstPendingAt?: number;
  private lastEditAt?: number;
  private automaticSave?: Promise<void>;
  private retryAt?: number;
  private retryDelay = 1000;
  private busy?: "opening" | "closing" | "refreshing" | "resolving";
  private remoteRefresh?: Promise<void>;
  private remoteRefreshRequested = false;
  private remoteInspection?: Promise<void>;
  private remoteInspectionRequested = false;
  private inspectAfterBusy = false;
  private currentRevision = 0;
  private closeRequested = false;
  private disposed = false;
  private state = initialEditorState;

  constructor(private ports: EditorPorts) {}

  isCurrentSaved(sessionId: string, generation: string): boolean {
    const document = this.current;
    return !!document && !document.remoteBlocked && document.draft.session === sessionId && !document.draft.dirty && document.draft.generation.toString() === generation;
  }

  isCurrentSession(sessionId: string): boolean {
    return this.current?.draft.session === sessionId;
  }

  isBlockedRecoverySnapshot(session: string, generation: string, revision: number): boolean {
    const current = this.current;
    return !!current && !this.disposed && current.conflict?.phase === "save_blocked"
      && current.draft.session === session && current.draft.generation.toString() === generation && this.currentRevision === revision;
  }

  recoverySnapshot(): { id: string; session: string; generation: string; revision: number; bytes: Uint8Array } {
    const current = this.current;
    if (!current || (current.remoteBlocked && current.conflict?.phase !== "save_blocked") || this.busy || this.disposed) throw new Error("열린 문서를 다시 확인해 주세요.");
    if (current.view.isComposing()) throw new Error("한글 조합을 마친 뒤 다시 시도해 주세요.");
    return { id: current.id, session: current.draft.session, generation: current.draft.generation.toString(), revision: this.currentRevision, bytes: current.draft.recoveryBytes() };
  }

  isCurrentSnapshot(session: string, generation: string, revision: number): boolean {
    const current = this.current;
    return !!current && !this.disposed && !this.busy && !current.remoteBlocked && !current.view.isComposing()
      && this.currentRevision === revision && current.draft.session === session && current.draft.generation.toString() === generation;
  }

  async withConflictSnapshot<T extends WorkspaceContentConflictChooseRemoteResponse>(
    expected: { id: string; session: string; generation: string; revision: number },
    action: () => Promise<T>,
  ): Promise<T> {
    return this.withHeldSnapshot(expected, action, true);
  }

  async withDeletedDraftSnapshot(
    expected: { id: string; session: string; generation: string; revision: number },
    action: () => Promise<WorkspaceDeletedDraftCopyConfirmResult>,
  ): Promise<WorkspaceDeletedDraftCopyConfirmResult> {
    return this.withHeldSnapshot(expected, action, false);
  }

  async withDeletedFollowupSnapshot<T>(
    expected: { id: string; session: string; generation: string; revision: number },
    validate: () => void,
    action: () => Promise<T>,
  ): Promise<T> {
    const result = await this.withHeldSnapshot(expected, async () => ({ status: "pending" as const, value: await action() }), false, validate);
    return result.value;
  }

  async withMergeQueueSnapshot(
    expected: { id: string; session: string; generation: string; revision: number },
    action: () => Promise<WorkspaceContentConflictProgressResponse>,
  ): Promise<WorkspaceContentConflictProgressResponse> {
    return this.withHeldSnapshot(expected, action, false, undefined, true);
  }

  private async withHeldSnapshot<T extends { status: string }>(
    expected: { id: string; session: string; generation: string; revision: number },
    action: () => Promise<T>,
    permitsCompletion: boolean,
    validate?: () => void,
    retirePending = false,
  ): Promise<T> {
    const document = this.current;
    if (!document || document.id !== expected.id || !this.isCurrentSnapshot(expected.session, expected.generation, expected.revision)) {
      throw new Error("문서가 변경됐습니다. 충돌을 다시 확인해 주세요.");
    }
    const hold = this.ports.holdConflictSession, release = this.ports.releaseConflictSession, retire = this.ports.retireConflictSession;
    if (!hold || !release || !retire) throw new Error("충돌 해결을 준비하지 못했습니다.");
    const registration = this.ports.remoteInspectionVersion?.();
    let settle!: () => void;
    const prepared = new Promise<void>((resolve) => { settle = resolve; });
    const conflict: NonNullable<ActiveDocument["conflict"]> = { phase: "preparing", prepared, settle, held: false, registration };
    document.conflict = conflict;
    this.busy = "resolving";
    document.view.setEditable(false);
    try {
      await hold(document.id, expected.session);
      conflict.held = true;
      if (this.disposed || this.current !== document) throw new Error("문서가 닫혔습니다.");
      await this.flush();
      if (this.disposed || this.current !== document || document.remoteBlocked || document.view.isComposing() ||
        document.draft.session !== expected.session || document.draft.generation.toString() !== expected.generation ||
        this.currentRevision !== expected.revision || this.ports.remoteInspectionVersion?.() !== registration) {
        throw new Error("문서가 변경됐습니다. 충돌을 다시 확인해 주세요.");
      }
      validate?.();
      conflict.phase = "invoked";
      conflict.settle();
      const result = await action();
      if (this.disposed || this.current !== document || document.view.isComposing() || this.ports.remoteInspectionVersion?.() !== registration) {
        throw new Error("작업 공간이나 편집 상태가 변경됐습니다.");
      }
      if ((permitsCompletion && result.status === "completed") || (retirePending && result.status === "pending")) {
        document.remoteBlocked = true;
        try {
          retire(document.id, expected.session);
          conflict.phase = "retired";
        } catch (cause) {
          conflict.phase = "retire_blocked";
          throw cause;
        } finally { conflict.held = false; }
        this.publish({ notice: retirePending ? "합본 전송을 준비했습니다. 문서를 다시 열어 상태를 확인해 주세요." : "충돌 선택 상태를 확인했습니다. 문서를 다시 열어 확인해 주세요.", error: "" });
      } else if (result.status === "pending") {
        try {
          release(document.id, expected.session);
          conflict.held = false;
          document.conflict = undefined;
        } catch (cause) {
          document.remoteBlocked = true;
          conflict.phase = "retire_blocked";
          conflict.held = false;
          try { retire(document.id, expected.session); conflict.phase = "retired"; } catch { /* The editor remains blocked until recovery. */ }
          throw cause;
        }
      } else {
        throw new Error("충돌 해결 결과를 확인할 수 없습니다.");
      }
      return result;
    } catch (cause) {
      const invoked = conflict.phase === "invoked";
      if (conflict.held) {
        if (invoked) {
          document.remoteBlocked = true;
          conflict.phase = "retire_blocked";
          conflict.held = false;
          try { retire(document.id, expected.session); conflict.phase = "retired"; } catch { /* The editor remains blocked until recovery. */ }
        } else {
          try {
            release(document.id, expected.session);
            conflict.held = false;
            document.conflict = undefined;
          } catch {
            document.remoteBlocked = true;
            conflict.phase = "save_blocked";
          }
        }
      }
      if (!this.disposed && this.current === document && (invoked || document.remoteBlocked || this.ports.remoteInspectionVersion?.() !== registration)) {
        document.remoteBlocked = true;
        this.publish({ error: conflict.phase === "save_blocked" ? "기기 초안 저장 또는 편집 잠금 해제에 실패했습니다. 저장을 다시 시도하거나 원문을 복구해 주세요." : "충돌 해결 결과를 확인할 수 없습니다. 문서를 다시 열어 확인해 주세요." });
      }
      throw cause;
    } finally {
      conflict.settle();
      this.busy = undefined;
      this.restoreEditing();
      this.resumeRemoteInspection();
    }
  }

  async open(id: string): Promise<void> {
    await this.openInternal(id);
  }

  async reopenConflictCheckpoint(action: (documentId: string, previousSessionId: string) => Promise<{ state: WorkspaceConflictResolutionState; document: OpenedDocument }>): Promise<WorkspaceConflictResolutionState> {
    const previous = this.current;
    if (!previous || this.disposed || this.busy) throw new Error("충돌 복구를 다시 확인해 주세요.");
    this.retryConflictRetirement(previous);
    if (previous.conflict?.phase !== "retired") throw new Error("충돌 복구를 다시 확인해 주세요.");
    const result = await this.openInternal(previous.id, () => action(previous.id, previous.draft.session));
    if (!result || "kind" in result) throw new Error("충돌 복구를 완료하지 못했습니다.");
    return result;
  }

  async reopenDeletedDraftCheckpoint(action: (documentId: string, previousSessionId: string) => Promise<{ state: WorkspaceDeletedDraftCopyStatus | null; document: OpenedDocument }>): Promise<WorkspaceDeletedDraftCopyStatus | null> {
    const previous = this.current;
    if (!previous || this.disposed || this.busy) throw new Error("초안 복구를 다시 확인해 주세요.");
    this.retryConflictRetirement(previous);
    if (previous.conflict?.phase !== "retired") throw new Error("초안 복구를 다시 확인해 주세요.");
    const result = await this.openInternal(previous.id, async () => {
      const reopened = await action(previous.id, previous.draft.session);
      return { state: { kind: "deleted_draft_copy", state: reopened.state }, document: reopened.document };
    });
    if (!result || !("kind" in result) || result.kind !== "deleted_draft_copy") throw new Error("초안 복구를 완료하지 못했습니다.");
    return result.state;
  }

  private retryConflictRetirement(document: ActiveDocument): void {
    if (document.conflict?.phase !== "retire_blocked") return;
    const retire = this.ports.retireConflictSession;
    if (!retire) throw new Error("충돌 복구를 다시 확인해 주세요.");
    retire(document.id, document.draft.session);
    document.conflict.held = false;
    document.conflict.phase = "retired";
  }

  private async openInternal(id: string, recover?: () => Promise<{ state: RecoveryOpenState; document: OpenedDocument }>): Promise<RecoveryOpenState | undefined> {
    if (this.disposed || this.busy || (!recover && (this.current?.remoteBlocked || id === this.current?.id))) {
      if (recover) throw new Error("충돌 복구를 다시 확인해 주세요.");
      return;
    }
    if (this.current?.view.isComposing()) {
      this.publish({ error: "한글 조합을 마친 뒤 문서를 전환해 주세요." });
      return;
    }
    this.busy = "opening";
    this.current?.view.setEditable(false);
    const previous = this.current;
    const registration = this.ports.remoteInspectionVersion?.();
    let recoveryState: RecoveryOpenState | undefined;
    let response: OpenedDocument | undefined;
    let prepared: ActiveDocument | undefined;
    try {
      if (!recover) await this.flush();
      if (this.disposed) return;
      if (recover) {
        const result = await recover();
        response = result.document;
        recoveryState = result.state;
        if (!previous || response.document_id !== id || !response.edit_session_id || response.edit_session_id === previous.draft.session) {
          response = undefined;
          throw new Error("다른 충돌 복구 결과입니다.");
        }
        if (!this.disposed && this.ports.remoteInspectionVersion?.() !== registration) throw new Error("작업 공간이 변경됐습니다.");
      } else response = await this.ports.openDocument(id);
      if (this.disposed) {
        await this.ports.closeDocument({ document_id: id, edit_session_id: response.edit_session_id });
        response = undefined;
        return;
      }
      const source = openSource(fromBase64(response.bytes_base64));
      if (response.readonly && !source.readonly) {
        source.readonly = true;
        source.reason = "이 문서는 읽기 전용입니다.";
      }
      const session = response.edit_session_id;
      const draft = new Draft(source, session, response.generation, (generation, bytes) =>
        this.ports.saveDocument({ document_id: id, edit_session_id: session, generation, bytes_base64: toBase64(bytes) }));
      const view = this.ports.createEditor({
        source,
        change: (text) => {
          if (this.disposed || this.busy || this.current !== prepared || !prepared || prepared.remoteBlocked || prepared.draft.source.readonly) return;
          prepared.draft.change(text);
          this.ports.edited?.(id, session, prepared.draft.generation.toString());
          this.ports.sourceChanged?.(text, false);
          if (!prepared?.retryBlocked && this.state.status !== "저장 대기 중") this.publish({ status: "저장 대기 중" });
          this.schedule();
        },
        inputRejected: () => {
          if (!this.disposed && this.current === prepared) this.publish({ error: "입력은 적용되지 않았습니다. 편집 가능한 최대 크기는 5 MiB입니다." });
        },
        composition: (active) => {
          if (!this.disposed && this.current === prepared) this.ports.compositionChanged?.(id, session, active);
        },
        save: () => { void this.save().catch(() => {}); },
      });
      prepared = { id, draft, view };
      view.setEditable(false);
      view.show();
      if (previous && !recover) await this.release(previous);
      if (this.disposed) {
        view.destroy();
        await this.release(prepared);
        return;
      }
      this.current = prepared;
      this.currentRevision++;
      if (!recover) {
        previous?.view.destroy();
        this.ports.sourceChanged?.(source.text, true);
      }
      const cacheNotice = response.cache_warning === "storage" ? "기기 캐시에 저장하지 못했습니다."
        : response.cache_warning === "damaged" ? "기기 캐시 상태를 확인해야 합니다."
        : response.cache_warning === "unexpected" ? "기기 캐시 오류를 확인해야 합니다." : "";
      const openNotice = response.open_notice === "prior_draft_preserved" ? "이전 초안을 보존해 최신 본문을 읽기 전용으로 표시합니다." : "";
      const sourceNotice = source.reason ?? (response.local_draft ? "보존된 기기 초안입니다. 서버 상태는 아직 확인하지 않았습니다." : source.normalizeOnEdit ? "수정하면 LF로 저장됩니다." : "");
      const nextState = {
        selected: id,
        notice: [sourceNotice, openNotice, cacheNotice].filter(Boolean).join(" "),
        status: source.readonly ? "읽기 전용, 원본 보존" : response.generation === "0" ? "수정 없음" : "이 기기에 저장됨",
        error: "",
        remoteChanged: false,
        remoteDeleted: false,
      };
      if (!recover) this.publish(nextState);
      try {
        if (recoveryState) this.ports.opened?.(id, session, response.generation, response.local_draft === true, recoveryState);
        else this.ports.opened?.(id, session, response.generation, response.local_draft === true);
      }
      catch (cause) {
        if (recover) throw cause;
        this.publish({ error: String(cause) });
      }
      if (recover) {
        previous?.view.destroy();
        this.ports.sourceChanged?.(source.text, true);
        this.publish(nextState);
      }
      return recoveryState;
    } catch (cause) {
      prepared?.view.destroy();
      if (recover && prepared && this.current === prepared) {
        this.current = previous;
        this.currentRevision++;
      }
      if (!this.disposed) previous?.view.show();
      let error = String(cause);
      if (response) {
        try {
          if (prepared) await this.release(prepared);
          else await this.ports.closeDocument({ document_id: id, edit_session_id: response.edit_session_id });
        } catch (cleanup) {
          error += ` (세션 정리 실패: ${String(cleanup)})`;
        }
      }
      this.publish({ error });
      if (recover) throw cause;
    } finally {
      this.busy = undefined;
      this.restoreEditing();
      this.resumeRemoteInspection();
      if (this.closeRequested) {
        this.closeRequested = false;
        await this.close();
      }
    }
  }

  async refreshRemote(): Promise<void> {
    this.remoteRefreshRequested = true;
    if (this.remoteRefresh) return this.remoteRefresh;
    const task = (async () => {
      while (this.remoteRefreshRequested && !this.disposed) {
        this.remoteRefreshRequested = false;
        await this.refreshRemoteOnce();
      }
    })();
    this.remoteRefresh = task;
    try { await task; }
    finally { if (this.remoteRefresh === task) this.remoteRefresh = undefined; }
  }

  async inspectRemoteChange(): Promise<void> {
    this.remoteInspectionRequested = true;
    if (this.remoteInspection) return this.remoteInspection;
    const task = (async () => {
      while (this.remoteInspectionRequested && !this.disposed) {
        this.remoteInspectionRequested = false;
        await this.inspectRemoteChangeOnce();
      }
    })();
    this.remoteInspection = task;
    try { await task; }
    finally { if (this.remoteInspection === task) this.remoteInspection = undefined; }
  }

  private async inspectRemoteChangeOnce(): Promise<void> {
    const document = this.current;
    const inspect = this.ports.inspectRemoteChange;
    if (!document || !inspect || this.disposed || document.remoteBlocked) return;
    if (this.busy) { this.inspectAfterBusy = true; return; }
    const session = document.draft.session;
    const generation = document.draft.generation.toString();
    const revision = this.currentRevision;
    const registration = this.ports.remoteInspectionVersion?.();
    let result: "same" | "changed" | "deleted" | "pending" | undefined;
    try { result = await inspect(document.id, session, generation); }
    catch { result = undefined; }
    if (this.disposed) return;
    if (this.current !== document || this.currentRevision !== revision || document.draft.session !== session ||
        document.draft.generation.toString() !== generation || this.ports.remoteInspectionVersion?.() !== registration || this.busy) {
      if (this.busy) this.inspectAfterBusy = true;
      if (this.current && !this.busy) this.remoteInspectionRequested = true;
      return;
    }
    if (result === "deleted") this.publish({ remoteChanged: false, remoteDeleted: true });
    else if (result === "changed") this.publish({ remoteChanged: true, remoteDeleted: false });
    else if (result === "same") this.publish({ remoteChanged: false, remoteDeleted: false });
  }

  private async refreshRemoteOnce(): Promise<void> {
    const document = this.current;
    const prepare = this.ports.prepareRemoteRefresh;
    const commit = this.ports.commitRemoteRefresh;
    if (!document || !prepare || !commit || this.disposed || this.busy || document.remoteBlocked ||
        document.draft.dirty || document.draft.saving || document.view.isComposing()) return;
    const session = document.draft.session;
    const generation = document.draft.generation;
    let candidate: RemoteRefreshCandidate | null;
    let source: Source;
    try {
      candidate = await prepare(document.id, session);
      if (!candidate) return;
      source = openSource(fromBase64(candidate.bytes_base64));
    } catch {
      if (!this.disposed && this.current === document) this.publish({ error: "최신 문서를 확인하지 못했습니다." });
      return;
    }
    if (this.disposed || this.current !== document || this.busy || document.remoteBlocked ||
        document.draft.session !== session || document.draft.generation !== generation || document.draft.dirty ||
        document.draft.saving || document.view.isComposing()) return;
    this.busy = "refreshing";
    try {
      document.view.setEditable(false);
      let committed: boolean;
      try { committed = await commit(document.id, session, candidate.token); }
      catch {
        document.remoteBlocked = true;
        this.publish({ error: "문서 갱신 결과를 확인할 수 없습니다. 앱을 다시 열어 확인해 주세요." });
        return;
      }
      if (!committed || this.disposed || this.current !== document) return;
      try {
        document.view.replaceSource(source);
        document.draft = new Draft(source, session, generation.toString(), (generation, bytes) =>
          this.ports.saveDocument({ document_id: document.id, edit_session_id: session, generation, bytes_base64: toBase64(bytes) }));
        this.currentRevision++;
        this.ports.sourceChanged?.(source.text, true);
        this.publish({ status: source.readonly ? "읽기 전용, 원본 보존" : "수정 없음", notice: source.reason ?? (source.normalizeOnEdit ? "수정하면 LF로 저장됩니다." : ""), error: "", remoteChanged: false, remoteDeleted: false });
      } catch {
        document.remoteBlocked = true;
        this.publish({ error: "갱신한 문서를 표시하지 못했습니다. 앱을 다시 열어 확인해 주세요." });
      }
    } finally {
      this.busy = undefined;
      this.restoreEditing();
      this.resumeRemoteInspection();
      if (this.closeRequested) {
        this.closeRequested = false;
        await this.close();
      }
    }
  }

  async flush(): Promise<void> {
    this.clearTimer();
    const document = this.current;
    if (this.disposed || !document) return;
    if (document.draft.dirty) this.publish({ status: "이 기기에 저장 중" });
    try {
      await document.draft.flush();
      if (this.current === document) {
        document.retryBlocked = false;
        this.resetRetry();
        this.clearTimer();
        this.publish({ status: document.draft.source.readonly ? "읽기 전용, 원본 보존" : document.draft.generation === 0n ? "수정 없음" : "이 기기에 저장됨", error: "" });
      }
    } catch (cause) {
      this.recordSaveFailure(document, cause);
      throw cause;
    } finally {
      if (!this.disposed && this.current === document && document.draft.dirty) this.scheduleTimer();
    }
  }

  async save(): Promise<void> {
    const document = this.current;
    if (this.disposed || !document || this.busy || (document.remoteBlocked && document.conflict?.phase !== "save_blocked")) return;
    if (document.view.isComposing()) {
      this.publish({ error: "한글 조합을 마친 뒤 저장해 주세요." });
      return;
    }
    await this.flush();
    if (this.disposed || this.current !== document) return;
    if (document.conflict?.phase === "save_blocked") {
      if (this.ports.remoteInspectionVersion?.() !== document.conflict.registration) {
        this.publish({ error: "기기 초안은 저장됐지만 작업 공간이 변경됐습니다. 원문을 복구하고 등록 상태를 확인해 주세요." });
        return;
      }
      try {
        this.ports.releaseConflictSession?.(document.id, document.draft.session);
        document.conflict.held = false;
        document.conflict = undefined;
        document.remoteBlocked = false;
        this.restoreEditing();
      } catch {
        this.publish({ error: "기기 초안은 저장됐지만 편집 잠금을 해제하지 못했습니다. 원문을 복사하거나 내보낼 수 있습니다." });
      }
      return;
    }
    try {
      await this.ports.manualSync?.(document.id, document.draft.session);
    } catch (cause) {
      if (!this.disposed && this.current === document) this.publish({ error: String(cause) });
      throw cause;
    }
  }

  async prepareForWorkspaceChange(): Promise<boolean> {
    return this.closeCurrentDocumentFor("작업 공간을 전환해");
  }

  async closeCurrentDocument(): Promise<boolean> {
    return this.closeCurrentDocumentFor("문서를 닫아");
  }

  private async closeCurrentDocumentFor(action: string): Promise<boolean> {
    if (this.disposed || this.busy || this.current?.remoteBlocked) return false;
    if (this.current?.view.isComposing()) {
      this.publish({ error: `한글 조합을 마친 뒤 ${action} 주세요.` });
      return false;
    }
    this.busy = "closing";
    const document = this.current;
    document?.view.setEditable(false);
    try {
      await this.flush();
      if (document) await this.release(document);
      if (this.disposed) return false;
      document?.view.destroy();
      this.current = undefined;
      this.ports.sourceChanged?.("", true);
      this.publish({ selected: "", status: "문서 없음", notice: "", error: "", remoteChanged: false, remoteDeleted: false });
      return true;
    } catch (cause) {
      this.publish({ error: String(cause) });
      return false;
    } finally {
      this.busy = undefined;
      this.restoreEditing();
      this.resumeRemoteInspection();
    }
  }

  async close(): Promise<void> {
    if (this.disposed) return;
    if (this.current?.conflict?.phase === "retired" || this.current?.conflict?.phase === "retire_blocked") {
      if (this.busy) return;
      this.busy = "closing";
      let exiting = false;
      try {
        await this.ports.exit();
        exiting = true;
      } catch (cause) {
        this.publish({ error: String(cause) });
      } finally {
        if (!exiting) this.busy = undefined;
      }
      return;
    }
    if (this.busy) {
      if (this.busy === "opening" || this.busy === "refreshing") this.closeRequested = true;
      return;
    }
    if (this.current?.view.isComposing()) {
      this.publish({ error: "한글 조합을 마친 뒤 창을 닫아 주세요." });
      return;
    }
    this.busy = "closing";
    this.current?.view.setEditable(false);
    let exiting = false;
    try {
      await this.flush();
      // Keep the session usable until the process exits; a failed exit can be retried.
      if (!this.disposed) {
        await this.ports.exit();
        exiting = true;
      }
    } catch (cause) {
      this.publish({ error: String(cause) });
    } finally {
      if (!exiting) {
        this.busy = undefined;
        this.restoreEditing();
        this.resumeRemoteInspection();
      }
    }
  }

  async dispose(): Promise<void> {
    if (this.disposed) return;
    if (this.current?.conflict?.phase === "save_blocked" && this.current.draft.dirty) {
      await this.flush();
    }
    this.disposed = true;
    this.closeRequested = false;
    this.clearTimer();
    const document = this.current;
    this.current = undefined;
    if (document) {
      document.view.destroy();
      if (document.conflict?.phase === "preparing") await document.conflict.prepared;
      if (document.conflict?.phase === "invoked" && document.conflict.held) {
        this.ports.retireConflictSession?.(document.id, document.draft.session);
        document.conflict.held = false;
        document.conflict.phase = "retired";
      }
      if (document.conflict?.phase === "retired" || document.conflict?.phase === "retire_blocked") return;
      await document.draft.flush();
      await this.release(document);
    }
  }

  private schedule() {
    const now = performance.now();
    this.firstPendingAt ??= now;
    this.lastEditAt = now;
    this.scheduleTimer();
  }

  private scheduleTimer() {
    if (this.firstPendingAt === undefined || this.lastEditAt === undefined || this.disposed || this.current?.retryBlocked) return;
    clearTimeout(this.timer);
    const deadline = Math.max(Math.min(this.lastEditAt + 1000, this.firstPendingAt + 5000), this.retryAt ?? 0);
    this.timer = setTimeout(() => {
      this.timer = undefined;
      this.automaticCheckpoint();
    }, Math.max(0, deadline - performance.now()));
  }

  private automaticCheckpoint() {
    const document = this.current;
    if (this.disposed || !document || !document.draft.dirty) {
      this.clearTimer();
      return;
    }
    if (this.automaticSave || document.draft.saving) return;
    this.clearTimer();
    this.publish({ status: "이 기기에 저장 중" });
    const saving = document.draft.flushOnce();
    this.automaticSave = saving;
    void saving.then(() => {
      if (this.disposed || this.current !== document) return;
      document.retryBlocked = false;
      this.resetRetry();
      if (document.draft.dirty) this.publish({ status: "저장 대기 중", error: "" });
      else {
        this.clearTimer();
        this.publish({ status: "이 기기에 저장됨", error: "" });
      }
    }, (cause: unknown) => this.recordSaveFailure(document, cause)).finally(() => {
      if (this.automaticSave === saving) this.automaticSave = undefined;
      if (!this.disposed && this.current === document && document.draft.dirty) this.scheduleTimer();
    });
  }

  private clearTimer() {
    clearTimeout(this.timer);
    this.timer = undefined;
    this.firstPendingAt = undefined;
    this.lastEditAt = undefined;
  }

  private resetRetry() {
    this.retryAt = undefined;
    this.retryDelay = 1000;
  }

  private recordSaveFailure(document: ActiveDocument, cause: unknown) {
    if (this.disposed || this.current !== document) return;
    if (document.failedAttempt === document.draft.saveAttempt) return;
    document.failedAttempt = document.draft.saveAttempt;
    this.publish({ status: "저장 오류, 입력 유지 중", error: String(cause) });
    if (cause !== "Storage") {
      document.retryBlocked = true;
      this.clearTimer();
      return;
    }
    document.retryBlocked = false;
    const now = performance.now();
    this.firstPendingAt ??= now;
    this.lastEditAt ??= now;
    this.retryAt = now + this.retryDelay;
    this.retryDelay = Math.min(this.retryDelay * 2, 30_000);
    this.scheduleTimer();
  }

  private release(document: ActiveDocument): Promise<void> {
    document.closing ??= this.ports.closeDocument({ document_id: document.id, edit_session_id: document.draft.session }).catch((cause) => {
      document.closing = undefined;
      throw cause;
    });
    return document.closing;
  }

  private restoreEditing() {
    if (!this.disposed && this.current) this.current.view.setEditable(!this.current.remoteBlocked && !this.current.draft.source.readonly);
  }

  private resumeRemoteInspection() {
    if (!this.inspectAfterBusy) return;
    this.inspectAfterBusy = false;
    if (!this.disposed && this.current) queueMicrotask(() => { void this.inspectRemoteChange(); });
  }

  private publish(change: Partial<EditorState>) {
    if (this.disposed) return;
    this.state = { ...this.state, ...change };
    this.ports.stateChanged(this.state);
  }
}
