import { useEffect, useRef, useState } from "react";
import type { WorkspaceFrozenMoveChoice, WorkspaceFrozenMoveEntry, WorkspaceRegistration } from "../../../packages/protocol-types";

export type FrozenMoveReviewItem = Omit<WorkspaceFrozenMoveEntry, "receipt" | "status"> & { status: "queued" | "sent" | "abandoning" | "failed"; reason: string | null };
export type FrozenMoveReviewChoice = WorkspaceFrozenMoveChoice;
export type FrozenMoveReviewPorts = {
  list(binding: WorkspaceRegistration, after: string | null): Promise<{ binding: WorkspaceRegistration; entries: FrozenMoveReviewItem[]; next_after_id: string | null }>;
  decide(binding: WorkspaceRegistration, oldBinding: WorkspaceRegistration, operationId: string, choice: FrozenMoveReviewChoice): Promise<void>;
};
export type FrozenMoveReviewProps = { binding: WorkspaceRegistration; ports: FrozenMoveReviewPorts; onApplied(): void };

type Decision = { bindingKey: string; item: FrozenMoveReviewItem; choice: FrozenMoveReviewChoice };

function previousStatus(item: FrozenMoveReviewItem): string {
  if (item.reason === "recovery_conflict") return "서버 복구 결과 확인 필요";
  if (item.status === "failed") return "서버 처리 실패";
  if (item.status === "sent") return "전송 후 결과 미확정";
  if (item.status === "abandoning") return "취소 요청 결과 미확정";
  return "전송 대기";
}

