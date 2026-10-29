import { useEffect, useRef, useState } from "react";
import type { WorkspaceRegistration } from "../../../packages/protocol-types";

type ObservedNode = { node_id: string; parent_id: string | null; name: string; kind: "file" | "folder"; deleted: boolean };
export type FrozenCreateReviewItem = {
  old_binding: WorkspaceRegistration;
  operation_id: string;
  operation_seq: string;
  node_id: string;
  parent_id: string;
  name: string;
  kind: "file" | "folder";
  status: "queued" | "sent" | "abandoning" | "failed";
  reason?: string | null;
  current_same_id: ObservedNode | null;
  current_same_name: ObservedNode[];
};
export type FrozenCreateReviewChoice =
  | { kind: "apply"; parent_id: string; name: string }
  | { kind: "dismiss" };
export type FrozenCreateReviewPorts = {
  list(binding: WorkspaceRegistration, after: string | null): Promise<{ binding: WorkspaceRegistration; entries: FrozenCreateReviewItem[]; next_after_id: string | null }>;
  folders(binding: WorkspaceRegistration, after: string | null): Promise<{ nodes: ObservedNode[]; next_after_id: string | null }>;
  decide(binding: WorkspaceRegistration, oldBinding: WorkspaceRegistration, operationId: string, choice: FrozenCreateReviewChoice): Promise<void>;
};
export type FrozenCreateReviewProps = {
  binding: WorkspaceRegistration;
  ports: FrozenCreateReviewPorts;
  onApplied(): void;
};
type Pending = { bindingKey: string; item: FrozenCreateReviewItem; choice: FrozenCreateReviewChoice };

function label(node: ObservedNode): string {
  return `${node.name || "/"} (${node.node_id}, ${node.kind === "folder" ? "폴더" : "파일"}${node.deleted ? ", 삭제됨" : ""})`;
}

function failureReason(reason: string | null | undefined): string {
  switch (reason) {
    case "destination_exists": return "목적지에 같은 이름이 있습니다.";
    case "node_id_conflict": return "항목 식별자가 충돌했습니다.";
    case "node_deleted": case "parent_not_folder": case "not_found": return "이전 목적지 폴더를 확인할 수 없습니다.";
    default: return "서버에서 이전 요청을 처리하지 못했습니다.";
  }
}

