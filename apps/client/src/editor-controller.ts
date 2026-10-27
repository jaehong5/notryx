import type { M0CloseRequest, M0OpenedDocument, M0SavedCheckpoint, M0SaveRequest } from "../../../packages/protocol-types";
import { Draft } from "./draft";
import { fromBase64, toBase64 } from "./ipc";
import { openSource, type Source } from "./source";

export type EditorViewHandle = {
  show(): void;
  destroy(): void;
  setEditable(enabled: boolean): void;
  isComposing(): boolean;
};

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
};

export const initialEditorState: EditorState = { selected: "", status: "문서를 여는 중", error: "", notice: "" };

export type EditorPorts = {
  openDocument(id: string): Promise<M0OpenedDocument & { readonly?: boolean; local_draft?: boolean }>;
  saveDocument(request: M0SaveRequest): Promise<M0SavedCheckpoint>;
  closeDocument(request: M0CloseRequest): Promise<void>;
  exit(): Promise<void>;
  createEditor(options: EditorOptions): EditorViewHandle;
  stateChanged(state: EditorState): void;
  manualSync?(documentId: string, sessionId: string): Promise<void>;
  sourceChanged?(text: string, documentChanged: boolean): void;
  edited?(documentId: string, sessionId: string, generation: string): void;
  opened?(documentId: string, sessionId: string, generation: string, localDraft: boolean): void;
  compositionChanged?(documentId: string, sessionId: string, active: boolean): void;
};

type ActiveDocument = {
  id: string;
  draft: Draft;
  view: EditorViewHandle;
  closing?: Promise<void>;
};

export class EditorController {
  private current?: ActiveDocument;
  private timer?: ReturnType<typeof setTimeout>;
  private busy?: "opening" | "closing";
  private closeRequested = false;
  private disposed = false;
  private state = initialEditorState;

  constructor(private ports: EditorPorts) {}

  isCurrentSaved(sessionId: string, generation: string): boolean {
    const document = this.current;
    return !!document && document.draft.session === sessionId && !document.draft.dirty && document.draft.generation.toString() === generation;
  }

  isCurrentSession(sessionId: string): boolean {
    return this.current?.draft.session === sessionId;
  }

  recoverySnapshot(): { id: string; session: string; generation: string; bytes: Uint8Array } {
    const current = this.current;
    if (!current || this.busy || this.disposed) throw new Error("열린 문서를 다시 확인해 주세요.");
    if (current.view.isComposing()) throw new Error("한글 조합을 마친 뒤 다시 시도해 주세요.");
    return { id: current.id, session: current.draft.session, generation: current.draft.generation.toString(), bytes: current.draft.recoveryBytes() };
  }

  isCurrentSnapshot(session: string, generation: string): boolean {
    return this.current?.draft.session === session && this.current.draft.generation.toString() === generation;
  }

  async open(id: string): Promise<void> {
    if (this.disposed || this.busy || id === this.current?.id) return;
    if (this.current?.view.isComposing()) {
      this.publish({ error: "한글 조합을 마친 뒤 문서를 전환해 주세요." });
      return;
    }
    this.busy = "opening";
    this.current?.view.setEditable(false);
    const previous = this.current;
    let response: (M0OpenedDocument & { readonly?: boolean; local_draft?: boolean }) | undefined;
    let prepared: ActiveDocument | undefined;
    try {
      await this.flush();
      if (this.disposed) return;
      response = await this.ports.openDocument(id);
      if (this.disposed) {
        await this.ports.closeDocument({ document_id: id, edit_session_id: response.edit_session_id });
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
          if (this.disposed || this.busy || this.current !== prepared || source.readonly) return;
          draft.change(text);
          this.ports.edited?.(id, session, draft.generation.toString());
          this.ports.sourceChanged?.(text, false);
          this.publish({ status: "저장 대기 중" });
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
      if (previous) await this.release(previous);
      if (this.disposed) {
        view.destroy();
        await this.release(prepared);
        return;
      }
      this.current = prepared;
      previous?.view.destroy();
      this.ports.sourceChanged?.(source.text, true);
      this.publish({
        selected: id,
        notice: source.reason ?? (response.local_draft ? "보존된 기기 초안입니다. 서버 상태는 아직 확인하지 않았습니다." : source.normalizeOnEdit ? "수정하면 LF로 저장됩니다." : ""),
        status: source.readonly ? "읽기 전용, 원본 보존" : response.generation === "0" ? "수정 없음" : "이 기기에 저장됨",
        error: "",
      });
      try { this.ports.opened?.(id, session, response.generation, response.local_draft === true); }
      catch (cause) { this.publish({ error: String(cause) }); }
    } catch (cause) {
      prepared?.view.destroy();
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
    } finally {
      this.busy = undefined;
      this.restoreEditing();
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
        this.publish({ status: document.draft.source.readonly ? "읽기 전용, 원본 보존" : document.draft.generation === 0n ? "수정 없음" : "이 기기에 저장됨", error: "" });
      }
    } catch (cause) {
      if (this.current === document) this.publish({ status: "저장 오류, 입력 유지 중", error: String(cause) });
      throw cause;
    }
  }

  async save(): Promise<void> {
    const document = this.current;
    if (this.disposed || !document || this.busy) return;
    if (document.view.isComposing()) {
      this.publish({ error: "한글 조합을 마친 뒤 저장해 주세요." });
      return;
    }
    await this.flush();
    if (this.disposed || this.current !== document) return;
    try {
      await this.ports.manualSync?.(document.id, document.draft.session);
    } catch (cause) {
      if (!this.disposed && this.current === document) this.publish({ error: String(cause) });
      throw cause;
    }
  }

  async prepareForWorkspaceChange(): Promise<boolean> {
    if (this.disposed || this.busy) return false;
    if (this.current?.view.isComposing()) {
      this.publish({ error: "한글 조합을 마친 뒤 작업 공간을 전환해 주세요." });
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
      this.publish({ selected: "", status: "문서 없음", notice: "", error: "" });
      return true;
    } catch (cause) {
      this.publish({ error: String(cause) });
      return false;
    } finally {
      this.busy = undefined;
      this.restoreEditing();
    }
  }

  async close(): Promise<void> {
    if (this.disposed) return;
    if (this.busy) {
      if (this.busy === "opening") this.closeRequested = true;
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
      }
    }
  }

  async dispose(): Promise<void> {
    if (this.disposed) return;
    this.disposed = true;
    this.closeRequested = false;
    this.clearTimer();
    const document = this.current;
    this.current = undefined;
    if (document) {
      document.view.destroy();
      await document.draft.flush();
      await this.release(document);
    }
  }

  private schedule() {
    if (this.timer !== undefined) return;
    this.timer = setTimeout(() => {
      this.timer = undefined;
      void this.flush().catch(() => {});
    }, 250);
  }

  private clearTimer() {
    clearTimeout(this.timer);
    this.timer = undefined;
  }

  private release(document: ActiveDocument): Promise<void> {
    document.closing ??= this.ports.closeDocument({ document_id: document.id, edit_session_id: document.draft.session }).catch((cause) => {
      document.closing = undefined;
      throw cause;
    });
    return document.closing;
  }

  private restoreEditing() {
    if (!this.disposed && this.current) this.current.view.setEditable(!this.current.draft.source.readonly);
  }

  private publish(change: Partial<EditorState>) {
    if (this.disposed) return;
    this.state = { ...this.state, ...change };
    this.ports.stateChanged(this.state);
  }
}
