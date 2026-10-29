import { useEffect, useRef, useState } from "react";
import type { WorkspaceFrozenDeleteChoice, WorkspaceFrozenDeleteEntry, WorkspaceRegistration } from "../../../packages/protocol-types";

export type FrozenDeleteReviewItem = Omit<WorkspaceFrozenDeleteEntry, "receipt" | "status"> & { status: "queued" | "sent" | "abandoning" | "failed"; reason: string | null };
export type FrozenDeleteReviewChoice = WorkspaceFrozenDeleteChoice;
export type FrozenDeleteReviewPorts = {
  list(binding: WorkspaceRegistration, after: string | null): Promise<{ binding: WorkspaceRegistration; entries: FrozenDeleteReviewItem[]; next_after_id: string | null }>;
  decide(binding: WorkspaceRegistration, oldBinding: WorkspaceRegistration, operationId: string, choice: FrozenDeleteReviewChoice): Promise<void>;
};
export type FrozenDeleteReviewProps = { binding: WorkspaceRegistration; ports: FrozenDeleteReviewPorts; onApplied(): void };
type Decision = { bindingKey: string; item: FrozenDeleteReviewItem; choice: FrozenDeleteReviewChoice };

function previousStatus(item: FrozenDeleteReviewItem): string {
  if (item.reason === "recovery_conflict") return "서버 복구 결과 확인 필요";
  if (item.status === "failed") return "서버 처리 실패";
  if (item.status === "sent") return "전송 후 결과 미확정";
  if (item.status === "abandoning") return "취소 요청 결과 미확정";
  return "전송 대기";
}

