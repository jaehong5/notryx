import { useEffect, useRef, useState } from "react";
import type { NodeState, WorkspaceDeleteEntry, WorkspaceRegistration } from "../../../packages/protocol-types";

export type DeleteDisplay = Pick<WorkspaceDeleteEntry, "operation_id" | "operation_seq" | "node_id" | "expected_node_etag" | "status"> & { reason: string | null };

export type DeletePanelPorts = {
  queue(binding: WorkspaceRegistration, request: { node_id: string; expected_node_etag: string }): Promise<DeleteDisplay>;
  status(binding: WorkspaceRegistration, after: string): Promise<{ entries: DeleteDisplay[]; next_after_seq: string | null }>;
  retry(binding: WorkspaceRegistration, operationId: string): Promise<void>;
  cancel(binding: WorkspaceRegistration, operationId: string): Promise<void>;
};

export type DeletePanelProps = {
  binding: WorkspaceRegistration;
  node: NodeState | null;
  displayName?: string;
  ports: DeletePanelPorts;
  revision: number;
  permissionDeniedOperationId?: string | null;
  onClose(): void;
  onWorkReady(): void;
  onRecheck?(operationId: string): void;
};

function statusText(entry: DeleteDisplay, denied: boolean): string {
  if (denied && (entry.status === "queued" || entry.status === "sent")) return "삭제 권한이 없습니다.";
  if (entry.status === "queued") return "전송 대기";
  if (entry.status === "sent") return "서버 결과 확인 중";
  if (entry.status === "abandoning") return "취소 처리 중";
  if (entry.status === "abandoned") return "취소 완료";
  if (entry.status === "committed") return "휴지통으로 이동 완료";
  switch (entry.reason) {
    case "recovery_conflict": return "삭제의 서버 결과를 대조해야 합니다. 문서 상태와 보존 자료를 확인하세요.";
    case "node_precondition_failed": case "precondition_failed": case "node_changed": return "문서 상태가 변경됐습니다. 목록을 갱신한 뒤 다시 확인하세요.";
    case "node_deleted": case "not_found": return "문서가 없거나 이미 삭제됐습니다. 목록을 다시 확인하세요.";
    default: return "삭제 결과를 확인하고 다시 시도해 주세요.";
  }
}

function queueError(cause: unknown): string {
  const error = String(cause);
  if (error.includes("OpenEditSessions")) return "열린 문서를 닫거나 다른 문서로 전환한 뒤 다시 확인하세요.";
  if (error.includes("OutboxBlocked")) return "이 문서의 초안 또는 전송 대기 작업을 확인한 뒤 다시 시도하세요.";
  if (error.includes("ReconciliationRequired")) return "이전 등록의 보존 자료를 확인한 뒤 다시 시도하세요.";
  if (error.includes("InvalidRequest")) return "문서 상태가 변경됐습니다. 목록을 갱신한 뒤 다시 확인하세요.";
  return "삭제 요청을 저장하지 못했습니다. 문서와 등록 상태를 확인하세요.";
}

function knownQueueRejection(cause: unknown): boolean {
  const error = String(cause);
  return ["OpenEditSessions", "OutboxBlocked", "ReconciliationRequired", "InvalidRequest"].some((code) => error.includes(code));
}

