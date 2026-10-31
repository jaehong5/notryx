import { invoke, isTauri } from "@tauri-apps/api/core";
import { version as engineVersion } from "mermaid/package.json";
import { checkMermaidSource, MERMAID_POLICY_VERSION, sanitizeMermaidSvg } from "./mermaid-policy";

type Render = (source: string, theme: "light" | "dark") => Promise<string>;
type Job = { source: string; theme: "light" | "dark"; signal: AbortSignal; key: string; resolve: (svg: string) => void; reject: (error: unknown) => void };

export class MermaidRenderQueue {
  private active = 0;
  private pending: Job[] = [];
  private cache = new Map<string, string>();
  private cacheBytes = 0;

  constructor(private readonly renderNative: Render = (source, theme) => {
    if (!isTauri()) throw new Error("mermaid_unsupported");
    return invoke("mermaid_render", { source, theme });
  }) {}

  async render(source: string, signal: AbortSignal, theme: "light" | "dark" = "light"): Promise<string> {
    checkMermaidSource(source);
    signal.throwIfAborted();
    const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(source));
    const hash = [...new Uint8Array(digest)].map((value) => value.toString(16).padStart(2, "0")).join("");
    const key = `${engineVersion}:${MERMAID_POLICY_VERSION}:${theme}:${hash}`;
    signal.throwIfAborted();
    const cached = this.cache.get(key);
    if (cached !== undefined) return cached;
    return new Promise((resolve, reject) => {
      const job = { source, theme, signal, key, resolve, reject };
      this.pending.push(job);
      const cancel = () => {
        this.pending = this.pending.filter((item) => item !== job);
        reject(new DOMException("Aborted", "AbortError"));
      };
      signal.addEventListener("abort", cancel, { once: true });
      job.resolve = (svg) => { signal.removeEventListener("abort", cancel); resolve(svg); };
      job.reject = (error) => { signal.removeEventListener("abort", cancel); reject(error); };
      this.drain();
    });
  }

  private drain(): void {
    while (this.active < 2 && this.pending.length) {
      const job = this.pending.shift()!;
      if (job.signal.aborted) continue;
      this.active++;
      void Promise.resolve().then(() => this.renderNative(job.source, job.theme)).then((raw) => {
        job.signal.throwIfAborted();
        const svg = sanitizeMermaidSvg(raw);
        const bytes = svg.length * 2;
        while (this.cache.size && (this.cacheBytes + bytes > 8 * 1024 * 1024 || this.cache.size >= 32)) {
          const first = this.cache.keys().next().value!;
          this.cacheBytes -= this.cache.get(first)!.length * 2;
          this.cache.delete(first);
        }
        this.cacheBytes -= (this.cache.get(job.key)?.length ?? 0) * 2;
        this.cache.set(job.key, svg);
        this.cacheBytes += bytes;
        job.resolve(svg);
      }).catch(job.reject).finally(() => { this.active--; this.drain(); });
    }
  }
}

export const mermaidQueue = new MermaidRenderQueue();
