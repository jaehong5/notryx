import { useEffect, useRef, useState } from "react";
import type { WorkspaceFrozenContentInspection, WorkspaceOutboxStatus, WorkspaceRegistration } from "../../../packages/protocol-types";

export type FrozenContentReviewItem = {
  old_binding: WorkspaceRegistration;
  operation_id: string;
  operation_seq: string;
  document_id: string;
  generation: string;
  expected_revision?: string | null;
  status: "queued" | "sent" | "abandoning";
  abandon_state?: "none" | "requested" | "sent";
};

export type FrozenContentReviewSnapshot = FrozenContentReviewItem & {
  binding: WorkspaceRegistration;
  expected_revision: string | null;
  local_generation?: string;
  outcome_unknown: boolean;
  base_revision: string;
  remote_revision: string;
  operation_bytes: Uint8Array;
  base_bytes: Uint8Array;
  local_bytes: Uint8Array;
  remote_bytes: Uint8Array;
  inspected?: WorkspaceFrozenContentInspection;
  current_target?: FrozenContentCurrentTarget;
  cross_epoch_inspected?: unknown;
  target_key?: string;
};

export type FrozenContentCurrentTarget = { node_id: string; parent_id: string; name: string; path: string };

export type FrozenContentLocalReviewSnapshot = FrozenContentReviewItem & {
  binding: WorkspaceRegistration;
  dependency: string | null;
  expected_revision: string | null;
  local_generation?: string;
  outcome_unknown: boolean;
  base_revision: string;
  operation_bytes: Uint8Array;
  base_bytes: Uint8Array;
  local_bytes: Uint8Array;
  remote_status: "deleted" | "too_large" | "unknown";
  remote_index_error: boolean;
  evidence_key?: string;
  inspected?: unknown;
};

export type FrozenContentReviewPorts = {
  list(binding: WorkspaceRegistration, after: string | null): Promise<{ binding: WorkspaceRegistration; entries: FrozenContentReviewItem[]; next_after_id: string | null }>;
  load(binding: WorkspaceRegistration, oldBinding: WorkspaceRegistration, operationId: string): Promise<FrozenContentReviewSnapshot>;
  apply?(snapshot: FrozenContentReviewSnapshot): Promise<{ binding: WorkspaceRegistration; operation_id: string }>;
  loadCrossEpoch?(binding: WorkspaceRegistration, oldBinding: WorkspaceRegistration, operationId: string): Promise<FrozenContentReviewSnapshot>;
  applyCrossEpoch?(snapshot: FrozenContentReviewSnapshot, confirmed: true): Promise<{ binding: WorkspaceRegistration; operation_id: string }>;
  dismiss?(snapshot: FrozenContentReviewSnapshot): Promise<{ binding: WorkspaceRegistration; old_operation_id: string }>;
  loadLocal?(binding: WorkspaceRegistration, oldBinding: WorkspaceRegistration, operationId: string): Promise<FrozenContentLocalReviewSnapshot>;
  dismissLocal?(snapshot: FrozenContentLocalReviewSnapshot): Promise<{ binding: WorkspaceRegistration; old_operation_id: string }>;
  status?(binding: WorkspaceRegistration, oldBinding: WorkspaceRegistration, documentId: string, operationId: string): Promise<FrozenContentRecordedDecision | null>;
};

export type FrozenContentRecordedDecision = {
  binding: WorkspaceRegistration;
  old_binding: WorkspaceRegistration;
  document_id: string;
  old_operation_id: string;
  operation_id: string;
  operation_seq: string;
  status: WorkspaceOutboxStatus;
  approved_generation: string;
  approved_hash: string;
} | {
  kind: "dismissed";
  binding: WorkspaceRegistration;
  old_binding: WorkspaceRegistration;
  document_id: string;
  old_operation_id: string;
  operation_seq: string;
  old_status: "queued" | "sent" | "abandoning";
  abandon_state: "none" | "requested" | "sent";
};

type SourceView = { text: string; bom: boolean; lineEnding: string } | null;
type Detail = { snapshot: FrozenContentReviewSnapshot | FrozenContentLocalReviewSnapshot; payload: SourceView; base: SourceView; draft: SourceView; remote: SourceView };

