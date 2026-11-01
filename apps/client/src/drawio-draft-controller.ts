export type DiagramDraftState = "pending" | "saving" | "saved" | "error";
type Dependencies = {
  save: (request: { generation: string; xml: string }) => Promise<{ generation: string }>;
  close: () => Promise<void>;
  status: (state: DiagramDraftState) => void;
};

export class DiagramDraftController {
  private pending?: { xml: string };
  private invalid = false;
  private disposed = false;
  private due = 0;
  private timer?: ReturnType<typeof setTimeout>;
  private inFlight?: Promise<void>;

  constructor(private generation: string, private readonly dependencies: Dependencies) {}

  change(xml: string): void {
    if (this.disposed) return;
    this.pending = { xml };
    this.invalid = false;
    this.due = Date.now() + 1000;
    this.dependencies.status("pending");
    this.schedule();
  }

  reject(): void {
    this.invalid = true;
    this.pending = undefined;
    this.clearTimer();
    this.dependencies.status("error");
  }

  async close(): Promise<void> {
    await this.flush();
    await this.dependencies.close();
  }

  async flush(): Promise<string> {
    this.clearTimer();
    if (this.disposed || this.invalid) throw new Error("invalid diagram draft");
    while (this.pending || this.inFlight) {
      await (this.inFlight ?? this.saveOne());
      this.clearTimer();
      if (this.invalid) throw new Error("invalid diagram draft");
    }
    return this.generation;
  }

  dispose(): void { this.disposed = true; this.clearTimer(); }

  private clearTimer(): void {
    if (this.timer !== undefined) clearTimeout(this.timer);
    this.timer = undefined;
  }

  private schedule(): void {
    this.clearTimer();
    if (!this.pending || this.invalid || this.disposed) return;
    this.timer = setTimeout(() => {
      this.timer = undefined;
      if (!this.inFlight) void this.saveOne().catch(() => {});
    }, Math.max(0, this.due - Date.now()));
  }

  private saveOne(): Promise<void> {
    if (this.inFlight) return this.inFlight;
    const pending = this.pending;
    if (!pending || this.invalid || this.disposed) return Promise.resolve();
    let failed = false;
    this.dependencies.status("saving");
    this.inFlight = this.dependencies.save({ generation: this.generation, xml: pending.xml }).then(result => {
      if (!/^[1-9][0-9]*$/u.test(result.generation) || BigInt(result.generation) !== BigInt(this.generation) + 1n) {
        throw new Error("invalid checkpoint acknowledgement");
      }
      this.generation = result.generation;
      if (this.pending === pending) this.pending = undefined;
      if (!this.disposed) this.dependencies.status(this.invalid ? "error" : this.pending ? "pending" : "saved");
    }).catch((error: unknown) => {
      failed = true;
      this.clearTimer();
      if (!this.disposed) this.dependencies.status("error");
      throw error;
    }).finally(() => {
      this.inFlight = undefined;
      if (!failed) this.schedule();
    });
    return this.inFlight;
  }
}
