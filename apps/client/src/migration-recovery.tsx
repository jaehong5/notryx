import { invoke } from "@tauri-apps/api/core";
import { useEffect, useState, type ReactNode } from "react";
import type { ClientStartupStatus, MigrationDraft, MigrationInventory, MigrationOperation, MigrationSourceKind } from "../../../packages/protocol-types";

function object(value: unknown): Record<string, unknown> {
  if (value === null || typeof value !== "object" || Array.isArray(value)) throw new Error("잘못된 복구 응답입니다.");
  return value as Record<string, unknown>;
}

function text(value: unknown): string {
  if (typeof value !== "string" || value.length > 2048) throw new Error("잘못된 복구 응답입니다.");
  return value;
}

function nullableText(value: unknown): string | null {
  return value === null ? null : text(value);
}

function bool(value: unknown): boolean {
  if (typeof value !== "boolean") throw new Error("잘못된 복구 응답입니다.");
  return value;
}

function startup(value: unknown): ClientStartupStatus {
  const result = object(value);
  if (!['ready', 'recovery', 'store_locked', 'unavailable'].includes(text(result.mode))) throw new Error("잘못된 시작 상태입니다.");
  const mode = result.mode as ClientStartupStatus["mode"];
  const renderer_session_id = nullableText(result.renderer_session_id);
  if ((mode === "recovery") !== (renderer_session_id !== null)) throw new Error("잘못된 시작 상태입니다.");
  return { mode, renderer_session_id };
}

function binding(value: unknown): MigrationOperation["binding"] {
  const data = object(value);
  return { server_origin: text(data.server_origin), workspace_id: text(data.workspace_id), workspace_epoch: text(data.workspace_epoch), device_id: text(data.device_id), device_stream_id: text(data.device_stream_id) };
}

function inventory(value: unknown): MigrationInventory {
  const data = object(value);
  if (!Array.isArray(data.drafts) || data.drafts.length > 100 || !Array.isArray(data.operations) || data.operations.length > 100) throw new Error("잘못된 복구 목록입니다.");
  const drafts: MigrationDraft[] = data.drafts.map((value: unknown) => {
    const row = object(value);
    return { storage_id: text(row.storage_id), document_id: text(row.document_id), binding: row.binding === null ? null : binding(row.binding), registration_status: nullableText(row.registration_status), generation: nullableText(row.generation), has_checkpoint: bool(row.has_checkpoint), has_base: bool(row.has_base), metadata_error: bool(row.metadata_error) };
  });
  const operations: MigrationOperation[] = data.operations.map((value: unknown) => {
    const row = object(value);
    const kind = nullableText(row.kind);
    if (kind !== null && kind !== "content" && kind !== "restore" && kind !== "create" && kind !== "delete" && kind !== "move") throw new Error("잘못된 복구 작업 종류입니다.");
    return { operation_id: text(row.operation_id), document_id: text(row.document_id), binding: row.binding === null ? null : binding(row.binding), sequence: nullableText(row.sequence), generation: nullableText(row.generation), kind, expected_node_etag: nullableText(row.expected_node_etag), expected_meta_etag: nullableText(row.expected_meta_etag), status: text(row.status), terminal_kind: nullableText(row.terminal_kind), registration_status: nullableText(row.registration_status), metadata_error: bool(row.metadata_error) };
  });
  return { drafts, operations, next_draft_after_id: nullableText(data.next_draft_after_id), next_operation_after_id: nullableText(data.next_operation_after_id), draft_error: nullableText(data.draft_error), operation_error: nullableText(data.operation_error) };
}

async function loadInventory(owner: string, draft: string | null, operation: string | null, first: boolean): Promise<MigrationInventory> {
  return inventory(await invoke("migration_inventory", { request: { renderer_session_id: owner, draft_after_id: draft, operation_after_id: operation, include_drafts: first || draft !== null, include_operations: first || operation !== null } }));
}