export function FrozenCreateReview({ binding, ports, onApplied }: FrozenCreateReviewProps) {
  const key = JSON.stringify(binding);
  const activeKey = useRef(key); activeKey.current = key;
  const scope = useRef(0);
  const actionBusy = useRef(false);
  const itemRequest = useRef(0);
  const folderRequest = useRef(0);
  const [dataKey, setDataKey] = useState("");
  const [items, setItems] = useState<FrozenCreateReviewItem[]>([]);
  const [folders, setFolders] = useState<ObservedNode[]>([]);
  const [after, setAfter] = useState<string | null>(null);
  const [folderAfter, setFolderAfter] = useState<string | null>(null);
  const [parent, setParent] = useState("");
  const [name, setName] = useState("");
  const [selected, setSelected] = useState<string | null>(null);
  const [confirmDismiss, setConfirmDismiss] = useState(false);
  const [busy, setBusy] = useState(false);
  const [pending, setPending] = useState<Pending | null>(null);
  const [message, setMessage] = useState("");
  const current = (generation: number) => activeKey.current === key && scope.current === generation;

  async function loadItems(cursor: string | null, generation: number) {
    const attempt = ++itemRequest.current;
    try {
      const page = await ports.list(binding, cursor);
      if (!current(generation) || itemRequest.current !== attempt) return;
      if (JSON.stringify(page.binding) !== key) throw new Error("stale_binding");
      setItems(page.entries.filter((item) => item.old_binding.workspace_id === binding.workspace_id));
      setAfter(page.next_after_id); setDataKey(key); setSelected(null);
    } catch { if (current(generation) && itemRequest.current === attempt) setMessage("이전 생성 요청을 불러오지 못했습니다."); }
  }
  async function loadFolders(cursor: string | null, generation: number) {
    const attempt = ++folderRequest.current;
    try {
      const page = await ports.folders(binding, cursor);
      if (!current(generation) || folderRequest.current !== attempt) return;
      setFolders(page.nodes.filter((node) => node.kind === "folder" && !node.deleted));
      setFolderAfter(page.next_after_id); setParent("");
    } catch { if (current(generation) && folderRequest.current === attempt) setMessage("현재 폴더 목록을 불러오지 못했습니다."); }
  }
  useEffect(() => {
    const generation = ++scope.current;
    actionBusy.current = false;
    setDataKey(""); setItems([]); setFolders([]); setAfter(null); setFolderAfter(null);
    setSelected(null); setParent(""); setName(""); setBusy(false); setPending(null); setConfirmDismiss(false); setMessage("");
    void loadItems(null, generation); void loadFolders(null, generation);
    return () => { scope.current++; };
  }, [key, ports]);

  async function decide(next: Pending) {
    if (actionBusy.current || !current(scope.current) || next.bindingKey !== key || next.item.old_binding.workspace_id !== binding.workspace_id) return;
    if (next.choice.kind === "apply" && next.item.kind === "file" && !next.choice.name.toLowerCase().endsWith(".md")) { setMessage("문서 이름은 .md로 끝나야 합니다."); return; }
    const generation = scope.current;
    actionBusy.current = true; setBusy(true); setMessage(""); setPending(next);
    try {
      await ports.decide(binding, next.item.old_binding, next.item.operation_id, next.choice);
      if (!current(generation)) return;
      itemRequest.current++;
      setPending(null); setConfirmDismiss(false);
      setItems((old) => old.filter((item) => item.operation_id !== next.item.operation_id));
      setSelected(null);
      if (next.choice.kind === "apply") { setMessage("새 생성 요청을 이 기기에 저장했습니다. 서버 반영 상태는 새 폴더 또는 문서에서 확인하세요."); onApplied(); }
      else setMessage("이전 요청을 적용하지 않기로 기록했습니다. 서버 항목은 삭제하지 않았습니다.");
    } catch (cause) {
      if (!current(generation)) return;
      if (String(cause).includes("dependent_operations")) {
        setPending(null); setConfirmDismiss(false); setMessage("연결된 작업이 있어 이 요청을 처리할 수 없습니다. 기존 자료는 보존됩니다.");
      } else setMessage("결과를 확인하지 못했습니다. 같은 선택을 다시 확인해 주세요.");
    } finally { if (current(generation)) { actionBusy.current = false; setBusy(false); } }
  }
  const item = dataKey === key ? items.find((entry) => entry.operation_id === selected) : undefined;
  return <section aria-label="이전 생성 요청 대조" className="frozen-create-review">
    <h2>이전 생성 요청 대조</h2>
    <p>결과가 미확정인 요청은 현재 목록만으로 이전 처리 여부를 판단할 수 없습니다.</p>
    <p>현재 상태는 이 기기의 마지막 동기화 목록을 기준으로 표시합니다.</p>
    {dataKey === key && items.map((entry) => <button key={entry.operation_id} data-review-operation={entry.operation_id} disabled={busy || pending !== null} onClick={() => { setSelected(entry.operation_id); setParent(""); setName(entry.name); setConfirmDismiss(false); setMessage(""); }}>
      {entry.kind === "file" ? "문서" : "폴더"} {entry.name} / {entry.reason === "recovery_conflict" ? "서버 복구 결과 확인 필요" : entry.status === "failed" ? "서버 처리 실패" : entry.status === "sent" ? "전송 후 결과 미확정" : entry.status === "abandoning" ? "취소 요청 결과 미확정" : "전송 대기"}
    </button>)}
    {dataKey === key && after !== null && <button disabled={busy || pending !== null} onClick={() => void loadItems(after, scope.current)}>다음 이전 요청</button>}
    {dataKey === key && <button disabled={busy || pending !== null} onClick={() => void loadItems(null, scope.current)}>처음 이전 요청</button>}
    {item && <div>
      <p>이전 서버 주소: {item.old_binding.server_origin}</p><p>현재 서버 주소: {binding.server_origin}</p><p>이전 node ID: {item.node_id}</p><p>이전 부모 ID: {item.parent_id}</p><p>이전 이름: {item.name}</p><p>이전 종류: {item.kind === "file" ? "문서" : "폴더"}</p>
      <p>현재 같은 ID: {item.current_same_id ? label(item.current_same_id) : "목록에서 확인되지 않음"}</p>
      <p>현재 같은 이름: {item.current_same_name.length ? item.current_same_name.map(label).join(", ") : "목록에서 확인되지 않음"}</p>
      {item.reason === "recovery_conflict" ? <p>서버 복구 중 충돌해 이전 요청의 적용 여부를 확인할 수 없습니다. 현재 상태를 대조하세요.</p> : item.status === "failed" ? <p>서버 처리 실패: {failureReason(item.reason)}</p> : <p>같은 이름이나 ID가 보여도 이전 요청의 서버 결과는 확정하지 않습니다.</p>}
      <label>현재 목적지<select data-testid="review-parent" value={parent} disabled={busy || pending !== null} onChange={(event) => setParent(event.target.value)}>
        <option value="">폴더 선택</option>{folders.map((folder) => <option key={folder.node_id} value={folder.node_id}>{label(folder)}</option>)}
      </select></label>
      {folderAfter !== null && <button disabled={busy || pending !== null} onClick={() => void loadFolders(folderAfter, scope.current)}>다음 폴더</button>}
      <button disabled={busy || pending !== null} onClick={() => void loadFolders(null, scope.current)}>처음 폴더</button>
      <label>새 {item.kind === "file" ? "문서" : "폴더"} 이름<input data-testid="review-name" value={name} disabled={busy || pending !== null} onChange={(event) => setName(event.target.value)} /></label>
      <button data-testid="review-apply" disabled={busy || pending !== null || !name || !folders.some((folder) => folder.node_id === parent)} onClick={() => void decide({ bindingKey: key, item, choice: { kind: "apply", parent_id: parent, name } })}>새 생성 요청으로 적용</button>
      <button data-testid="review-dismiss" disabled={busy || pending !== null} onClick={() => setConfirmDismiss(true)}>이전 요청 미적용</button>
      {confirmDismiss && !pending && <div role="alert"><p>이 요청을 적용하지 않습니다. 이전 기록은 보존하며 서버 항목은 삭제하지 않습니다.</p><button data-testid="review-dismiss-confirm" disabled={busy} onClick={() => void decide({ bindingKey: key, item, choice: { kind: "dismiss" } })}>미적용 확인</button><button disabled={busy} onClick={() => setConfirmDismiss(false)}>돌아가기</button></div>}
    </div>}
    {dataKey === key && pending && <button data-testid="review-retry" disabled={busy} onClick={() => void decide(pending)}>같은 선택 다시 확인</button>}
    {message && <p role="status">{message}</p>}
  </section>;
}
