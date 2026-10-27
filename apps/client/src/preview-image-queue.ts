export class PreviewImageQueue {
  private tail: Promise<void> = Promise.resolve();
  private pending = 0;
  private readonly waiting = new Set<() => void>();

  async load<T>(work: () => Promise<T>, signal: AbortSignal): Promise<T> {
    while (this.pending >= 16) {
      if (signal.aborted) throw new Error("preview_cancelled");
      await new Promise<void>((resolve, reject) => {
        const cleanup = () => {
          this.waiting.delete(ready);
          signal.removeEventListener("abort", cancelled);
        };
        const ready = () => { cleanup(); resolve(); };
        const cancelled = () => { cleanup(); reject(new Error("preview_cancelled")); };
        this.waiting.add(ready);
        signal.addEventListener("abort", cancelled, { once: true });
      });
    }
    if (signal.aborted) {
      if (this.pending < 16) this.waiting.values().next().value?.();
      throw new Error("preview_cancelled");
    }
    this.pending++;
    const task = this.tail.then(() => {
      if (signal.aborted) throw new Error("preview_cancelled");
      return work();
    });
    this.tail = task.then(() => {}, () => {});
    try {
      return await task;
    } finally {
      this.pending--;
      this.waiting.values().next().value?.();
    }
  }
}