function bindingKey(binding: WorkspaceRegistration): string {
  return JSON.stringify([binding.server_origin, binding.workspace_id, binding.workspace_epoch, binding.device_id, binding.device_stream_id]);
}

function sourceView(bytes: Uint8Array, maximum = 5 * 1024 * 1024): SourceView {
  if (!ArrayBuffer.isView(bytes) || Object.prototype.toString.call(bytes) !== "[object Uint8Array]" || bytes.byteLength > maximum) return null;
  try {
    const text = new TextDecoder("utf-8", { fatal: true }).decode(bytes);
    let lf = false, crlf = false, cr = false;
    for (let index = 0; index < text.length; index++) {
      if (text[index] === "\r") {
        if (text[index + 1] === "\n") { crlf = true; index++; } else cr = true;
      } else if (text[index] === "\n") lf = true;
    }
    const endings = [lf && "LF", crlf && "CRLF", cr && "단독 CR"].filter(Boolean);
    return { text, bom: bytes.length >= 3 && bytes[0] === 0xef && bytes[1] === 0xbb && bytes[2] === 0xbf,
      lineEnding: endings.length === 0 ? "없음" : endings.length === 1 ? String(endings[0]) : `혼합 (${endings.join(", ")})` };
  } catch { return null; }
}

function SourceSection({ id, label, detail, source }: { id: string; label: string; detail: string; source: SourceView }) {
  return <div data-testid={id}>
    <p>{label} ({detail}{source ? `, ${source.bom ? "BOM 포함, " : ""}줄바꿈: ${source.lineEnding}` : ""})</p>
    {source ? <textarea readOnly aria-label={label} value={source.text} /> : <p>UTF-8 원문이 아니거나 표시 크기 제한을 넘어 텍스트로 표시할 수 없습니다. 보존된 자료는 변경하지 않았습니다.</p>}
  </div>;
}

