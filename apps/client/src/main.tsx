import { createRoot } from "react-dom/client";
import { useEffect, useRef, useState } from "react";
import { Compartment, EditorState } from "@codemirror/state";
import { EditorView, keymap, lineNumbers, drawSelection } from "@codemirror/view";
import { defaultKeymap, history, historyKeymap } from "@codemirror/commands";
import { markdown } from "@codemirror/lang-markdown";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { EditorController, initialEditorState } from "./editor-controller";
import { openDocument, saveDocument, closeDocument } from "./ipc";
import "./style.css";

const documents = [
  ["welcome", "시작하기"], ["unicode", "한글과 원문 보존"], ["100k", "100 KiB 문서"],
  ["100k-b", "100 KiB 문서 B"], ["5m", "5 MiB 문서"], ["5m-b", "5 MiB 문서 B"],
  ["mixed", "혼합 줄바꿈"], ["non-utf8", "비UTF-8 원문"],
];

function App() {
  const parent = useRef<HTMLDivElement>(null);
  const controller = useRef<EditorController | null>(null);
  const [state, setState] = useState(initialEditorState);
  const [probeError, setProbeError] = useState("");
  const [isolation, setIsolation] = useState(false);

  useEffect(() => {
    const instance = new EditorController({
      openDocument,
      saveDocument,
      closeDocument,
      exit: () => invoke<void>("m0_exit"),
      stateChanged: setState,
      createEditor: ({ source, change, save }) => {
        const editing = new Compartment();
        const view = new EditorView({
          state: EditorState.create({
            doc: source.text,
            extensions: [
              lineNumbers(), history(), drawSelection(),
              keymap.of([{ key: "Mod-s", run: () => { save(); return true; } }, ...defaultKeymap, ...historyKeymap]),
              editing.of([EditorState.readOnly.of(source.readonly), EditorView.editable.of(!source.readonly)]),
              EditorView.contentAttributes.of({ "aria-label": "Markdown 원문" }),
              EditorView.updateListener.of((update) => {
                if (update.docChanged) change(update.state.doc.toString());
              }),
              ...(source.original.length > 1024 * 1024 ? [] : [markdown()]),
            ],
          }),
        });
        return {
          show: () => {
            if (!parent.current) throw new Error("편집기 화면이 닫혔습니다.");
            parent.current.replaceChildren(view.dom);
          },
          destroy: () => view.destroy(),
          setEditable: (enabled) => view.dispatch({ effects: editing.reconfigure([EditorState.readOnly.of(!enabled), EditorView.editable.of(enabled)]) }),
          isComposing: () => view.composing,
        };
      },
    });
    controller.current = instance;
    void instance.open("welcome");
    const stop = listen("close-requested", () => { void instance.close(); });
    return () => {
      controller.current = null;
      void instance.dispose().catch(() => {});
      void stop.then((unlisten) => unlisten()).catch(() => {});
    };
  }, []);

  return <main>
    <header><strong>Notryx</strong><span className="badge">M0</span><span className="caption">로컬 편집 시험</span></header>
    <div className="layout">
      <nav aria-label="시험 문서">
        <p>합성 문서</p>
        {documents.map(([id, label]) => <button key={id} data-document={id} aria-current={state.selected === id ? "page" : undefined} onClick={() => void controller.current?.open(id)}>{label}</button>)}
        <small>서버 연결과 동기화는 아직 제공하지 않습니다.</small>
        <button id="isolation-open" onClick={() => { setIsolation(true); void invoke("m0_open_isolation").catch((cause) => setProbeError(String(cause))); }}>격리 시험 열기</button>
      </nav>
      <section className="document">
        <div className="toolbar"><span id="save-status" role="status">{state.status}</span><button id="save" onClick={() => void controller.current?.flush().catch(() => {})}>지금 저장</button></div>
        {state.notice && <p className="notice">{state.notice}</p>}
        {(state.error || probeError) && <p role="alert">{state.error || probeError}</p>}
        <div id="editor" ref={parent} />
        {isolation && <iframe id="sandbox-probe" title="격리 시험" sandbox="allow-scripts" src="isolation.html" />}
      </section>
    </div>
  </main>;
}

createRoot(document.getElementById("root")!).render(<App />);
