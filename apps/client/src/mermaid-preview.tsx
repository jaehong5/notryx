import { useEffect, useRef, useState } from "react";
import { mermaidFailureMessage } from "./mermaid-policy";
import { mermaidQueue } from "./mermaid-queue";

export function MermaidPreview({ source, theme = "light" }: { source: string; theme?: "light" | "dark" }) {
  const container = useRef<HTMLElement>(null);
  const [visible, setVisible] = useState(false);
  const [result, setResult] = useState<{ source: string; theme: string; url?: string; error?: string }>();
  useEffect(() => {
    if (typeof IntersectionObserver === "undefined") { setVisible(true); return; }
    const observer = new IntersectionObserver((entries) => {
      if (entries.some((entry) => entry.isIntersecting)) { setVisible(true); observer.disconnect(); }
    });
    if (container.current) observer.observe(container.current);
    return () => observer.disconnect();
  }, []);
  useEffect(() => {
    if (!visible) return;
    const controller = new AbortController();
    let url: string | undefined;
    void mermaidQueue.render(source, controller.signal, theme).then((svg) => {
      if (controller.signal.aborted) return;
      url = URL.createObjectURL(new Blob([svg], { type: "image/svg+xml" }));
      setResult({ source, theme, url });
    }).catch((error: unknown) => {
      if (!controller.signal.aborted) setResult({ source, theme, error: mermaidFailureMessage(error) });
    });
    return () => { controller.abort(); if (url) URL.revokeObjectURL(url); };
  }, [source, visible, theme]);
  const current = result?.source === source && result.theme === theme ? result : undefined;
  return <figure ref={container} className="mermaid-preview">
    {current?.url ? <img src={current.url} alt="Mermaid 다이어그램" onError={() => setResult({ source, theme, error: "다이어그램 이미지를 표시할 수 없어 원문을 표시합니다." })} />
      : <p className="caption">{current?.error ?? (visible ? "다이어그램을 그리는 중…" : "다이어그램 원문")}</p>}
    {current?.url ? <details><summary>Mermaid 원문</summary><pre><code>{source}</code></pre></details>
      : <pre><code>{source}</code></pre>}
  </figure>;
}
