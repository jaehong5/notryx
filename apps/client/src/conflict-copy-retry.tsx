import { useEffect, useRef, useState } from "react";
import type { NodeState, WorkspaceRegistration } from "../../../packages/protocol-types";
import { loadFolderChoices, type FolderChoice } from "./folder-choices";

export function ConflictCopyRetry({ binding, previousOperationId, previousParentId, previousName, loadFolders, busy, onConfirm, onCancel }: {
  binding: WorkspaceRegistration;
  previousOperationId: string;
  previousParentId: string;
  previousName: string;
  loadFolders(binding: WorkspaceRegistration, after: string | null): Promise<{ nodes: NodeState[]; next_after_id: string | null }>;
  busy: boolean;
  onConfirm(parent: string, name: string): Promise<void>;
  onCancel(): void;
}) {
  const request = useRef(0);
  const [folders, setFolders] = useState<FolderChoice[]>([]);
  const [parent, setParent] = useState("");
  const [name, setName] = useState("");
  const [review, setReview] = useState(false);
  const [message, setMessage] = useState("");
  const bindingKey = JSON.stringify(binding);

  async function load() {
    const attempt = ++request.current;
    setFolders([]); setParent(""); setReview(false);
    try {
      const choices = await loadFolderChoices((after) => loadFolders(binding, after), () => request.current === attempt);
      if (request.current !== attempt) return;
      setFolders(choices);
      setParent(""); setReview(false); setMessage("");
    } catch {
      if (request.current === attempt) { setFolders([]); setParent(""); setMessage("폴더 목록을 불러오지 못했습니다."); }
    }
  }
  useEffect(() => { void load(); return () => { request.current++; }; }, [bindingKey, loadFolders]);

  return <div>
    <p>이전 생성 요청 {previousOperationId}은 이름 충돌로 실패했습니다. 기존 원문과 실패 기록은 보존됩니다.</p>
    <label>새 목적지 폴더<select data-testid="conflict-copy-retry-parent" disabled={busy} value={parent} onChange={(event) => { setParent(event.target.value); setReview(false); }}><option value="">폴더 선택</option>{folders.map((folder) => <option key={folder.node_id} value={folder.node_id} disabled={folder.displayPath === null}>{folder.displayPath ?? `[경로 확인 필요] ${folder.name}`}</option>)}</select></label>
    <button type="button" disabled={busy} onClick={() => void load()}>폴더 목록 새로고침</button>
    <label>새 사본 이름<input data-testid="conflict-copy-retry-name" disabled={busy} value={name} onChange={(event) => { setName(event.target.value); setReview(false); }} /></label>
    {message && <p role="status">{message}</p>}
    {!review ? <button type="button" data-testid="conflict-copy-retry-review" disabled={busy || !name || !folders.some((folder) => folder.node_id === parent && folder.displayPath !== null)} onClick={() => {
      if (!name.toLowerCase().endsWith(".md")) { setMessage("문서 이름은 .md로 끝나야 합니다."); return; }
      if (parent === previousParentId && name === previousName) { setMessage("이전과 다른 폴더나 이름을 선택해 주세요."); return; }
      setMessage(""); setReview(true);
    }}>새 대상 확인</button> : <>
      <p>이전에 확인한 초안을 {folders.find((folder) => folder.node_id === parent)?.displayPath} 폴더에 {name}으로 보존합니다. 이전 실패와 원래 충돌은 유지됩니다.</p>
      <button type="button" data-testid="conflict-copy-retry-confirm" disabled={busy} onClick={() => { if (folders.some((folder) => folder.node_id === parent && folder.displayPath !== null) && name.toLowerCase().endsWith(".md") && (parent !== previousParentId || name !== previousName)) void onConfirm(parent, name); }}>새 대상 보존 확인</button>
    </>}
    <button type="button" data-testid="conflict-copy-retry-cancel" disabled={busy} onClick={onCancel}>취소</button>
  </div>;
}
