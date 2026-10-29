import { useEffect, useRef, useState } from "react";
import type { NodeState, WorkspaceRegistration } from "../../../packages/protocol-types";

export type CreateDisplay = {
  operation_id: string;
  operation_seq: string;
  node_id: string;
  parent_id: string;
  name: string;
  kind: "file" | "folder";
  status: "queued" | "sent" | "abandoning" | "abandoned" | "committed" | "failed";
  reason: string | null;
};

export type CreatePanelPorts = {
  // The port returns confirmed, live server metadata for this binding only.
  folders(binding: WorkspaceRegistration, after: string | null): Promise<{ nodes: NodeState[]; next_after_id: string | null }>;
  queue(binding: WorkspaceRegistration, request: { parent_id: string; name: string; kind: "file" | "folder" }): Promise<CreateDisplay>;
  status(binding: WorkspaceRegistration, after: string): Promise<{ entries: CreateDisplay[]; next_after_seq: string | null }>;
  // Retry rechecks/resumes the same durable operation; it does not allocate a new ID.
  retry(binding: WorkspaceRegistration, operationId: string): Promise<void>;
  // Cancel persists intent; it does not imply that the server cancelled an admitted operation.
  cancel(binding: WorkspaceRegistration, operationId: string): Promise<void>;
};

export type CreatePanelProps = {
  binding: WorkspaceRegistration;
  ports: CreatePanelPorts;
  revision: number;
  permissionDeniedOperationId?: string | null;
  onWorkReady(): void;
  onRecheck?(operationId: string): void;
};

function statusText(entry: CreateDisplay, denied: boolean): string {
  if (denied && (entry.status === "queued" || entry.status === "sent")) return "생성 권한이 없습니다.";
  if (entry.status === "queued") return "전송 대기";
  if (entry.status === "sent") return "서버 결과 확인 중";
  if (entry.status === "abandoning") return "취소 처리 중";
  if (entry.status === "abandoned") return "취소 완료";
  if (entry.status === "committed") return "생성 완료";
  switch (entry.reason) {
    case "destination_exists": return "목적지에 같은 이름이 있습니다.";
    case "node_id_conflict": return "항목 식별자가 충돌했습니다.";
    case "node_deleted": case "parent_not_folder": case "not_found": return "목적지 폴더를 다시 확인하세요.";
    default: return "항목을 생성하지 못했습니다. 서버 상태를 확인하세요.";
  }
}

