import { useEffect, useRef, useState } from "react";
import type { NodeState, WorkspaceRegistration } from "../../../packages/protocol-types";
import { loadFolderChoices, type FolderChoice } from "./folder-choices";

export type ConflictReviewSnapshot = {
  binding: WorkspaceRegistration;
  node_id: string;
  operation_id: string;
  operation_seq: string;
  generation: string;
  base_revision: string;
  base_bytes: Uint8Array;
  base_hash: string;
  local_bytes: Uint8Array;
  local_hash: string;
  conflict_revision: string;
  remote_revision: string;
  remote_bytes: Uint8Array;
  remote_hash: string;
};
export type ConflictReviewPorts = {
  load(binding: WorkspaceRegistration, documentId: string, sessionId: string, generation: string, operationId: string): Promise<ConflictReviewSnapshot>;
};
export type ConflictReviewProps = {
  binding: WorkspaceRegistration;
  documentId: string;
  sessionId: string;
  generation: string;
  operationId: string;
  ports: ConflictReviewPorts;
  onClose(): void;
  onChooseRemote?(snapshot: ConflictReviewSnapshot): Promise<void>;
  onKeepCopy?(snapshot: ConflictReviewSnapshot, parentId: string, name: string): Promise<void>;
  onSaveMerged?(snapshot: ConflictReviewSnapshot): Promise<void>;
  loadFolders?(binding: WorkspaceRegistration, after: string | null): Promise<{ nodes: NodeState[]; next_after_id: string | null }>;
};

type SourceView = { text: string; bom: boolean; lineEnding: string };
type View = { base: SourceView; local: SourceView; remote: SourceView };
type Result = { key: string; ports: ConflictReviewPorts; view: View | null; snapshot?: ConflictReviewSnapshot };

function sameBinding(a: WorkspaceRegistration, b: WorkspaceRegistration): boolean {
  return a.server_origin === b.server_origin && a.workspace_id === b.workspace_id && a.workspace_epoch === b.workspace_epoch && a.device_id === b.device_id && a.device_stream_id === b.device_stream_id;
}

function decode(bytes: Uint8Array): SourceView {
  if (!ArrayBuffer.isView(bytes) || Object.prototype.toString.call(bytes) !== "[object Uint8Array]" || bytes.byteLength > 5 * 1024 * 1024) throw new Error("invalid_content");
  const text = new TextDecoder("utf-8", { fatal: true }).decode(bytes);
  let lf = false, crlf = false, cr = false;
  for (let index = 0; index < text.length; index++) {
    if (text[index] === "\r") {
      if (text[index + 1] === "\n") { crlf = true; index++; }
      else cr = true;
    } else if (text[index] === "\n") lf = true;
  }
  const endings = [lf && "LF", crlf && "CRLF", cr && "단독 CR"].filter(Boolean);
  return { text, bom: bytes.length >= 3 && bytes[0] === 0xef && bytes[1] === 0xbb && bytes[2] === 0xbf,
    lineEnding: endings.length === 0 ? "없음" : endings.length === 1 ? String(endings[0]) : `혼합 (${endings.join(", ")})` };
}

