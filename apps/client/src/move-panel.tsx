import { useEffect, useMemo, useRef, useState } from "react";
import type { MoveNodeRequest, NodeState, WorkspaceRegistration } from "../../../packages/protocol-types";

export type MoveDisplay = {
  operation_id: string; operation_seq: string; node_id: string; expected_meta_etag: string;
  request: MoveNodeRequest; status: "queued" | "sent" | "abandoning" | "abandoned" | "committed" | "failed"; reason: string | null;
};
export type MoveInspection =
  | { status: "checked"; affected: number; unresolved: number; isCurrent(): boolean }
  | { status: "unavailable" };
export type MovePanelPorts = {
  folders(binding: WorkspaceRegistration, after: string | null): Promise<{ nodes: NodeState[]; next_after_id: string | null }>;
  queue(binding: WorkspaceRegistration, request: { node_id: string; expected_meta_etag: string; request: MoveNodeRequest }): Promise<MoveDisplay>;
  status(binding: WorkspaceRegistration, after: string): Promise<{ entries: MoveDisplay[]; next_after_seq: string | null }>;
  retry(binding: WorkspaceRegistration, operationId: string): Promise<void>;
  cancel(binding: WorkspaceRegistration, operationId: string): Promise<void>;
  inspect?(binding: WorkspaceRegistration, nodeId: string, request: MoveNodeRequest): MoveInspection;
};
export type MovePanelProps = {
  binding: WorkspaceRegistration; node: NodeState | null; displayName?: string; ports: MovePanelPorts;
  revision: number; treeRevision?: number; permissionDeniedOperationId?: string | null; onClose(): void; onWorkReady(): void; onRecheck?(operationId: string): void;
};

function folderPath(folder: NodeState, index: ReadonlyMap<string, NodeState>): string | null {
  const seen = new Set<string>();
  const parts: string[] = [];
  let current: NodeState | undefined = folder;
  while (current) {
    if (seen.has(current.node_id) || current.deleted || current.kind !== "folder") return null;
    seen.add(current.node_id);
    if (current.parent_id === null) return current.name === "" ? parts.reverse().join("/") || "/" : null;
    parts.push(current.name);
    current = index.get(current.parent_id);
  }
  return null;
}

function statusText(entry: MoveDisplay, denied: boolean): string {
  if (denied && (entry.status === "queued" || entry.status === "sent")) return "이동 권한이 없습니다.";
  if (entry.status === "queued") return "전송 대기";
  if (entry.status === "sent") return "서버 결과 확인 중";
  if (entry.status === "abandoning") return "취소 처리 중";
  if (entry.status === "abandoned") return "취소 완료";
  if (entry.status === "committed") return "이동 완료";
  switch (entry.reason) {
    case "metadata_precondition_failed": case "expected_parent_mismatch": return "문서 상태가 변경됐습니다. 목록을 갱신한 뒤 다시 확인하세요.";
    case "destination_exists": return "목적지에 같은 이름이 있습니다.";
    case "recovery_conflict": return "서버 결과를 대조하고 보존 자료를 확인하세요.";
    case "node_deleted": case "not_found": case "parent_not_folder": return "문서 또는 목적지 폴더를 다시 확인하세요.";
    default: return "이동 결과를 확인하고 다시 시도해 주세요.";
  }
}

function knownQueueRejection(cause: unknown): boolean {
  const error = String(cause);
  return ["OutboxBlocked", "ReconciliationRequired", "InvalidRequest", "stale_registration", "stale_renderer_session", "pairing_in_progress"].some((code) => error.includes(code));
}

function queueError(cause: unknown): string {
  const error = String(cause);
  if (error.includes("OutboxBlocked")) return "이 문서의 전송 대기 작업을 확인해 주세요.";
  if (error.includes("ReconciliationRequired")) return "이전 등록의 보존 자료를 확인해 주세요.";
  if (error.includes("InvalidRequest")) return "문서 상태 또는 목적지가 변경됐습니다. 목록을 갱신한 뒤 다시 확인하세요.";
  return "이동 요청을 저장하지 못했습니다. 등록 상태를 확인하세요.";
}

