import { createRoot } from "react-dom/client";
import { useEffect, useRef, useState } from "react";
import { Compartment, EditorState } from "@codemirror/state";
import { EditorView, keymap, lineNumbers, drawSelection } from "@codemirror/view";
import { defaultKeymap, history, historyKeymap } from "@codemirror/commands";
import { markdown } from "@codemirror/lang-markdown";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { openSource } from "./source";
import { Draft } from "./draft";
import { openDocument, saveDocument, closeDocument, fromBase64, toBase64 } from "./ipc";
import "./style.css";

const documents = [
  ["welcome", "시작하기"], ["unicode", "한글과 원문 보존"], ["100k", "100 KiB 문서"],
  ["100k-b", "100 KiB 문서 B"], ["5m", "5 MiB 문서"], ["5m-b", "5 MiB 문서 B"],
  ["mixed", "혼합 줄바꿈"], ["non-utf8", "비UTF-8 원문"],
];

function App() {
  const parent = useRef<HTMLDivElement>(null);
  const editor = useRef<EditorView | null>(null);
  const current = useRef<{ id: string; draft: Draft } | null>(null);
  const timer = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);
  const busy = useRef(false);
  const editing = useRef(new Compartment());
  const [selected, setSelected] = useState("");
  const [status, setStatus] = useState("문서를 여는 중");
  const [error, setError] = useState("");
  const [readonlyReason, setReadonlyReason] = useState("");
  const [isolation, setIsolation] = useState(false);

  async function flush() {
    clearTimeout(timer.current);
    timer.current = undefined;
    const document = current.current;
    if (!document) return;
    if (document.draft.dirty) setStatus("이 기기에 저장 중");
    try {
      await document.draft.flush();
      if (current.current === document) setStatus(document.draft.source.readonly ? "읽기 전용, 원본 보존" : "이 기기에 저장됨");
      setError("");
    } catch (cause) {
      setStatus("저장 오류, 입력 유지 중");
      setError(String(cause));
      throw cause;
    }
  }

  function schedule() {
    if (timer.current !== undefined) return;
    timer.current = setTimeout(() => {
      timer.current = undefined;
      void flush().catch(() => {});
    }, 250);
  }

  async function open(id: string) {
    if (busy.current || id === current.current?.id) return;
    if (editor.current?.composing) { setError("한글 조합을 마친 뒤 문서를 전환해 주세요."); return; }
    busy.current = true;
    setEditable(false);
    try {
      await flush();
      const previous = current.current;
      const response = await openDocument(id);
      let view: EditorView | undefined;
      try {
        const source = openSource(fromBase64(response.bytes_base64));
        const draft = new Draft(source, response.edit_session_id, response.generation, (generation, bytes) =>
          saveDocument({ document_id: id, edit_session_id: response.edit_session_id, generation, bytes_base64: toBase64(bytes) }));
        view = new EditorView({
          state: EditorState.create({
            doc: source.text,
            extensions: [
              lineNumbers(), history(), drawSelection(),
              keymap.of([{ key: "Mod-s", run: () => { void flush().catch(() => {}); return true; } }, ...defaultKeymap, ...historyKeymap]),
              editing.current.of([EditorState.readOnly.of(source.readonly), EditorView.editable.of(!source.readonly)]),
              EditorView.contentAttributes.of({ "aria-label": "Markdown 원문" }),
              EditorView.updateListener.of((update) => {
                if (!update.docChanged) return;
                draft.change(update.state.doc.toString());
                setStatus("저장 대기 중");
                schedule();
              }),
              ...(source.original.length > 1024 * 1024 ? [] : [markdown()]),
            ],
          }),
        });
        if (previous) await closeDocument({ document_id: previous.id, edit_session_id: previous.draft.session });
        editor.current?.destroy();
        parent.current!.replaceChildren(view.dom);
        current.current = { id, draft };
        editor.current = view;
        setSelected(id);
        setReadonlyReason(source.reason ?? "");
        setStatus(source.readonly ? "읽기 전용, 원본 보존" : response.generation === "0" ? "수정 없음" : "이 기기에 저장됨");
        setError("");
      } catch (cause) {
        view?.destroy();
        await closeDocument({ document_id: id, edit_session_id: response.edit_session_id });
        throw cause;
      }
    } catch (cause) {
      setError(String(cause));
    } finally {
      busy.current = false;
      setEditable(!current.current?.draft.source.readonly);
    }
  }

  function setEditable(enabled: boolean) {
    editor.current?.dispatch({ effects: editing.current.reconfigure([EditorState.readOnly.of(!enabled), EditorView.editable.of(enabled)]) });
  }

  useEffect(() => {
    void open("welcome");
    const stop = listen("close-requested", () => {
      if (busy.current) return;
      if (editor.current?.composing) { setError("한글 조합을 마친 뒤 창을 닫아 주세요."); return; }
      busy.current = true;
      setEditable(false);
      void flush().then(async () => {
        const doc = current.current;
        if (doc) await closeDocument({ document_id: doc.id, edit_session_id: doc.draft.session });
        await invoke("m0_exit");
      }).catch((cause) => { setError(String(cause)); }).finally(() => { busy.current = false; setEditable(!current.current?.draft.source.readonly); });
    });
    return () => { clearTimeout(timer.current); editor.current?.destroy(); void stop.then((unlisten) => unlisten()); };
  }, []);

  return <main>
    <header><strong>Notryx</strong><span className="badge">M0</span><span className="caption">로컬 편집 시험</span></header>
    <div className="layout">
      <nav aria-label="시험 문서">
        <p>합성 문서</p>
        {documents.map(([id, label]) => <button key={id} data-document={id} aria-current={selected === id ? "page" : undefined} onClick={() => void open(id)}>{label}</button>)}
        <small>서버 연결과 동기화는 아직 제공하지 않습니다.</small>
        <button id="isolation-open" onClick={() => { setIsolation(true); void invoke("m0_open_isolation").catch((cause) => setError(String(cause))); }}>격리 시험 열기</button>
      </nav>
      <section className="document">
        <div className="toolbar"><span id="save-status" role="status">{status}</span><button id="save" onClick={() => void flush().catch(() => {})}>지금 저장</button></div>
        {readonlyReason && <p className="notice">{readonlyReason}</p>}
        {error && <p role="alert">{error}</p>}
        <div id="editor" ref={parent} />
        {isolation && <iframe id="sandbox-probe" title="격리 시험" sandbox="allow-scripts" src="isolation.html" />}
      </section>
    </div>
  </main>;
}

createRoot(document.getElementById("root")!).render(<App />);