function bindingLabel(bindingValue: MigrationDraft["binding"], metadataError: boolean): string {
  return bindingValue ? `${bindingValue.server_origin} / 작업 공간 ${bindingValue.workspace_id} / epoch ${bindingValue.workspace_epoch} / 기기 ${bindingValue.device_id} / stream ${bindingValue.device_stream_id}` : metadataError ? "등록 정보 확인 불가" : "이전 형식의 등록 정보 없음";
}

function MigrationRecovery({ owner }: { owner: string }) {
  const [data, setData] = useState<MigrationInventory | null>(null);
  const [message, setMessage] = useState("");
  const [selected, setSelected] = useState("");
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    let active = true;
    void loadInventory(owner, null, null, true).then((page) => { if (active) setData(page); }).catch(() => { if (active) setMessage("개별 항목 목록을 읽을 수 없습니다. 전체 저장소 사본을 먼저 내보내세요."); });
    return () => { active = false; };
  }, [owner]);

  async function more() {
    if (!data || busy) return;
    setBusy(true);
    try {
      const page = await loadInventory(owner, data.next_draft_after_id, data.next_operation_after_id, false);
      setData({ drafts: [...data.drafts, ...page.drafts], operations: [...data.operations, ...page.operations], next_draft_after_id: page.next_draft_after_id, next_operation_after_id: page.next_operation_after_id, draft_error: page.draft_error ?? data.draft_error, operation_error: page.operation_error ?? data.operation_error });
    } catch { setMessage("다음 복구 목록을 읽지 못했습니다. 전체 저장소 사본을 먼저 내보내세요."); }
    finally { setBusy(false); }
  }

  async function act(kind: MigrationSourceKind, id: string | null, action: "view" | "export") {
    if (busy) return;
    setBusy(true);
    setMessage("");
    try {
      const request = { renderer_session_id: owner, kind, id };
      if (action === "export") {
        const result: unknown = await invoke("migration_export", { request });
        if (typeof result !== "boolean") throw new Error("잘못된 복구 응답입니다.");
        if (result) setMessage("선택한 새 파일로 내보냈습니다.");
      } else {
        const result = object(await invoke("migration_source", { request }));
        if (typeof result.bytes_base64 !== "string" || result.bytes_base64.length > 7_000_000) throw new Error("잘못된 원문 응답입니다.");
        const binary = atob(result.bytes_base64);
        const bytes = Uint8Array.from(binary, (character) => character.charCodeAt(0));
        try { setSelected(new TextDecoder("utf-8", { fatal: true }).decode(bytes)); }
        catch { setSelected("텍스트로 표시할 수 없는 원문입니다. 파일로 내보내면 bytes가 유지됩니다."); }
      }
    } catch (error) {
      setMessage(String(error).includes("recovery_export_exists") ? "이미 있는 파일은 변경하지 않았습니다. 다른 이름을 선택하세요." : "복구 작업에 실패했습니다. 기존 저장소는 유지됩니다.");
    } finally { setBusy(false); }
  }

  return <main data-testid="migration-recovery">
    <h1>로컬 저장소 복구</h1>
    <p>저장소를 업데이트하지 못했습니다. 기존 자료를 유지하고 편집과 서버 전송을 중지했습니다. 전체 저장소 사본에는 초안, 기준 원문과 전송 기록이 포함됩니다. 작업 데이터를 따로 내보내면 요청 본문만 저장되므로 조건과 등록 정보를 함께 보존하려면 전체 저장소 사본을 내보내세요.</p>
    <p>새 파일 이름을 선택하세요. 기존 파일은 덮어쓰지 않습니다.</p>
    <button data-testid="migration-export-store" disabled={busy} onClick={() => void act("store", null, "export")}>전체 저장소 내보내기</button>
    {message && <p role="status">{message}</p>}
    {data && <>
      <h2>초안과 기준 원문</h2>
      {data.draft_error && <p role="status">초안 목록을 읽지 못했습니다. 전체 저장소 사본을 보존해 주세요.</p>}
      {data.drafts.map((draft) => <section key={draft.storage_id}>
        <p>{draft.document_id} / generation {draft.generation ?? "없음 또는 확인 불가"} / {bindingLabel(draft.binding, draft.metadata_error)} / {draft.registration_status ?? "등록 상태 없음"} {draft.metadata_error && "/ 일부 정보 확인 불가"}</p>
        {draft.has_checkpoint && <><button data-testid="migration-view-draft" disabled={busy} onClick={() => void act("draft", draft.storage_id, "view")}>초안 보기</button>
        <button data-testid="migration-export-draft" disabled={busy} onClick={() => void act("draft", draft.storage_id, "export")}>초안 내보내기</button></>}
        {draft.has_base && <><button data-testid="migration-view-base" disabled={busy} onClick={() => void act("base", draft.storage_id, "view")}>기준 원문 보기</button>
        <button data-testid="migration-export-base" disabled={busy} onClick={() => void act("base", draft.storage_id, "export")}>기준 원문 내보내기</button></>}
      </section>)}
      <h2>전송 기록</h2>
      {data.operation_error && <p role="status">전송 기록 목록을 읽지 못했습니다. 전체 저장소 사본을 보존해 주세요.</p>}
      {data.operations.map((operation) => <section key={operation.operation_id}>
        <p>{operation.document_id} / 작업 {operation.operation_id} / 순번 {operation.sequence ?? "확인 불가"} / {operation.kind === "content" ? "문서 편집" : operation.kind === "restore" ? "복원 요청" : operation.kind === "create" ? "생성 요청" : operation.kind === "delete" ? "삭제 요청" : operation.kind === "move" ? "이동 요청" : "종류 확인 불가"} / 상태 {operation.status} / 종료 결과 {operation.terminal_kind ?? "없음 또는 구형 형식"} / 등록 {operation.registration_status ?? "확인 불가"} / {bindingLabel(operation.binding, operation.metadata_error)} {operation.metadata_error && "/ 일부 정보 확인 불가"}{operation.kind === "delete" && <> / 확인 ETag {operation.expected_node_etag ?? "확인 불가"}</>}{operation.kind === "move" && <> / 구조 ETag {operation.expected_meta_etag ?? "확인 불가"}</>}</p>
        {operation.kind !== "delete" && <><button data-testid="migration-view-outbox" disabled={busy} onClick={() => void act("outbox", operation.operation_id, "view")}>{operation.kind === "content" ? "전송 원문 보기" : operation.kind === "restore" ? "복원 요청 보기" : "작업 데이터 보기"}</button>
        <button data-testid="migration-export-outbox" disabled={busy} onClick={() => void act("outbox", operation.operation_id, "export")}>{operation.kind === "content" ? "전송 원문 내보내기" : operation.kind === "restore" ? "복원 요청 내보내기" : "작업 데이터 내보내기"}</button></>}
      </section>)}
      {(data.next_draft_after_id !== null || data.next_operation_after_id !== null) && <button disabled={busy} onClick={() => void more()}>복구 목록 더 보기</button>}
    </>}
    {selected && <pre aria-label="선택한 원문">{selected}</pre>}
  </main>;
}

export function StartupGate({ ready }: { ready: () => ReactNode }) {
  const [status, setStatus] = useState<ClientStartupStatus | null>(null);
  const [error, setError] = useState(false);
  useEffect(() => {
    let active = true;
    void invoke("client_startup_status").then(startup).then((value) => { if (active) setStatus(value); }).catch(() => { if (active) setError(true); });
    return () => { active = false; };
  }, []);
  if (error) return <main><h1>시작 상태 확인 실패</h1><p>로컬 저장소를 변경하지 않았습니다. 앱을 닫고 상태를 확인하세요.</p></main>;
  if (!status) return <main>로컬 저장소를 확인하고 있습니다.</main>;
  if (status.mode === "ready") return ready();
  if (status.mode === "recovery" && status.renderer_session_id) return <MigrationRecovery owner={status.renderer_session_id} />;
  return <main><h1>로컬 저장소를 열 수 없습니다</h1><p>{status.mode === "store_locked" ? "다른 Notryx 인스턴스가 이 저장소를 사용하고 있습니다." : "저장소를 읽을 수 없습니다. 기존 파일을 보존하고 상태를 확인하세요."}</p></main>;
}
