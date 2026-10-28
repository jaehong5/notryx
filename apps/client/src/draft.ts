import { recoverySource, saveSource, type Source } from "./source";

export type Save = (generation: string, bytes: Uint8Array) => Promise<{ edit_session_id: string; generation: string }>;

export class Draft {
  text: string;
  generation: bigint;
  durable: bigint;
  private pending?: Promise<void>;
  private edited = false;
  private attempt = 0;

  constructor(readonly source: Source, readonly session: string, generation: string, private save: Save) {
    this.text = source.text;
    this.generation = BigInt(generation);
    this.durable = this.generation;
  }

  change(text: string) {
    this.text = text;
    this.generation += 1n;
    this.edited = true;
  }

  recoveryBytes(): Uint8Array { return recoverySource(this.source, this.text, this.edited); }

  get dirty() { return this.generation !== this.durable; }
  get saving() { return this.pending !== undefined; }
  get saveAttempt() { return this.attempt; }

  flush(): Promise<void> {
    if (this.pending) return this.pending.then(() => this.flush());
    this.pending = this.drain().finally(() => { this.pending = undefined; });
    return this.pending;
  }

  flushOnce(): Promise<void> {
    if (this.pending) return this.pending;
    this.pending = this.saveOnce().finally(() => { this.pending = undefined; });
    return this.pending;
  }

  private async drain() {
    while (this.dirty) await this.saveOnce();
  }

  private async saveOnce() {
    if (!this.dirty) return;
    this.attempt += 1;
    const generation = this.generation;
    const reply = await this.save(generation.toString(), saveSource(this.source, this.text, true));
    if (reply.edit_session_id !== this.session || reply.generation !== generation.toString()) {
      throw new Error("저장 응답이 현재 편집 요청과 일치하지 않습니다.");
    }
    this.durable = generation;
  }
}
