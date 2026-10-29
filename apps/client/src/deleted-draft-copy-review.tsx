import { useEffect, useMemo, useRef, useState } from "react";
import type { NodeState, WorkspaceDeletedDraftCopySnapshot, WorkspaceRegistration } from "../../../packages/protocol-types";
import { loadFolderChoices, type FolderChoice } from "./folder-choices";

export function DeletedDraftCopyReview({ snapshot, localBytes, loadFolders, onConfirm, onCancel }: {
  snapshot: WorkspaceDeletedDraftCopySnapshot;
  localBytes: Uint8Array;
  loadFolders(binding: WorkspaceRegistration, after: string | null): Promise<{ nodes: NodeState[]; next_after_id: string | null }>;
  onConfirm(snapshot: WorkspaceDeletedDraftCopySnapshot, parentId: string, name: string): Promise<void>;
  onCancel(): void;
}) {
  const [folders, setFolders] = useState<FolderChoice[]>([]);
  const [parent, setParent] = useState("");
  const [name, setName] = useState("");
  const [review, setReview] = useState(false);
  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState("");
  const folderRequest = useRef(0);
  const busyRef = useRef(false);
  const bindingKey = JSON.stringify(snapshot.binding);
  const snapshotKey = JSON.stringify([snapshot.document_id, snapshot.edit_session_id, snapshot.generation, snapshot.local_hash, snapshot.tombstone_seq]);

  async function load() {
    const attempt = ++folderRequest.current;
    setFolders([]); setParent(""); setReview(false);
    try {
      const choices = await loadFolderChoices((after) => loadFolders(snapshot.binding, after), () => folderRequest.current === attempt);
      if (folderRequest.current !== attempt) return;
      setFolders(choices);
      setParent(""); setReview(false); setMessage("");
    } catch {
      if (folderRequest.current === attempt) { setFolders([]); setParent(""); setMessage("폴더 목록을 불러오지 못했습니다."); }
    }
  }
  useEffect(() => { setName(""); setReview(false); void load(); return () => { folderRequest.current++; }; }, [bindingKey, snapshotKey, loadFolders]);

  const source = useMemo((): { text: string; bom: boolean; endings: string } | null => {
    try {
    if (!(localBytes instanceof Uint8Array) || localBytes.byteLength > 5 * 1024 * 1024) throw new Error("invalid_source");
    const text = new TextDecoder("utf-8", { fatal: true }).decode(localBytes);
    const endings = new Set<string>();
    for (let index = 0; index < text.length; index++) {
      if (text[index] === "\r" && text[index + 1] === "\n") { endings.add("CRLF"); index++; }
      else if (text[index] === "\r") endings.add("단독 CR");
      else if (text[index] === "\n") endings.add("LF");
    }
    return { text, bom: localBytes.length >= 3 && localBytes[0] === 0xef && localBytes[1] === 0xbb && localBytes[2] === 0xbf,
      endings: endings.size ? [...endings].join(", ") : "없음" };
    } catch {
      return null;
    }
  }, [localBytes]);

  return <section aria-label="삭제된 문서 초안 사본">
    <h2>삭제된 문서의 초안 보존</h2>
    <p>초안을 새 문서로 보존합니다. 원래 문서는 삭제된 상태로 남으며 사본 생성 중 빈 문서가 잠시 보일 수 있습니다.</p>
    {!source ? <p role="status">이 초안을 확인할 수 없습니다. 기존 원문은 보존됩니다.</p> : <>
      <label>보존할 기기 초안 (generation {snapshot.generation}{source.bom ? ", BOM 포함" : ""}, 줄바꿈: {source.endings})<textarea aria-label="보존할 기기 초안" readOnly value={source.text} /></label>
      <label>목적지 폴더<select data-testid="deleted-copy-parent" value={parent} disabled={busy} onChange={(event) => { setParent(event.target.value); setReview(false); }}><option value="">폴더 선택</option>{folders.map((folder) => <option key={folder.node_id} value={folder.node_id} disabled={folder.displayPath === null}>{folder.displayPath ?? `[경로 확인 필요] ${folder.name}`}</option>)}</select></label>
      <button type="button" disabled={busy} onClick={() => void load()}>폴더 목록 새로고침</button>
      <label>사본 이름<input data-testid="deleted-copy-name" disabled={busy} value={name} onChange={(event) => { setName(event.target.value); setReview(false); }} /></label>
      {message && <p role="status">{message}</p>}
      {!review ? <button type="button" data-testid="deleted-copy-review" disabled={busy || !name || !folders.some((folder) => folder.node_id === parent && folder.displayPath !== null)} onClick={() => {
        if (!name.toLowerCase().endsWith(".md")) { setMessage("문서 이름은 .md로 끝나야 합니다."); return; }
        setMessage(""); setReview(true);
      }}>초안과 대상 확인</button> : <>
        <p>표시한 초안을 {folders.find((folder) => folder.node_id === parent)?.displayPath} 폴더에 {name}으로 보존합니다.</p>
        <button type="button" data-testid="deleted-copy-confirm" disabled={busy} onClick={() => {
          if (busyRef.current || !folders.some((folder) => folder.node_id === parent && folder.displayPath !== null) || !name.toLowerCase().endsWith(".md")) return;
          busyRef.current = true; setBusy(true);
          void onConfirm(snapshot, parent, name).finally(() => { busyRef.current = false; setBusy(false); });
        }}>사본 보존 확인</button>
      </>}
    </>}
    <button type="button" data-testid="deleted-copy-cancel" disabled={busy} onClick={onCancel}>취소</button>
  </section>;
}