export function MovePanel({ binding, node, displayName, ports, revision, treeRevision = 0, permissionDeniedOperationId, onClose, onWorkReady, onRecheck }: MovePanelProps) {
  const bindingKey = JSON.stringify(binding);
  const [target] = useState(() => ({ bindingKey, nodeId: node?.node_id ?? null, parentId: node?.parent_id ?? null, path: displayName ?? node?.name ?? "", name: node?.name ?? "", metaEtag: node?.meta_etag ?? null, kind: node?.kind, deleted: node?.deleted }));
  const activeKey = useRef(bindingKey); activeKey.current = bindingKey;
  const scope = useRef(0), activeAction = useRef<number | null>(null), statusRequest = useRef(0), folderRequest = useRef(0);
  const [folders, setFolders] = useState<NodeState[]>([]);
  const folderIndex = useMemo(() => new Map(folders.map((item) => [item.node_id, item])), [folders]);
  const [parent, setParent] = useState("");
  const [name, setName] = useState(target.name);
  const [review, setReview] = useState<{ request: MoveNodeRequest; destination: string; inspection: MoveInspection | null } | null>(null);
  const [entries, setEntries] = useState<MoveDisplay[]>([]);
  const [nextEntries, setNextEntries] = useState<string | null>(null);
  const [busy, setBusy] = useState(false), [confirmed, setConfirmed] = useState(node === null), [uncertain, setUncertain] = useState(false);
  const [message, setMessage] = useState("");
  const treeVersion = useRef(treeRevision);
  treeVersion.current = treeRevision;
  const lastTreeVersion = useRef(treeRevision);

  function current(generation: number) {
    return scope.current === generation && activeKey.current === bindingKey && bindingKey === target.bindingKey;
  }

  async function readFolders(): Promise<NodeState[]> {
    const result: NodeState[] = [];
    const ids = new Set<string>(), cursors = new Set<string>();
    let after: string | null = null;
    for (let pageIndex = 0; pageIndex < 512; pageIndex++) {
      const page = await ports.folders(binding, after);
      for (const folder of page.nodes) {
        if (ids.has(folder.node_id)) throw new Error("duplicate_folder");
        ids.add(folder.node_id);
        if (folder.kind === "folder" && !folder.deleted) result.push(folder);
      }
      if (page.next_after_id === null) return result;
      if (!page.nodes.length || cursors.has(page.next_after_id)) throw new Error("invalid_folder_page");
      cursors.add(page.next_after_id); after = page.next_after_id;
    }
    throw new Error("too_many_folders");
  }

  async function loadFolders(generation: number) {
    const attempt = ++folderRequest.current;
    try {
      const nodes = await readFolders();
      if (!current(generation) || folderRequest.current !== attempt) return;
      setFolders(nodes); setParent(""); setReview(null);
    } catch {
      if (current(generation) && folderRequest.current === attempt) setMessage("폴더 목록을 불러오지 못했습니다.");
    }
  }

  async function loadStatus(after: string, generation: number, preserveMessage = false) {
    const attempt = ++statusRequest.current;
    try {
      const page = await ports.status(binding, after);
      if (!current(generation) || statusRequest.current !== attempt) return;
      setEntries(page.entries); setNextEntries(page.next_after_seq);
    } catch {
      if (!preserveMessage && current(generation) && statusRequest.current === attempt) setMessage("이동 요청 상태를 불러오지 못했습니다.");
    }
  }

  useEffect(() => {
    const generation = ++scope.current;
    setFolders([]); setParent(""); setName(target.name); setReview(null);
    setEntries([]); setNextEntries(null); setBusy(false); setMessage("");
    if (bindingKey === target.bindingKey) {
      if (target.nodeId) void loadFolders(generation);
      void loadStatus("0", generation);
    }
    return () => { scope.current++; };
  }, [bindingKey, ports]);
  useEffect(() => { if (scope.current && bindingKey === target.bindingKey) void loadStatus("0", scope.current); }, [revision]);
  useEffect(() => {
    if (lastTreeVersion.current === treeRevision) return;
    lastTreeVersion.current = treeRevision;
    setReview(null);
    setMessage("작업 공간의 동기화된 경로가 변경됐습니다. 목적지를 다시 확인하세요.");
    if (scope.current && target.nodeId && bindingKey === target.bindingKey) void loadFolders(scope.current);
  }, [treeRevision]);

  function prepare() {
    if (busy || confirmed || bindingKey !== target.bindingKey || !target.nodeId || !target.parentId || !target.metaEtag || target.kind !== "file" || target.deleted) return;
    const folder = folders.find((item) => item.node_id === parent);
    if (!folder || !name || !name.toLowerCase().endsWith(".md")) { setMessage("목적지와 .md로 끝나는 문서 이름을 확인하세요."); return; }
    const destination = folderPath(folder, folderIndex);
    if (destination === null) { setMessage("목적지 경로를 확인할 수 없습니다. 폴더 목록을 다시 확인하세요."); return; }
    const request = { expected_parent_id: target.parentId, parent_id: parent, name };
    let inspection: MoveInspection | null;
    try { inspection = ports.inspect?.(binding, target.nodeId, request) ?? null; }
    catch { inspection = { status: "unavailable" }; }
    setMessage("");
    setReview({ request, destination, inspection });
  }

  async function submit() {
    if (busy || confirmed || activeAction.current === scope.current || !review || bindingKey !== target.bindingKey || !target.nodeId || !target.metaEtag) return;
    const generation = scope.current;
    activeAction.current = generation; setBusy(true); setMessage("");
    let submitted = false;
    try {
      if (review.inspection?.status === "checked" && !review.inspection.isCurrent()) {
        setReview(null); setMessage("열린 문서가 변경됐습니다. 링크를 다시 검사해 주세요."); return;
      }
      const checkedTreeVersion = treeRevision;
      const fresh = await readFolders();
      if (!current(generation)) return;
      if (review.inspection?.status === "checked" && !review.inspection.isCurrent()) {
        setReview(null); setMessage("열린 문서가 변경됐습니다. 링크를 다시 검사해 주세요."); return;
      }
      const folder = fresh.find((item) => item.node_id === review.request.parent_id);
      const freshIndex = new Map(fresh.map((item) => [item.node_id, item]));
      const path = folder ? folderPath(folder, freshIndex) : null;
      if (!path || path !== review.destination || checkedTreeVersion !== treeVersion.current) {
        setFolders(fresh); setParent(""); setReview(null);
        setMessage("목적지 경로가 변경됐거나 확인할 수 없습니다. 새 경로를 다시 확인하세요.");
        return;
      }
      submitted = true;
      const operation = await ports.queue(binding, { node_id: target.nodeId, expected_meta_etag: target.metaEtag, request: review.request });
      if (!current(generation)) return;
      statusRequest.current++;
      setEntries((previous) => [operation, ...previous.filter((item) => item.operation_id !== operation.operation_id)]);
      setConfirmed(true); setReview(null); setMessage("이동 요청을 이 기기에 저장했습니다. 전송 대기 중입니다.");
      onWorkReady();
    } catch (cause) {
      if (current(generation)) {
        if (!submitted) { setReview(null); setMessage("목적지 경로를 확인할 수 없습니다. 폴더 목록을 다시 확인하세요."); }
        else if (knownQueueRejection(cause)) { setReview(null); setMessage(queueError(cause)); }
        else {
          setConfirmed(true); setUncertain(true); setReview(null);
          setMessage("이동 요청 결과를 확인할 수 없습니다. 저장된 요청 상태를 확인해 주세요.");
          await loadStatus("0", generation, true);
        }
      }
    } finally {
      if (activeAction.current === generation) activeAction.current = null;
      if (current(generation)) setBusy(false);
    }
  }

  async function act(entry: MoveDisplay, action: "retry" | "cancel") {
    if (busy || activeAction.current === scope.current || (entry.status !== "queued" && entry.status !== "sent")) return;
    const generation = scope.current;
    activeAction.current = generation; setBusy(true); setMessage("");
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

  return <section aria-label="문서 이름 변경 또는 이동" className="move-panel">
    <h2>{target.nodeId ? "문서 이름 변경 또는 이동" : "이동 요청"}</h2>
    {bindingKey !== target.bindingKey ? <p role="status">작업 공간이 변경됐습니다. 문서를 다시 선택해 주세요.</p> : <>
      {target.nodeId && !confirmed && <>
        <p>선택 당시 문서: {target.path}</p>
        {!review ? <>
          <label>목적지 폴더<select data-testid="move-parent" value={parent} disabled={busy} onChange={(event) => { setParent(event.target.value); setReview(null); }}>
            <option value="">폴더 선택</option>
            {folders.map((folder) => <option key={folder.node_id} value={folder.node_id}>{folderPath(folder, folderIndex) ?? "[경로 확인 필요]"} ({folder.node_id})</option>)}
          </select></label>
          <button disabled={busy} onClick={() => void loadFolders(scope.current)}>폴더 목록 새로고침</button>
          <label>새 이름<input data-testid="move-name" value={name} disabled={busy} onChange={(event) => { setName(event.target.value); setReview(null); }} /></label>
          <button data-testid="move-prepare" disabled={busy || !parent || !name} onClick={prepare}>이동 검토</button>
        </> : <>
          <p>기존 부모: {target.parentId}</p>
          <p>이 기기에 동기화된 경로: {review.destination} ({review.request.parent_id})</p>
          <p>새 이름: {review.request.name}</p>
          {review.inspection?.status === "checked" ? <p>열린 문서의 미리보기 링크 검사: 영향 가능 {review.inspection.affected}건, 확인하지 못한 링크 {review.inspection.unresolved}건. 목록에 없는 대상은 확인하지 못할 수 있습니다. 다른 문서와 지원하지 않는 구문은 검사하지 않았습니다.</p>
            : <p>본문을 검사하지 않았습니다. 이동하거나 이름을 바꾸면 이 문서와 다른 문서의 상대 링크가 영향을 받을 수 있습니다.</p>}
          <button data-testid="move-confirm" disabled={busy} onClick={() => void submit()}>이동 요청</button>
          <button data-testid="move-confirm-cancel" disabled={busy} onClick={() => setReview(null)}>취소</button>
        </>}
      </>}
      {message && <p role="status">{message}</p>}
      {entries.length > 0 && <div><h3>이동 요청</h3>{entries.map((entry) => <div key={entry.operation_id}>
        <p>{entry.request.name} ({entry.node_id}): {statusText(entry, permissionDeniedOperationId === entry.operation_id)}</p>
        {uncertain && (entry.status === "queued" || entry.status === "sent") && permissionDeniedOperationId !== entry.operation_id && <button data-move-recheck={entry.operation_id} disabled={busy} onClick={() => { if (onRecheck) onRecheck(entry.operation_id); else onWorkReady(); }}>이 요청 다시 확인</button>}
        {(entry.status === "queued" || entry.status === "sent") && permissionDeniedOperationId === entry.operation_id && <button data-move-retry={entry.operation_id} disabled={busy} onClick={() => void act(entry, "retry")}>이동 다시 시도</button>}
        {(entry.status === "queued" || entry.status === "sent") && <button data-move-cancel={entry.operation_id} disabled={busy} onClick={() => void act(entry, "cancel")}>이동 요청 취소</button>}
      </div>)}</div>}
      {nextEntries !== null && <button disabled={busy} onClick={() => void loadStatus(nextEntries, scope.current)}>다음 이동 요청</button>}
    </>}
    <button disabled={busy} onClick={onClose}>닫기</button>
  </section>;
}