export function FrozenDeleteReview({ binding, ports, onApplied }: FrozenDeleteReviewProps) {
  const key = JSON.stringify(binding);
  const activeKey = useRef(key); activeKey.current = key;
  const scope = useRef(0);
  const actionBusy = useRef(false);
  const itemRequest = useRef(0);
  const [dataKey, setDataKey] = useState("");
  const [items, setItems] = useState<FrozenDeleteReviewItem[]>([]);
  const [after, setAfter] = useState<string | null>(null);
  const [selected, setSelected] = useState<string | null>(null);
  const [confirmation, setConfirmation] = useState<Decision | null>(null);
  const [pending, setPending] = useState<Decision | null>(null);
  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState("");
  const current = (generation: number) => activeKey.current === key && scope.current === generation;

  async function loadItems(cursor: string | null, generation: number) {
    const attempt = ++itemRequest.current;
    try {
      const page = await ports.list(binding, cursor);
      if (!current(generation) || itemRequest.current !== attempt) return;
      if (JSON.stringify(page.binding) !== key) throw new Error("stale_binding");
      setItems(page.entries.filter((item) => item.old_binding.workspace_id === binding.workspace_id && JSON.stringify(item.old_binding) !== key));
      setAfter(page.next_after_id); setDataKey(key); setSelected(null);
    } catch { if (current(generation) && itemRequest.current === attempt) setMessage("이전 삭제 요청을 불러오지 못했습니다."); }
  }

  useEffect(() => {
    const generation = ++scope.current;
    actionBusy.current = false;
    setDataKey(""); setItems([]); setAfter(null); setSelected(null);
    setConfirmation(null); setPending(null); setBusy(false); setMessage("");
    void loadItems(null, generation);
    return () => { scope.current++; };
  }, [key, ports]);

  async function decide(next: Decision) {
    if (actionBusy.current || !current(scope.current) || next.bindingKey !== key || next.item.old_binding.workspace_id !== binding.workspace_id || JSON.stringify(next.item.old_binding) === key) return;
    const generation = scope.current;
    actionBusy.current = true; setBusy(true); setMessage(""); setPending(next); setConfirmation(null);
    try {
      await ports.decide(binding, next.item.old_binding, next.item.operation_id, next.choice);
      if (!current(generation)) return;
      itemRequest.current++;
      setPending(null); setItems((old) => old.filter((item) => item.operation_id !== next.item.operation_id)); setSelected(null);
      if (next.choice.kind === "apply") { setMessage("새 삭제 요청을 이 기기에 저장했습니다. 서버 반영 상태는 삭제 요청에서 확인하세요."); onApplied(); }
      else setMessage("이전 요청을 적용하지 않기로 기록했습니다. 새 삭제 요청을 만들지 않았습니다. 이전 요청의 결과는 확인되지 않았을 수 있습니다.");
    } catch (cause) {
      if (!current(generation)) return;
      if (String(cause).includes("dependent_operations")) {
        setPending(null); setMessage("연결된 작업이 있어 이 요청을 처리할 수 없습니다. 기존 자료는 보존됩니다.");
      } else if (String(cause).includes("InvalidRequest")) {
        setPending(null); setSelected(null);
        setMessage("현재 문서 상태가 변경됐습니다. 목록을 다시 불러와 확인해 주세요.");
      } else if (String(cause).includes("OpenEditSessions")) {
        setPending(null); setMessage("열린 문서를 닫거나 다른 문서로 전환한 뒤 다시 확인하세요.");
      } else if (String(cause).includes("OutboxBlocked") || String(cause).includes("ReconciliationRequired")) {
        setPending(null); setMessage("보존된 초안과 전송 대기 작업을 먼저 확인하세요.");
      } else setMessage("결과를 확인하지 못했습니다. 같은 선택을 다시 확인해 주세요.");
    } finally { if (current(generation)) { actionBusy.current = false; setBusy(false); } }
  }

  const item = dataKey === key ? items.find((entry) => entry.operation_id === selected) : undefined;
  const node = item?.current_node;
  return <section aria-label="이전 삭제 요청 대조" className="frozen-delete-review">
    <h2>이전 삭제 요청 대조</h2>
    <p>현재 상태만으로 이전 요청의 결과를 확정할 수 없습니다.</p>
    <p>현재 상태는 이 기기의 마지막 동기화 목록을 기준으로 표시합니다.</p>
    {dataKey === key && items.map((entry) => <button key={entry.operation_id} data-frozen-delete={entry.operation_id} disabled={busy || pending !== null || confirmation !== null} onClick={() => { setSelected(entry.operation_id); setMessage(""); }}>
      {entry.current_node?.name || entry.node_id} / {previousStatus(entry)}
    </button>)}
    {dataKey === key && after !== null && <button disabled={busy || pending !== null || confirmation !== null} onClick={() => void loadItems(after, scope.current)}>다음 이전 요청</button>}
    {dataKey === key && <button disabled={busy || pending !== null || confirmation !== null} onClick={() => void loadItems(null, scope.current)}>처음 이전 요청</button>}
    {item && <div>
      <p>이전 서버 주소: {item.old_binding.server_origin}</p><p>현재 서버 주소: {binding.server_origin}</p><p>이전 문서 ID: {item.node_id}</p>
      <p>현재 문서: {node ? `${node.name} (${node.node_id}${node.deleted ? ", 삭제됨" : ""})` : "목록에서 확인되지 않음"}</p>
      {item.status === "failed" && item.reason !== "recovery_conflict" && <p>서버에서 이전 삭제 요청을 처리하지 못했습니다.</p>}
      {item.reason === "recovery_conflict" && <p>서버 복구 중 충돌해 이전 요청의 적용 여부를 확인할 수 없습니다.</p>}
      <button data-testid="frozen-delete-apply" disabled={busy || pending !== null || confirmation !== null || !node || node.deleted || node.kind !== "file" || node.node_id !== item.node_id} onClick={() => { if (node) setConfirmation({ bindingKey:key, item, choice:{ kind:"apply", expected_node_etag:node.node_etag } }); }}>현재 문서를 새로 삭제</button>
      <button data-testid="frozen-delete-dismiss" disabled={busy || pending !== null || confirmation !== null} onClick={() => setConfirmation({ bindingKey:key, item, choice:{kind:"dismiss"} })}>이전 요청 미적용</button>
      {confirmation && <div role="alert">
        {confirmation.choice.kind === "apply" ? <><p>{node?.name} 문서를 휴지통으로 옮기는 새 요청을 만들까요?</p><button data-testid="frozen-delete-apply-confirm" disabled={busy} onClick={() => void decide(confirmation)}>새 삭제 요청 확인</button></> : <><p>새 삭제 요청을 만들지 않습니다. 이전 요청의 결과는 확인되지 않았을 수 있습니다.</p><button data-testid="frozen-delete-dismiss-confirm" disabled={busy} onClick={() => void decide(confirmation)}>미적용 확인</button></>}
        <button disabled={busy} onClick={() => setConfirmation(null)}>돌아가기</button>
      </div>}
    </div>}
    {dataKey === key && pending && <button data-testid="frozen-delete-retry" disabled={busy} onClick={() => void decide(pending)}>같은 선택 다시 확인</button>}
    {message && <p role="status">{message}</p>}
  </section>;
}
