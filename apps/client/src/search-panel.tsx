import { useEffect, useRef, useState, type FormEvent } from "react";
import type { WorkspaceRegistration } from "../../../packages/protocol-types";

export type SearchScope = "workspace" | "recovery";
export type SearchResult = {
  node_id: string;
  scope: SearchScope;
  source: "draft" | "cache";
};
export type SearchPage = {
  results: SearchResult[];
  available_documents: number;
  total_documents: number;
  recovery_available_documents: number;
  recovery_total_documents: number;
  next_after_id: string | null;
};
export type SearchPanelPorts = {
  scan(binding: WorkspaceRegistration, request: { query: string; scope: SearchScope; after: string | null; request_id: string }): Promise<SearchPage>;
  cancel(binding: WorkspaceRegistration, requestId: string): Promise<void>;
};

let nextRequestId = 0;

export function advanceSearchRequestId(current: number): number | null {
  if (!Number.isSafeInteger(current) || current < 0 || current >= Number.MAX_SAFE_INTEGER) return null;
  return current + 1;
}

export function SearchPanel({ binding, ports, open, label, revision = 0 }: {
  binding: WorkspaceRegistration;
  ports: SearchPanelPorts;
  open(result: SearchResult): void;
  label?(result: SearchResult): string;
  revision?: number;
}) {
  const bindingKey = JSON.stringify(binding);
  const attempt = useRef(0);
  const active = useRef<{ id: string; binding: WorkspaceRegistration } | null>(null);
  const [query, setQuery] = useState("");
  const [scope, setScope] = useState<SearchScope | "">("");
  const [page, setPage] = useState<SearchPage | null>(null);
  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState("");

  function invalidate() {
    attempt.current++;
    const previous = active.current;
    active.current = null;
    if (previous) void ports.cancel(previous.binding, previous.id).catch(() => {});
    setPage(null);
    setBusy(false);
  }

  useEffect(() => {
    invalidate();
    setMessage("");
    return () => {
      attempt.current++;
      const previous = active.current;
      active.current = null;
      if (previous) void ports.cancel(previous.binding, previous.id).catch(() => {});
    };
  }, [bindingKey, ports, revision]);

  useEffect(() => {
    setQuery("");
    setScope("");
  }, [bindingKey]);

  async function load(after: string | null, continuation: boolean) {
    if (!query || !scope || (continuation && (!active.current || busy))) return;
    if (!continuation) invalidate();
    let current = active.current;
    if (!continuation) {
      const next = advanceSearchRequestId(nextRequestId);
      if (next === null) { setMessage("검색 요청 한도에 도달했습니다. 앱을 다시 열어 주세요."); return; }
      nextRequestId = next;
      current = { id: String(next), binding };
    }
    if (!current) return;
    active.current = current;
    const currentAttempt = ++attempt.current;
    setBusy(true);
    setMessage("");
    try {
      const result = await ports.scan(binding, { query, scope, after, request_id: current.id });
      if (attempt.current !== currentAttempt || active.current !== current) return;
      setPage(result);
      if (!result.results.length) setMessage(result.next_after_id ? "이 구간에 일치하는 문서가 없습니다. 다음 구간을 확인할 수 있습니다." : "이 구간에 일치하는 문서가 없습니다.");
    } catch {
      if (attempt.current !== currentAttempt || active.current !== current) return;
      active.current = null;
      setPage(null);
      setMessage("검색을 완료하지 못했습니다. 검색을 다시 시작하세요.");
    } finally {
      if (attempt.current === currentAttempt) setBusy(false);
    }
  }

  function submit(event: FormEvent) {
    event.preventDefault();
    void load(null, false);
  }

  const available = scope === "recovery" ? page?.recovery_available_documents : page?.available_documents;
  const total = scope === "recovery" ? page?.recovery_total_documents : page?.total_documents;
  return <section aria-label="본문 검색" className="search-panel">
    <h2>본문 검색</h2>
    <form onSubmit={submit}>
      <label>검색어<input value={query} onChange={(event) => { invalidate(); setMessage(""); setQuery(event.target.value); }} /></label>
      <label>검색 범위<select value={scope} onChange={(event) => { invalidate(); setMessage(""); setScope(event.target.value as SearchScope | ""); }}>
        <option value="">범위 선택</option><option value="workspace">작업 공간</option><option value="recovery">복구 초안</option>
      </select></label>
      <button type="submit" disabled={!query || !scope}>검색</button>
      <button type="button" disabled={!active.current} onClick={() => { invalidate(); setMessage("검색을 취소했습니다."); }}>검색 취소</button>
    </form>
    <p>이 기기에 저장된 문서와 초안의 본문을 검색합니다.</p>
    {busy && <p role="status">검색 중</p>}
    {message && <p role="status">{message}</p>}
    {page && <div>
      <p>이번 구간에서 검색 가능한 문서: {available} / {total}</p>
      {page.results.map((result) => <button type="button" key={`${result.scope}:${result.node_id}`} data-search-node={result.node_id} onClick={() => open(result)}>
        {label?.(result) || result.node_id} {result.source === "draft" ? "(기기 초안)" : "(저장된 본문)"}
      </button>)}
      {page.next_after_id !== null && <button type="button" disabled={busy} onClick={() => void load(page.next_after_id, true)}>다음 결과</button>}
    </div>}
  </section>;
}