export function ConflictReview({ binding, documentId, sessionId, generation, operationId, ports, onClose, onChooseRemote, onKeepCopy, onSaveMerged, loadFolders }: ConflictReviewProps) {
  const key = JSON.stringify([binding.server_origin, binding.workspace_id, binding.workspace_epoch, binding.device_id, binding.device_stream_id, documentId, sessionId, generation, operationId]);
  const [result, setResult] = useState<Result | null>(null);
  const [confirming, setConfirming] = useState(false);
  const [choosing, setChoosing] = useState(false);
  const choosingRef = useRef(false);
  const folderRequest = useRef(0);
  const [copyOpen, setCopyOpen] = useState(false);
  const [copyReview, setCopyReview] = useState(false);
  const [copyBusy, setCopyBusy] = useState(false);
  const copyBusyRef = useRef(false);
  const [folders, setFolders] = useState<FolderChoice[]>([]);
  const [parent, setParent] = useState("");
  const [name, setName] = useState("");
  const [folderError, setFolderError] = useState("");
  const [mergeReview, setMergeReview] = useState<{ key: string; ports: ConflictReviewPorts; snapshot: ConflictReviewSnapshot } | null>(null);
  const [mergeBusy, setMergeBusy] = useState(false);
  const mergeBusyRef = useRef(false);
  useEffect(() => () => { folderRequest.current++; }, [key]);
  async function showFolders() {
    if (!loadFolders) return;
    const attempt = ++folderRequest.current;
    setFolders([]); setParent(""); setCopyReview(false);
    try {
      const choices = await loadFolderChoices((after) => loadFolders(binding, after), () => folderRequest.current === attempt);
      if (folderRequest.current !== attempt) return;
      setFolders(choices);
      setParent(""); setCopyReview(false); setFolderError("");
    } catch {
      if (folderRequest.current === attempt) { setFolders([]); setParent(""); setFolderError("폴더 목록을 불러오지 못했습니다."); }
    }
  }
  useEffect(() => {
    let active = true;
    void ports.load(binding, documentId, sessionId, generation, operationId).then((snapshot) => {
      if (!active) return;
      if (!sameBinding(snapshot.binding, binding) || snapshot.node_id !== documentId || snapshot.operation_id !== operationId || snapshot.generation !== generation || !snapshot.base_revision || !snapshot.conflict_revision || !snapshot.remote_revision) throw new Error("stale_conflict");
      const base = decode(snapshot.base_bytes), local = decode(snapshot.local_bytes), remote = decode(snapshot.remote_bytes);
      if (active) setResult({ key, ports, snapshot, view: { base, local, remote } });
    }).catch(() => { if (active) setResult({ key, ports, view:null }); });
    return () => { active = false; };
  }, [key, ports]);
  const current = result?.key === key && result.ports === ports ? result : null;
  const view = current?.view;
  const currentMergeReview = mergeReview?.key === key && mergeReview.ports === ports && mergeReview.snapshot === current?.snapshot ? mergeReview : null;
  return <section aria-label="충돌 원문 비교">
    <h2>충돌 원문 비교</h2>
    <p>서버 본문은 충돌이 발생한 뒤 달라졌을 수 있습니다. 이 기기에 저장된 초안은 비교를 준비한 시점의 내용이며 이후 입력이 있을 수 있습니다.</p>
    {!current ? <p role="status">비교 자료를 불러오는 중입니다.</p> : !view ? <p role="status">비교 자료를 불러오지 못했습니다. 현재 편집과 보존된 초안은 유지됩니다.</p> : <>
      <label>기준 원문 ({view.base.bom ? "BOM 포함, " : ""}줄바꿈: {view.base.lineEnding})<textarea aria-label="기준 원문" readOnly value={view.base.text} /></label>
      <label>이 기기에 저장된 내 초안 ({view.local.bom ? "BOM 포함, " : ""}줄바꿈: {view.local.lineEnding})<textarea aria-label="이 기기에 저장된 내 초안" readOnly value={view.local.text} /></label>
      <label>조회 당시 서버 본문 ({view.remote.bom ? "BOM 포함, " : ""}줄바꿈: {view.remote.lineEnding})<textarea aria-label="조회 당시 서버 본문" readOnly value={view.remote.text} /></label>
      {onSaveMerged && current?.snapshot && <>
        <p>기존 편집기에서 합본을 작성한 뒤 충돌을 다시 비교하고, 저장된 기기 초안을 합본으로 선택하세요.</p>
        {current.snapshot.local_bytes.includes(13) ? <>
          <p role="status">기기 초안에 CR 줄바꿈이 있습니다. 편집기에서 내용을 수정하면 LF로 저장됩니다. 저장 후 다시 비교해 주세요.</p>
          <button type="button" data-testid="conflict-merge-review" disabled>기기 초안을 합본으로 확인</button>
        </> : !currentMergeReview ? <button type="button" data-testid="conflict-merge-review" disabled={mergeBusy} onClick={() => {
          setMergeReview({ key, ports, snapshot:current.snapshot! });
        }}>기기 초안을 합본으로 확인</button> : <>
          <p>표시한 기기 초안을 합본으로 서버에 저장합니다. 확인한 서버 본문이 바뀌면 다시 비교해야 합니다. 서버 저장을 확인하기 전까지 완료가 아닙니다.</p>
          <button type="button" data-testid="conflict-merge-confirm" disabled={mergeBusy} onClick={() => {
            if (mergeBusyRef.current || !onSaveMerged || currentMergeReview.snapshot !== current.snapshot) return;
            mergeBusyRef.current = true; setMergeBusy(true);
            void onSaveMerged(currentMergeReview.snapshot).finally(() => { mergeBusyRef.current = false; setMergeBusy(false); });
          }}>합본 저장 확인</button>
          <button type="button" disabled={mergeBusy} onClick={() => setMergeReview(null)}>취소</button>
        </>}
      </>}
      {onKeepCopy && loadFolders && current?.snapshot && <>
        {!copyOpen ? <button type="button" data-testid="conflict-copy-open" onClick={() => { setCopyOpen(true); void showFolders(); }}>내 초안을 사본으로 저장</button> : <>
          <label>목적지 폴더<select data-testid="conflict-copy-parent" value={parent} disabled={copyBusy} onChange={(event) => { setParent(event.target.value); setCopyReview(false); }}><option value="">폴더 선택</option>{folders.map((folder) => <option key={folder.node_id} value={folder.node_id} disabled={folder.displayPath === null}>{folder.displayPath ?? `[경로 확인 필요] ${folder.name}`}</option>)}</select></label>
          <button type="button" disabled={copyBusy} onClick={() => void showFolders()}>폴더 목록 새로고침</button>
          <label>사본 이름<input data-testid="conflict-copy-name" value={name} disabled={copyBusy} onChange={(event) => { setName(event.target.value); setCopyReview(false); }} /></label>
          {folderError && <p role="status">{folderError}</p>}
          {!copyReview ? <button type="button" data-testid="conflict-copy-review" disabled={copyBusy || !name || !folders.some((folder) => folder.node_id === parent && folder.displayPath !== null)} onClick={() => { if (!name.toLowerCase().endsWith(".md")) { setFolderError("문서 이름은 .md로 끝나야 합니다."); return; } setFolderError(""); setCopyReview(true); }}>사본 내용과 대상 확인</button> : <>
            <p>표시한 내 초안을 {folders.find((folder) => folder.node_id === parent)?.displayPath} 폴더에 {name}으로 보존합니다. 생성 중 빈 문서가 잠시 보일 수 있으며 원래 문서의 충돌은 남습니다.</p>
            <button type="button" data-testid="conflict-copy-confirm" disabled={copyBusy} onClick={() => {
              if (copyBusyRef.current || !current.snapshot || !onKeepCopy || !folders.some((folder) => folder.node_id === parent && folder.displayPath !== null) || !name.toLowerCase().endsWith(".md")) return;
              copyBusyRef.current = true; setCopyBusy(true);
              void onKeepCopy(current.snapshot, parent, name).finally(() => { copyBusyRef.current = false; setCopyBusy(false); });
            }}>사본 보존 확인</button>
          </>}
          <button type="button" data-testid="conflict-copy-cancel" disabled={copyBusy} onClick={() => { folderRequest.current++; setCopyOpen(false); setCopyReview(false); setParent(""); setName(""); setFolderError(""); }}>취소</button>
        </>}
      </>}
      {onChooseRemote && current?.snapshot && <>
        {!confirming ? <button type="button" data-testid="conflict-choose-remote" onClick={() => setConfirming(true)}>서버 본문 선택</button> : <>
          <p>확인한 서버 본문을 선택합니다. 기존 초안과 충돌 자료는 보존됩니다. 후속 작업이 남으면 선택이 보류되며, 처리 버튼으로 직접 이어갈 수 있습니다.</p>
          <button type="button" data-testid="conflict-choose-remote-confirm" disabled={choosing} onClick={() => {
            if (choosingRef.current || !current.snapshot || !onChooseRemote) return;
            choosingRef.current = true; setChoosing(true);
            void onChooseRemote(current.snapshot).finally(() => { choosingRef.current = false; setChoosing(false); });
          }}>서버 본문으로 선택 확인</button>
          <button type="button" disabled={choosing} onClick={() => setConfirming(false)}>취소</button>
        </>}
      </>}
    </>}
    <button type="button" onClick={onClose}>닫기</button>
  </section>;
}