export function CreatePanel({ binding, ports, revision, permissionDeniedOperationId, onWorkReady, onRecheck }: CreatePanelProps) {
  const bindingKey = JSON.stringify(binding);
  const activeKey = useRef(bindingKey);
  activeKey.current = bindingKey;
  const scope = useRef(0);
  const activeAction = useRef<number | null>(null);
  const folderRequest = useRef(0);
  const statusRequest = useRef(0);
  const [folders, setFolders] = useState<NodeState[]>([]);
  const [nextFolders, setNextFolders] = useState<string | null>(null);
  const [parent, setParent] = useState("");
  const [name, setName] = useState("");
  const [kind, setKind] = useState<"file" | "folder">("folder");
  const [entries, setEntries] = useState<CreateDisplay[]>([]);
  const [nextEntries, setNextEntries] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState("");

  function current(generation: number) {
    return scope.current === generation && activeKey.current === bindingKey;
  }

  async function loadFolders(after: string | null, generation: number) {
    const attempt = ++folderRequest.current;
    try {
      const page = await ports.folders(binding, after);
      if (!current(generation) || folderRequest.current !== attempt) return;
      setFolders(page.nodes.filter((node) => node.kind === "folder" && !node.deleted));
      setNextFolders(page.next_after_id);
      setParent("");
    } catch {
      if (current(generation) && folderRequest.current === attempt) setMessage("폴더 목록을 불러오지 못했습니다.");
    }
  }

  async function loadStatus(after: string, generation: number) {
    const attempt = ++statusRequest.current;
    try {
      const page = await ports.status(binding, after);
      if (!current(generation) || statusRequest.current !== attempt) return;
      setEntries(page.entries);
      setNextEntries(page.next_after_seq);
    } catch {
      if (current(generation) && statusRequest.current === attempt) setMessage("생성 요청 상태를 불러오지 못했습니다.");
    }
  }

  useEffect(() => {
    const generation = ++scope.current;
    setFolders([]); setNextFolders(null); setParent(""); setName(""); setKind("folder");
    setEntries([]); setNextEntries(null); setBusy(false); setMessage("");
    void loadFolders(null, generation);
    void loadStatus("0", generation);
    return () => { scope.current++; };
  }, [bindingKey, ports]);
  useEffect(() => { if (scope.current) void loadStatus("0", scope.current); }, [revision]);

  async function submit() {
    if (busy || activeAction.current === scope.current || !name || !folders.some((folder) => folder.node_id === parent)) return;
    if (kind === "file" && !name.toLowerCase().endsWith(".md")) { setMessage("문서 이름은 .md로 끝나야 합니다."); return; }
    const generation = scope.current;
    activeAction.current = generation;
    setBusy(true); setMessage("");
    try {
      const operation = await ports.queue(binding, { parent_id: parent, name, kind });
      if (!current(generation)) return;
      statusRequest.current++;
      setEntries((previous) => [operation, ...previous.filter((item) => item.operation_id !== operation.operation_id)]);
      setName(""); setMessage("생성 요청을 이 기기에 저장했습니다. 전송 대기 중입니다.");
      onWorkReady();
    } catch {
      if (current(generation)) setMessage("생성 요청을 저장하지 못했습니다. 이름과 등록 상태를 확인하세요.");
    } finally {
      if (activeAction.current === generation) activeAction.current = null;
      if (current(generation)) setBusy(false);
    }
  }

  async function act(entry: CreateDisplay, action: "retry" | "cancel") {
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

  return <section aria-label="새 폴더 또는 문서" className="create-panel">
    <h2>새 폴더 또는 문서</h2>
    <label>목적지 폴더<select data-testid="create-parent" value={parent} disabled={busy} onChange={(event) => setParent(event.target.value)}>
      <option value="">폴더 선택</option>
      {folders.map((folder) => <option key={folder.node_id} value={folder.node_id}>{folder.parent_id === null ? "/" : `${folder.name} (${folder.node_id})`}</option>)}
    </select></label>
    {nextFolders !== null && <button disabled={busy} onClick={() => void loadFolders(nextFolders, scope.current)}>다음 폴더</button>}
    <button disabled={busy} onClick={() => void loadFolders(null, scope.current)}>처음 폴더</button>
    <label>종류<select data-testid="create-kind" value={kind} disabled={busy} onChange={(event) => setKind(event.target.value === "file" ? "file" : "folder")}><option value="folder">폴더</option><option value="file">빈 문서</option></select></label>
    <label>이름<input data-testid="create-name" value={name} disabled={busy} onChange={(event) => setName(event.target.value)} /></label>
    <button data-testid="create-submit" disabled={busy || !name || !parent} onClick={() => void submit()}>생성 요청</button>
    {message && <p role="status">{message}</p>}
    {entries.length > 0 && <div><h3>생성 요청</h3>{entries.map((entry) => <div key={entry.operation_id}>
      <p>{entry.kind === "file" ? "문서" : "폴더"} {entry.name}: {statusText(entry, permissionDeniedOperationId === entry.operation_id)}{entry.kind === "file" && (entry.status === "queued" || entry.status === "sent" || entry.status === "committed") ? " 서버 반영과 동기화 후 열 수 있습니다." : ""}</p>
      {(entry.status === "queued" || entry.status === "sent") && permissionDeniedOperationId === entry.operation_id && <button data-create-retry={entry.operation_id} disabled={busy} onClick={() => void act(entry, "retry")}>생성 다시 시도</button>}
      {(entry.status === "queued" || entry.status === "sent") && <button data-create-cancel={entry.operation_id} disabled={busy} onClick={() => void act(entry, "cancel")}>생성 요청 취소</button>}
    </div>)}</div>}
    {nextEntries !== null && <button disabled={busy} onClick={() => void loadStatus(nextEntries, scope.current)}>다음 생성 요청</button>}
  </section>;
}