export function DeletePanel({ binding, node, displayName, ports, revision, permissionDeniedOperationId, onClose, onWorkReady, onRecheck }: DeletePanelProps) {
  const bindingKey = JSON.stringify(binding);
  const [target] = useState(() => ({ bindingKey, nodeId: node?.node_id ?? null, name: displayName ?? node?.name ?? "", nodeEtag: node?.node_etag ?? null, kind: node?.kind, deleted: node?.deleted }));
  const activeKey = useRef(bindingKey);
  activeKey.current = bindingKey;
  const scope = useRef(0);
  const activeAction = useRef<number | null>(null);
  const statusRequest = useRef(0);
  const [entries, setEntries] = useState<DeleteDisplay[]>([]);
  const [nextEntries, setNextEntries] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [confirmed, setConfirmed] = useState(node === null);
  const [uncertain, setUncertain] = useState(false);
  const [message, setMessage] = useState("");

  function current(generation: number) {
    return scope.current === generation && activeKey.current === bindingKey && bindingKey === target.bindingKey;
  }

  async function loadStatus(after: string, generation: number, preserveMessage = false) {
    const attempt = ++statusRequest.current;
    try {
      const page = await ports.status(binding, after);
      if (!current(generation) || statusRequest.current !== attempt) return;
      setEntries(page.entries);
      setNextEntries(page.next_after_seq);
    } catch {
      if (!preserveMessage && current(generation) && statusRequest.current === attempt) setMessage("삭제 요청 상태를 불러오지 못했습니다.");
    }
  }

  useEffect(() => {
    const generation = ++scope.current;
    setEntries([]); setNextEntries(null); setBusy(false); setMessage("");
    if (bindingKey === target.bindingKey) void loadStatus("0", generation);
    return () => { scope.current++; };
  }, [bindingKey, ports]);
  useEffect(() => { if (scope.current && bindingKey === target.bindingKey) void loadStatus("0", scope.current); }, [revision]);

  async function submit() {
    if (busy || confirmed || activeAction.current === scope.current || bindingKey !== target.bindingKey || !target.nodeId || !target.nodeEtag || target.kind !== "file" || target.deleted) return;
    const generation = scope.current;
    activeAction.current = generation;
    setBusy(true); setMessage("");
    try {
      const operation = await ports.queue(binding, { node_id: target.nodeId, expected_node_etag: target.nodeEtag });
      if (!current(generation)) return;
      statusRequest.current++;
      setEntries((previous) => [operation, ...previous.filter((item) => item.operation_id !== operation.operation_id)]);
      setConfirmed(true);
      setMessage("삭제 요청을 이 기기에 저장했습니다. 전송 대기 중입니다.");
      onWorkReady();
    } catch (cause) {
      if (current(generation)) {
        if (knownQueueRejection(cause)) setMessage(queueError(cause));
        else {
          setConfirmed(true);
          setUncertain(true);
          setMessage("삭제 요청 결과를 확인할 수 없습니다. 저장된 요청 상태를 확인해 주세요.");
          await loadStatus("0", generation, true);
        }
      }
    } finally {
      if (activeAction.current === generation) activeAction.current = null;
      if (current(generation)) setBusy(false);
    }
  }

  async function act(entry: DeleteDisplay, action: "retry" | "cancel") {
    if (busy || activeAction.current === scope.current || (entry.status !== "queued" && entry.status !== "sent")) return;
    const generation = scope.current;
    activeAction.current = generation;
    setBusy(true); setMessage("");
    try {
      await ports[action](binding, entry.operation_id);
      if (!current(generation)) return;
      if (action === "cancel") {
        statusRequest.current++;
        setEntries((previous) => previous.map((item) => item.operation_id === entry.operation_id && (item.status === "queued" || item.status === "sent") ? { ...item, status: "abandoning" } : item));
        setMessage("취소 요청을 이 기기에 저장했습니다.");
      } else setMessage("같은 요청의 서버 결과를 다시 확인합니다.");
      onWorkReady();
    } catch (cause) {
      if (!current(generation)) return;
      const error = String(cause);
      if (error.includes("operation_has_result")) {
        setMessage("서버에서 요청을 처리했거나 처리 중입니다. 결과를 다시 확인합니다.");
        if (onRecheck) onRecheck(entry.operation_id); else onWorkReady();
      } else if (error.includes("operation_retired")) setMessage("서버에서 이전 결과를 회수했습니다. 작업 상태를 대조해야 합니다.");
      else if (error.includes("not_fifo_head") || error.includes("stale_operation")) setMessage("전송 순서가 바뀌었습니다. 상태를 다시 확인하세요.");
      else setMessage(action === "cancel" ? "취소 요청을 저장하지 못했습니다." : "요청을 다시 확인하지 못했습니다.");
    } finally {
      if (activeAction.current === generation) activeAction.current = null;
      if (current(generation)) setBusy(false);
    }
  }

  return <section aria-label="문서 삭제" className="delete-panel">
    <h2>{target.nodeId ? "문서 삭제" : "삭제 요청"}</h2>
    {bindingKey !== target.bindingKey ? <p role="status">작업 공간이 변경됐습니다. 문서를 다시 선택해 주세요.</p> : <>
      {target.nodeId && <p>{target.name} 문서를 휴지통으로 옮길까요?</p>}
      {!confirmed && <div>
        <button data-testid="delete-confirm" disabled={busy || target.kind !== "file" || target.deleted} onClick={() => void submit()}>휴지통으로 이동 요청</button>
        <button data-testid="delete-confirm-cancel" disabled={busy} onClick={onClose}>확인 취소</button>
      </div>}
      {message && <p role="status">{message}</p>}
      {uncertain && <button data-testid="delete-status-refresh" disabled={busy} onClick={() => void loadStatus("0", scope.current, true)}>삭제 요청 상태 다시 확인</button>}
      {entries.length > 0 && <div><h3>삭제 요청</h3>{entries.map((entry) => <div key={entry.operation_id}>
        <p>{entry.node_id === target.nodeId ? target.name : entry.node_id}: {statusText(entry, permissionDeniedOperationId === entry.operation_id)}</p>
        {uncertain && (entry.status === "queued" || entry.status === "sent") && permissionDeniedOperationId !== entry.operation_id && <button data-delete-recheck={entry.operation_id} disabled={busy} onClick={() => { if (onRecheck) onRecheck(entry.operation_id); else onWorkReady(); }}>이 요청 다시 확인</button>}
        {(entry.status === "queued" || entry.status === "sent") && permissionDeniedOperationId === entry.operation_id && <button data-delete-retry={entry.operation_id} disabled={busy} onClick={() => void act(entry, "retry")}>삭제 다시 시도</button>}
        {(entry.status === "queued" || entry.status === "sent") && <button data-delete-cancel={entry.operation_id} disabled={busy} onClick={() => void act(entry, "cancel")}>삭제 요청 취소</button>}
      </div>)}</div>}
      {nextEntries !== null && <button disabled={busy} onClick={() => void loadStatus(nextEntries, scope.current)}>다음 삭제 요청</button>}
    </>}
    {(confirmed || bindingKey !== target.bindingKey) && <button disabled={busy} onClick={onClose}>닫기</button>}
  </section>;
}