export function FrozenMoveReview({ binding, ports, onApplied }: FrozenMoveReviewProps) {
  const key = JSON.stringify(binding);
  const activeKey = useRef(key); activeKey.current = key;
  const scope = useRef(0), actionBusy = useRef(false), itemRequest = useRef(0);
  const [dataKey, setDataKey] = useState("");
  const [items, setItems] = useState<FrozenMoveReviewItem[]>([]);
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
    } catch { if (current(generation) && itemRequest.current === attempt) setMessage("이전 이동 요청을 불러오지 못했습니다."); }
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
      if (next.choice.kind === "apply") { setMessage("새 이동 요청을 이 기기에 저장했습니다. 서버 반영 상태는 이동 요청에서 확인하세요."); onApplied(); }
      else setMessage("이전 요청을 적용하지 않기로 기록했습니다. 새 이동 요청을 만들지 않았습니다. 이전 요청의 결과는 확인되지 않았을 수 있습니다.");
    } catch (cause) {
      if (!current(generation)) return;
      const error = String(cause);
      if (error.includes("InvalidRequest")) {
        setPending(null); setSelected(null); setMessage("현재 문서 상태 또는 목적지가 변경됐습니다. 목록을 다시 불러와 확인해 주세요.");
      } else if (error.includes("OutboxBlocked") || error.includes("ReconciliationRequired") || error.includes("dependent_operations")) {
        setPending(null); setMessage("보존된 초안과 전송 대기 작업을 먼저 확인하세요.");
      } else setMessage("결과를 확인하지 못했습니다. 같은 선택을 다시 확인해 주세요.");
    } finally { if (current(generation)) { actionBusy.current = false; setBusy(false); } }
  }

  const item = dataKey === key ? items.find((entry) => entry.operation_id === selected) : undefined;
  const node = item?.current_node, destination = item?.current_destination;
  const canApply = !!item && !!node && !node.deleted && node.kind === "file" && node.node_id === item.node_id && node.parent_id !== null
    && !!destination && !destination.deleted && destination.kind === "folder" && destination.node_id === item.request.parent_id && destination.node_id !== node.node_id;
  return <section aria-label="이전 이동 요청 대조" className="frozen-move-review">
    <h2>이전 이동 요청 대조</h2>
    <p>현재 상태만으로 이전 요청의 결과를 확정할 수 없습니다.</p>
    <p>현재 상태는 이 기기의 마지막 동기화 목록을 기준으로 표시합니다.</p>
    {dataKey === key && items.map((entry) => <button key={entry.operation_id} data-frozen-move={entry.operation_id} disabled={busy || pending !== null || confirmation !== null} onClick={() => { setSelected(entry.operation_id); setMessage(""); }}>
      {entry.current_node?.name || entry.node_id} / {previousStatus(entry)}
    </button>)}
    {dataKey === key && after !== null && <button disabled={busy || pending !== null || confirmation !== null} onClick={() => void loadItems(after, scope.current)}>다음 이전 요청</button>}
    {dataKey === key && <button disabled={busy || pending !== null || confirmation !== null} onClick={() => void loadItems(null, scope.current)}>처음 이전 요청</button>}
    {item && <div>
      <p>이전 서버 주소: {item.old_binding.server_origin}</p><p>현재 서버 주소: {binding.server_origin}</p><p>이전 문서 ID: {item.node_id}</p>
      <p>이전 요청: 부모 {item.request.expected_parent_id}에서 {item.request.parent_id}로 이동, 이름 {item.request.name}</p>
      <p>현재 문서: {node ? `${node.name} (${node.node_id}${node.deleted ? ", 삭제됨" : ""})` : "목록에서 확인되지 않음"}</p>
      {node && <p>현재 부모: {node.parent_id ?? "없음"}</p>}
      <p>현재 목적지: {destination ? `${destination.name || "/"} (${destination.node_id}${destination.deleted ? ", 삭제됨" : ""})` : "목록에서 확인되지 않음"}</p>
      {item.status === "failed" && item.reason !== "recovery_conflict" && <p>서버에서 이전 이동 요청을 처리하지 못했습니다.</p>}
      {item.reason === "recovery_conflict" && <p>서버 복구 중 충돌해 이전 요청의 적용 여부를 확인할 수 없습니다.</p>}
      <button data-testid="frozen-move-apply" disabled={busy || pending !== null || confirmation !== null || !canApply} onClick={() => {
        if (canApply && node?.parent_id) setConfirmation({ bindingKey:key, item, choice:{ kind:"apply", expected_meta_etag:node.meta_etag, request:{ expected_parent_id:node.parent_id, parent_id:item.request.parent_id, name:item.request.name } } });
      }}>현재 문서를 새로 이동</button>
      <button data-testid="frozen-move-dismiss" disabled={busy || pending !== null || confirmation !== null} onClick={() => setConfirmation({ bindingKey:key, item, choice:{kind:"dismiss"} })}>이전 요청 미적용</button>
      {confirmation && <div role="alert">
        {confirmation.choice.kind === "apply" ? <>
          <p>{node?.name} 문서를 {destination?.name || "/"} 폴더에 {confirmation.choice.request.name} 이름으로 이동하는 새 요청을 만들까요?</p>
          <p>본문을 검사하지 않았습니다. 이동하거나 이름을 바꾸면 이 문서와 다른 문서의 상대 링크가 영향을 받을 수 있습니다.</p>
          <button data-testid="frozen-move-apply-confirm" disabled={busy} onClick={() => void decide(confirmation)}>새 이동 요청 확인</button>
        </> : <><p>새 이동 요청을 만들지 않습니다. 이전 요청의 결과는 확인되지 않았을 수 있습니다.</p><button data-testid="frozen-move-dismiss-confirm" disabled={busy} onClick={() => void decide(confirmation)}>미적용 확인</button></>}
        <button disabled={busy} onClick={() => setConfirmation(null)}>돌아가기</button>
      </div>}
    </div>}
    {dataKey === key && pending && <button data-testid="frozen-move-retry" disabled={busy} onClick={() => void decide(pending)}>같은 선택 다시 확인</button>}
    {message && <p role="status">{message}</p>}
  </section>;
}
