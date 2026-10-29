import { useEffect, useRef, useState } from "react";
import type { NodeState, RestoreNodeRequest, TrashNode, TrashPage, WorkspaceRegistration } from "../../../packages/protocol-types";

export type RestoreDisplay = { operation_id: string; operation_seq: string; node_id: string; request: RestoreNodeRequest; status: "queued" | "sent" | "abandoning" | "abandoned" | "committed" | "failed"; reason: string | null };
export type TrashPanelPorts = {
  trash(binding: WorkspaceRegistration, after: string | null): Promise<TrashPage>;
  folders(after: string | null): Promise<{ nodes: NodeState[]; next_after_id: string | null }>;
  queue(binding: WorkspaceRegistration, node: string, request: RestoreNodeRequest): Promise<RestoreDisplay>;
  status(binding: WorkspaceRegistration, after: string): Promise<{ entries: RestoreDisplay[]; next_after_seq: string | null }>;
  cancel(binding: WorkspaceRegistration, operationId: string): Promise<void>;
};

function statusText(entry: RestoreDisplay): string {
  if (entry.status === "queued") return "전송 대기";
  if (entry.status === "sent") return "서버 결과 확인 중";
  if (entry.status === "abandoning") return "취소 처리 중";
  if (entry.status === "abandoned") return "취소 완료";
  if (entry.status === "committed") return "복원 완료";
  switch (entry.reason) {
    case "history_expired": return "보관된 원문이 만료되어 복원할 수 없습니다.";
    case "destination_exists": return "목적지에 같은 이름이 있습니다.";
    case "metadata_precondition_failed": return "삭제 항목이 변경됐습니다. 목록을 새로고침하세요.";
    case "not_deleted": return "이미 복원됐거나 삭제 상태가 아닙니다.";
    case "node_deleted": case "parent_not_folder": case "not_found": return "목적지 또는 삭제 항목을 다시 확인하세요.";
    default: return "복원하지 못했습니다. 서버 상태를 확인하세요.";
  }
}