export function FrozenContentReview({ binding, ports }: { binding: WorkspaceRegistration; ports: FrozenContentReviewPorts }) {
  const key = bindingKey(binding);
  const activeKey = useRef(key); activeKey.current = key;
  const scope = useRef(0), listRequest = useRef(0), detailRequest = useRef(0), applyRequest = useRef(0);
  const applying = useRef(false);
  const [dataKey, setDataKey] = useState("");
  const [items, setItems] = useState<FrozenContentReviewItem[]>([]);
  const [after, setAfter] = useState<string | null>(null);
  const [detail, setDetail] = useState<Detail | null>(null);
  const [recorded, setRecorded] = useState<FrozenContentRecordedDecision | null>(null);
  const [message, setMessage] = useState("");
  const [selectedDraft, setSelectedDraft] = useState(false);
  const [confirmedTarget, setConfirmedTarget] = useState(false);
  const [selectedDismiss, setSelectedDismiss] = useState(false);
  const [applyBusy, setApplyBusy] = useState(false);
  const [applyFinished, setApplyFinished] = useState(false);
  const current = (generation: number) => activeKey.current === key && scope.current === generation;

  async function loadItems(cursor: string | null, generation: number) {
    if (applying.current) return;
    const attempt = ++listRequest.current;
    detailRequest.current++; applyRequest.current++; applying.current = false;
    setDetail(null); setRecorded(null); setMessage(""); setSelectedDraft(false); setConfirmedTarget(false); setSelectedDismiss(false); setApplyBusy(false); setApplyFinished(false);
    try {
      const page = await ports.list(binding, cursor);
      if (!current(generation) || listRequest.current !== attempt) return;
      if (bindingKey(page.binding) !== key) throw new Error("stale_binding");
      setItems(page.entries.filter((item) => item.old_binding.workspace_id === binding.workspace_id && bindingKey(item.old_binding) !== key));
      setAfter(page.next_after_id); setDataKey(key);
    } catch {
      if (current(generation) && listRequest.current === attempt) { setItems([]); setAfter(null); setDataKey(key); setMessage("이전 본문 요청을 불러오지 못했습니다. 보존된 초안은 유지됩니다."); }
    }
  }

  useEffect(() => {
    const generation = ++scope.current;
    applying.current = false;
    setDataKey(""); setItems([]); setAfter(null); setDetail(null); setRecorded(null); setMessage("");
    void loadItems(null, generation);
    return () => { scope.current++; detailRequest.current++; applyRequest.current++; applying.current = false; };
  }, [key, ports]);

  async function inspect(item: FrozenContentReviewItem) {
    if (applying.current || item.old_binding.workspace_id !== binding.workspace_id || bindingKey(item.old_binding) === key) return;
    const generation = scope.current, attempt = ++detailRequest.current;
    applyRequest.current++; applying.current = false;
    setDetail(null); setRecorded(null); setSelectedDraft(false); setConfirmedTarget(false); setSelectedDismiss(false); setApplyBusy(false); setApplyFinished(false); setMessage("비교 자료를 불러오는 중입니다.");
    try {
      if (ports.status) {
        const decision = await ports.status(binding, item.old_binding, item.document_id, item.operation_id);
        if (!current(generation) || detailRequest.current !== attempt) return;
        if (decision) {
          if (bindingKey(decision.binding) !== key || bindingKey(decision.old_binding) !== bindingKey(item.old_binding) || decision.document_id !== item.document_id || decision.old_operation_id !== item.operation_id ||
            ("kind" in decision && (decision.operation_seq !== item.operation_seq || decision.old_status !== item.status || (item.abandon_state !== undefined && decision.abandon_state !== item.abandon_state)))) throw new Error("stale_decision");
          setRecorded(decision); setMessage(""); return;
        }
      }
      let result: FrozenContentReviewSnapshot | FrozenContentLocalReviewSnapshot;
      try {
        result = item.old_binding.workspace_epoch !== binding.workspace_epoch && ports.loadCrossEpoch
          ? await ports.loadCrossEpoch(binding, item.old_binding, item.operation_id)
          : await ports.load(binding, item.old_binding, item.operation_id);
      } catch (error) {
        if (!ports.loadLocal || !current(generation) || detailRequest.current !== attempt) throw error;
        result = await ports.loadLocal(binding, item.old_binding, item.operation_id);
      }
      if (!current(generation) || detailRequest.current !== attempt) return;
      if (bindingKey(result.binding) !== key || bindingKey(result.old_binding) !== bindingKey(item.old_binding) || result.operation_id !== item.operation_id || result.document_id !== item.document_id || result.operation_seq !== item.operation_seq || result.generation !== item.generation || result.status !== item.status || (item.abandon_state !== undefined && result.abandon_state !== item.abandon_state) || (item.expected_revision !== undefined && result.expected_revision !== item.expected_revision) || result.outcome_unknown !== (item.status === "sent")) throw new Error("stale_inspection");
      if ("remote_status" in result && ((result.remote_status !== "deleted" && result.remote_status !== "too_large" && result.remote_status !== "unknown") || typeof result.remote_index_error !== "boolean" || (result.remote_index_error && result.remote_status !== "unknown"))) throw new Error("invalid_remote_state");
      setDetail({ snapshot: result, payload: sourceView(result.operation_bytes), base: sourceView(result.base_bytes), draft: sourceView(result.local_bytes), remote: "remote_status" in result ? null : sourceView(result.remote_bytes, 25 * 1024 * 1024) });
      setMessage("");
    } catch {
      if (current(generation) && detailRequest.current === attempt) setMessage("비교 자료를 불러오지 못했습니다. 현재 연결과 동기화 상태를 확인하세요. 보존된 초안은 유지됩니다.");
    }
  }

  const visible = dataKey === key ? detail : null;
  const visibleDecision = dataKey === key ? recorded : null;
  const stateLabel: Record<WorkspaceOutboxStatus, string> = {
    queued: "전송 대기", sent: "전송 결과 미확정", committed: "서버 반영",
    conflict: "충돌", abandoning: "취소 결과 미확정", abandoned: "취소됨", failed: "실패",
  };
  const crossEpoch = !!visible && visible.snapshot.old_binding.workspace_epoch !== binding.workspace_epoch;
  const target = visible && !("remote_status" in visible.snapshot) ? visible.snapshot.current_target : undefined;
  const targetValid = !!visible && !!target && target.node_id === visible.snapshot.document_id && !!target.parent_id && !!target.name && !!target.path &&
    !target.name.includes("/") && target.name !== "." && target.name !== ".." && target.path.startsWith("/") &&
    target.path.slice(1).split("/").every((part) => !!part && part !== "." && part !== "..") && target.path.split("/").at(-1) === target.name;
  const unsupported = visible && ("remote_status" in visible.snapshot
    ? "서버 본문을 확인하지 못해 적용을 보류합니다. 보존된 자료는 유지됩니다."
    : crossEpoch && (!targetValid || !ports.applyCrossEpoch)
    ? "다른 epoch의 현재 적용 대상을 확인하지 못해 적용을 보류합니다. 보존된 자료는 유지됩니다."
    : visible.snapshot.status === "abandoning"
      ? "취소 결과가 미확정이므로 적용을 보류합니다. 보존된 자료는 유지됩니다."
      : visible.snapshot.expected_revision !== visible.snapshot.base_revision || !visible.snapshot.local_generation
        ? "이 작업의 적용 순서를 확인할 수 없어 적용을 보류합니다. 보존된 자료는 유지됩니다."
        : !visible.payload || !visible.base || !visible.draft || !visible.remote || visible.snapshot.remote_bytes.byteLength > 5 * 1024 * 1024
          ? "편집 가능한 원문이 아니므로 적용을 보류합니다. 보존된 자료는 유지됩니다."
          : "");

  async function applyDraft() {
    if (!visible || "remote_status" in visible.snapshot || unsupported || !selectedDraft || (crossEpoch && !confirmedTarget) || (crossEpoch ? !ports.applyCrossEpoch : !ports.apply) || applying.current || applyFinished) return;
    const generation = scope.current, detailAttempt = detailRequest.current, attempt = ++applyRequest.current;
    applying.current = true; setApplyBusy(true); setMessage("");
    try {
      const result = crossEpoch ? await ports.applyCrossEpoch!(visible.snapshot, true) : await ports.apply!(visible.snapshot);
      if (!current(generation) || detailRequest.current !== detailAttempt || applyRequest.current !== attempt) return;
      if (bindingKey(result.binding) !== key || !result.operation_id) throw new Error("stale_binding");
      setApplyFinished(true); setMessage("선택한 초안의 복구 요청이 기록됐습니다. 전송 상태는 동기화 상태에서 확인해 주세요.");
    } catch {
      if (current(generation) && detailRequest.current === detailAttempt && applyRequest.current === attempt) {
        setMessage("요청 결과를 확인하지 못했습니다. 같은 선택으로 다시 확인하거나 보존된 자료를 다시 대조해 주세요.");
      }
    } finally {
      if (current(generation) && detailRequest.current === detailAttempt && applyRequest.current === attempt) {
        applying.current = false; setApplyBusy(false);
      }
    }
  }

  async function dismissOperation() {
    if (!visible || !visible.payload || !visible.base || !visible.draft || ("remote_status" in visible.snapshot && visible.snapshot.remote_index_error) || !("remote_status" in visible.snapshot ? ports.dismissLocal : ports.dismiss) || !selectedDismiss || applying.current || applyFinished) return;
    const generation = scope.current, detailAttempt = detailRequest.current, attempt = ++applyRequest.current;
    applying.current = true; setApplyBusy(true); setMessage("");
    try {
      const result = "remote_status" in visible.snapshot ? await ports.dismissLocal!(visible.snapshot) : await ports.dismiss!(visible.snapshot);
      if (!current(generation) || detailRequest.current !== detailAttempt || applyRequest.current !== attempt) return;
      if (bindingKey(result.binding) !== key || result.old_operation_id !== visible.snapshot.operation_id) throw new Error("stale_binding");
      setApplyFinished(true); setMessage("선택한 이전 작업의 재발급 포기가 기록됐습니다. 보존된 초안과 다른 작업은 별도로 검토해야 합니다.");
    } catch {
      if (current(generation) && detailRequest.current === detailAttempt && applyRequest.current === attempt) {
        setMessage("포기 결과를 확인하지 못했습니다. 같은 선택으로 다시 확인하거나 보존된 자료를 다시 대조해 주세요.");
      }
    } finally {
      if (current(generation) && detailRequest.current === detailAttempt && applyRequest.current === attempt) {
        applying.current = false; setApplyBusy(false);
      }
    }
  }

  return <section aria-label="이전 본문 요청 대조" className="frozen-content-review">
    <h2>이전 본문 요청 대조</h2>
    <p>보존된 작업과 초안, 조회 당시 서버 본문을 읽기 전용으로 표시합니다. 현재 본문만으로 이전 요청의 처리 결과를 판단할 수 없습니다.</p>
    {dataKey === key && items.map((item) => <button key={item.operation_id} data-review-operation={item.operation_id} disabled={applyBusy} onClick={() => void inspect(item)}>
      {item.document_id} / 순번 {item.operation_seq} / {item.status === "sent" ? "전송 결과 미확정" : item.status === "abandoning" ? "취소 결과 미확정" : "전송 대기"}
    </button>)}
    {dataKey === key && after !== null && <button disabled={applyBusy} onClick={() => void loadItems(after, scope.current)}>다음 이전 요청</button>}
    {dataKey === key && <button disabled={applyBusy} onClick={() => void loadItems(null, scope.current)}>처음 이전 요청</button>}
    {message && dataKey === key && <p role="status">{message}</p>}
    {visibleDecision && "kind" in visibleDecision && visibleDecision.kind === "dismissed" && <div data-testid="frozen-content-recorded-dismissal">
      <p>이전 작업 {visibleDecision.old_operation_id}, 순번 {visibleDecision.operation_seq}의 재발급 포기가 기록됐습니다.</p>
      {visibleDecision.old_status === "sent" && <p>이전 요청의 전송 결과는 미확정입니다.</p>}
      {visibleDecision.old_status === "abandoning" && <p>이전 취소 요청의 결과는 미확정입니다.</p>}
      <p>보존된 초안과 다른 작업은 별도로 검토해야 합니다.</p>
    </div>}
    {visibleDecision && !("kind" in visibleDecision) && <div data-testid="frozen-content-recorded-decision">
      <p>이전에 선택한 마지막 저장 초안의 복구 요청이 기록됐습니다.</p>
      <p>새 작업 {visibleDecision.operation_id}, 순번 {visibleDecision.operation_seq}, 상태: {stateLabel[visibleDecision.status]}</p>
      <p>선택 당시 초안 generation {visibleDecision.approved_generation}, hash {visibleDecision.approved_hash}</p>
      <p>이전 요청의 처리 결과는 알 수 없습니다.</p>
    </div>}
    {visible && <div>
      <p>이전 서버: {visible.snapshot.old_binding.server_origin}, 이전 epoch: {visible.snapshot.old_binding.workspace_epoch}</p>
      <p>현재 서버: {binding.server_origin}, 현재 epoch: {binding.workspace_epoch}</p>
      {visible.snapshot.old_binding.workspace_epoch !== binding.workspace_epoch && <p>epoch가 다릅니다. 문서 ID가 같아도 과거와 현재 상태의 관계는 확인해야 합니다.</p>}
      {crossEpoch && targetValid && target && <p>현재 적용 대상: {target.path} (node ID {target.node_id}, 상위 node ID {target.parent_id}, 이름 {target.name})</p>}
      {visible.snapshot.outcome_unknown && <p>전송 후 처리 결과는 미확정입니다. 서버 본문이 같아도 이전 요청의 결과는 알 수 없습니다.</p>}
      {visible.snapshot.status === "abandoning" && <p>취소 결과는 미확정입니다. {visible.snapshot.abandon_state === "requested" ? "취소 요청이 대기 중입니다." : "취소 요청의 전송 또는 처리 결과가 미확정입니다."}</p>}
      {"remote_status" in visible.snapshot && <p>{visible.snapshot.dependency === null ? "선행 작업 의존성 없음" : `선행 작업 의존성: ${visible.snapshot.dependency}`}</p>}
      <SourceSection id="frozen-content-payload" label="이전 요청 원문" detail={`generation ${visible.snapshot.generation}, 요청 조건 revision ${visible.snapshot.expected_revision ?? "미확정"}`} source={visible.payload} />
      <SourceSection id="frozen-content-base" label="보존된 초안의 기준 원문" detail={`기준 revision ${visible.snapshot.base_revision}`} source={visible.base} />
      <SourceSection id="frozen-content-draft" label="마지막으로 저장된 초안" detail={`${visible.snapshot.local_generation ? `generation ${visible.snapshot.local_generation}, ` : ""}요청 이후에 수정됐을 수 있음`} source={visible.draft} />
      {"remote_status" in visible.snapshot ? <div data-testid="frozen-content-remote">
        <p>{visible.snapshot.remote_status === "deleted" ? "현재 서버 문서가 삭제된 것으로 확인됐습니다." : visible.snapshot.remote_status === "too_large" ? "현재 서버 문서가 25 MiB를 초과해 본문을 열 수 없습니다." : "현재 서버 문서의 상태를 확인하지 못했습니다."}</p>
        {visible.snapshot.remote_index_error && <p>동기화 인덱스 확인에 실패했습니다. 재동기화가 필요합니다.</p>}
        <p>서버 본문을 확인하지 못했습니다. 이전 요청의 결과는 알 수 없습니다.</p>
      </div> : <SourceSection id="frozen-content-remote" label="조회 당시 서버 본문" detail={`revision ${visible.snapshot.remote_revision}`} source={visible.remote} />}
      {(crossEpoch ? ports.applyCrossEpoch || ports.apply : ports.apply) && (unsupported ? <p>{unsupported}</p> : <div>
        <label><input data-testid="frozen-content-select-draft" type="checkbox" checked={selectedDraft} disabled={applyBusy || applyFinished} onChange={(event) => { setSelectedDraft(event.currentTarget.checked); if (event.currentTarget.checked) setSelectedDismiss(false); }} /> 마지막으로 저장된 초안을 새 작업의 원문으로 선택합니다.</label>
        {crossEpoch && <label><input data-testid="frozen-content-confirm-target" type="checkbox" checked={confirmedTarget} disabled={applyBusy || applyFinished} onChange={(event) => setConfirmedTarget(event.currentTarget.checked)} /> 현재 표시된 문서를 적용 대상으로 확인합니다. ID가 같아도 이전 문서와의 관계는 알 수 없습니다.</label>}
        <p>이전 요청 원문 선택은 아직 지원하지 않습니다. 이전 요청의 처리 결과는 확인되지 않습니다.</p>
        <button data-testid="frozen-content-apply" disabled={!selectedDraft || (crossEpoch && !confirmedTarget) || applyBusy || applyFinished} onClick={() => void applyDraft()}>선택한 초안으로 새 전송 대기 작업 만들기</button>
      </div>)}
      {("remote_status" in visible.snapshot ? ports.dismissLocal : ports.dismiss) && <div>
        {(!visible.payload || !visible.base || !visible.draft) && <p>보존된 원문을 표시할 수 없어 재발급 포기를 보류합니다.</p>}
        <label><input data-testid="frozen-content-select-dismiss" type="checkbox" checked={selectedDismiss} disabled={!visible.payload || !visible.base || !visible.draft || ("remote_status" in visible.snapshot && visible.snapshot.remote_index_error) || applyBusy || applyFinished} onChange={(event) => { setSelectedDismiss(event.currentTarget.checked); if (event.currentTarget.checked) setSelectedDraft(false); }} /> 이 이전 작업을 새 등록에서 재발급하지 않습니다.</label>
        <p>이전 원문과 초안은 보존됩니다. 다른 작업과 초안은 별도로 검토해야 합니다.</p>
        <button data-testid="frozen-content-dismiss" disabled={!visible.payload || !visible.base || !visible.draft || ("remote_status" in visible.snapshot && visible.snapshot.remote_index_error) || !selectedDismiss || applyBusy || applyFinished} onClick={() => void dismissOperation()}>이전 작업 재발급 포기 확인</button>
      </div>}
    </div>}
  </section>;
}