export function TrashPanel({ binding, ports, onQueued, onCancelled, onRecheck, onRetry, permissionDeniedOperationId, revision }: { binding: WorkspaceRegistration; ports: TrashPanelPorts; onQueued(): void; onCancelled?(operationId: string): void; onRecheck?(operationId: string): void; onRetry?(operationId: string): void; permissionDeniedOperationId?: string | null; revision: number }) {
  const scope = useRef(0);
  const trashRequest = useRef(0);
  const folderRequest = useRef(0);
  const statusRequest = useRef(0);
  const bindingKey = JSON.stringify(binding);
  const [nodes, setNodes] = useState<TrashNode[]>([]);
  const [next, setNext] = useState<string | null>(null);
  const [folders, setFolders] = useState<NodeState[]>([]);
  const [nextFolders, setNextFolders] = useState<string | null>(null);
  const [selected, setSelected] = useState<TrashNode | null>(null);
  const [parent, setParent] = useState("");
  const [name, setName] = useState("");
  const [entries, setEntries] = useState<RestoreDisplay[]>([]);
  const [nextEntries, setNextEntries] = useState<string | null>(null);
  const [message, setMessage] = useState("");
  const [busy, setBusy] = useState(false);

  async function loadTrash(after: string | null, current: number) {
    const attempt = ++trashRequest.current;
    try {
      const page = await ports.trash(binding, after);
      if (scope.current !== current || trashRequest.current !== attempt) return;
      setNodes(page.nodes); setNext(page.next_after_id); setSelected(null); setParent(""); setMessage("");
    } catch {
      if (scope.current === current && trashRequest.current === attempt) setMessage("휴지통을 불러오지 못했습니다. 연결과 서버 상태를 확인하세요.");
    }
  }
  async function loadFolders(after: string | null, current: number) {
    const attempt = ++folderRequest.current;
    try {
      const page = await ports.folders(after);
      if (scope.current !== current || folderRequest.current !== attempt) return;
      setFolders(page.nodes.filter((node) => node.kind === "folder" && !node.deleted));
      setNextFolders(page.next_after_id); setParent("");
    } catch {
      if (scope.current === current && folderRequest.current === attempt) setMessage("목적지 폴더 목록을 불러오지 못했습니다.");
    }
  }
  async function loadStatus(after: string, current: number) {
    const attempt = ++statusRequest.current;
    try {
      const page = await ports.status(binding, after);
      if (scope.current !== current || statusRequest.current !== attempt) return;
      setEntries(page.entries); setNextEntries(page.next_after_seq);
    } catch {
      if (scope.current === current && statusRequest.current === attempt) setMessage("복원 요청 상태를 불러오지 못했습니다.");
    }
  }
  useEffect(() => {
    const current = ++scope.current;
    setNodes([]); setNext(null); setFolders([]); setNextFolders(null); setSelected(null); setParent(""); setEntries([]); setNextEntries(null); setMessage(""); setBusy(false);
    void loadTrash(null, current); void loadFolders(null, current); void loadStatus("0", current);
    return () => { scope.current++; };
  }, [bindingKey, ports]);
  useEffect(() => { if (scope.current) void loadStatus("0", scope.current); }, [revision]);

  async function submit() {
    if (!selected || !selected.restorable || !parent || !name || busy) return;
    const current = scope.current;
    const request = { expected_meta_rev: selected.meta_rev, parent_id: parent, name };
    setBusy(true); setMessage("");
    try {
      const operation = await ports.queue(binding, selected.node_id, request);
      if (scope.current !== current) return;
      statusRequest.current++;
      setEntries((previous) => [operation, ...previous.filter((entry) => entry.operation_id !== operation.operation_id)]);
      setSelected(null); setMessage("복원 요청을 이 기기에 저장했습니다. 전송 대기 중입니다."); onQueued();
    } catch {
      if (scope.current === current) setMessage("복원 요청을 저장하지 못했습니다. 목록과 등록 상태를 확인하세요.");
    } finally {
      if (scope.current === current) setBusy(false);
    }
  }

  async function cancel(entry: RestoreDisplay) {
    if (busy || (entry.status !== "queued" && entry.status !== "sent")) return;
    const current = scope.current;
    setBusy(true); setMessage("");
    try {
      await ports.cancel(binding, entry.operation_id);
      if (scope.current !== current) return;
      statusRequest.current++;
      setEntries((previous) => previous.map((item) => item.operation_id === entry.operation_id && (item.status === "queued" || item.status === "sent") ? { ...item, status:"abandoning" } : item));
      setMessage("취소 요청을 이 기기에 저장했습니다.");
      onCancelled?.(entry.operation_id);
    } catch (cause) {
      if (scope.current !== current) return;
      const error = String(cause);
      if (error.includes("operation_has_result")) { setMessage("서버에서 이 요청을 처리 중이거나 결과를 확인해야 합니다. 서버 결과를 다시 확인합니다."); onRecheck?.(entry.operation_id); }
      else if (error.includes("operation_retired")) setMessage("서버에서 이전 결과를 회수했습니다. 작업 상태를 대조해야 합니다.");
      else if (error.includes("not_fifo_head") || error.includes("stale_operation")) setMessage("전송 순서가 바뀌었습니다. 상태를 다시 확인하세요.");
      else setMessage("취소 요청을 저장하지 못했습니다. 연결과 서버 상태를 확인하세요.");
    } finally {
      if (scope.current === current) setBusy(false);
    }
  }

  return <section aria-label="휴지통" className="trash-panel">
    <h2>휴지통</h2>
    <button disabled={busy} onClick={() => void loadTrash(null, scope.current)}>휴지통 새로고침</button>
    {nodes.map((node) => <button key={node.node_id} data-trash-node={node.node_id} disabled={busy || !node.restorable} onClick={() => { setSelected(node); setParent(""); setName(node.name); }}>
      {node.name} {node.kind === "folder" ? "(빈 폴더)" : ""} {!node.restorable ? "(복원할 수 없음)" : ""}
    </button>)}
    {next !== null && <button disabled={busy} onClick={() => void loadTrash(next, scope.current)}>다음 삭제 항목</button>}
    {selected && <div>
      <p>{selected.kind === "folder" ? "빈 폴더만 복원하며 자식은 복원하지 않습니다." : "복원할 위치와 이름을 선택하세요."}</p>
      <label>목적지 폴더<select data-testid="restore-parent" value={parent} onChange={(event) => setParent(event.target.value)}><option value="">폴더 선택</option>{folders.map((folder) => <option key={folder.node_id} value={folder.node_id}>{folder.parent_id === null ? "/" : `${folder.name} (${folder.node_id})`}</option>)}</select></label>
      {nextFolders !== null && <button disabled={busy} onClick={() => void loadFolders(nextFolders, scope.current)}>다음 폴더</button>}
      <button disabled={busy} onClick={() => void loadFolders(null, scope.current)}>처음 폴더</button>
      <label>이름<input value={name} onChange={(event) => setName(event.target.value)} /></label>
      <button data-testid="restore-submit" disabled={busy || !parent || !name} onClick={() => void submit()}>복원 요청</button>
    </div>}
    {message && <p role="status">{message}</p>}
    {entries.length > 0 && <div><h3>복원 요청</h3>{entries.map((entry) => <div key={entry.operation_id}><p>{entry.request.name}: {statusText(entry)} {(entry.status === "queued" || entry.status === "sent") && permissionDeniedOperationId === entry.operation_id && "복원 권한이 없습니다."}</p>{(entry.status === "queued" || entry.status === "sent") && permissionDeniedOperationId === entry.operation_id && <button data-restore-retry={entry.operation_id} disabled={busy || !onRetry} onClick={() => onRetry?.(entry.operation_id)}>복원 다시 시도</button>}{(entry.status === "queued" || entry.status === "sent") && <button data-restore-cancel={entry.operation_id} disabled={busy} onClick={() => void cancel(entry)}>복원 요청 취소</button>}</div>)}</div>}
    {nextEntries !== null && <button disabled={busy} onClick={() => void loadStatus(nextEntries, scope.current)}>다음 복원 요청</button>}
  </section>;
}
