export type TransmitScope = {
  binding: string;
  documentId: string;
  editSessionId: string;
};

export type QueuedOperation = {
  operationId: string;
  documentId: string;
  generation: string;
  committed?: boolean;
};

export type TransmitResult =
  | { status: "committed" | "conflict" | "failed" | "abandoned"; operationId: string }
  | { status: "ack_pending" | "auth_required" | "gateway_auth_required"; operationId?: string }
  | { status: "idle" | "pending" | "offline" | "needs_review" };

export type AuthRequiredReason = "auth_required" | "gateway_auth_required";

export class QueueStorageError extends Error {
  constructor(cause: unknown) {
    super("Storage", { cause });
  }
}

export type AutoTransmitPorts = {
  queue(scope: TransmitScope, generation: string): Promise<QueuedOperation>;
  transmit(): Promise<TransmitResult>;
  acknowledged(scope: TransmitScope, generation: string, current: boolean): void;
  authRequired(reason: AuthRequiredReason): void;
  failed(error: unknown): void;
  queueStorageFailed?(scope: TransmitScope): void;
  queueRecovered?(scope: TransmitScope): void;
  receiptConfirmed?(operationId: string): void;
};

type DocumentState = {
  scope: TransmitScope;
  latest: bigint;
  durable: bigint;
  queued: bigint;
  firstPendingAt?: number;
  lastEditAt?: number;
  composing: boolean;
  awaitingComposition: boolean;
  active: boolean;
  closing: boolean;
  restored: boolean;
  timer?: ReturnType<typeof setTimeout>;
  pending?: Promise<void>;
  pendingTarget?: bigint;
  queueRetryAt?: number;
  queueRetryDelay: number;
};

function number(value: string): bigint {
  if (!/^(0|[1-9][0-9]*)$/.test(value)) throw new Error("invalid_generation");
  const result = BigInt(value);
  if (result > 9223372036854775807n) throw new Error("invalid_generation");
  return result;
}

function key(scope: TransmitScope): string {
  if (!scope.binding || !scope.documentId || !scope.editSessionId) throw new Error("invalid_scope");
  return JSON.stringify([scope.binding, scope.documentId, scope.editSessionId]);
}

function staleCheckpoint(error: unknown): boolean {
  return error === "stale_checkpoint" || (error instanceof Error && error.message === "stale_checkpoint");
}

function retryableQueueError(error: unknown): boolean {
  return error === "Storage" || (error instanceof Error && error.message === "Storage");
}

export class AutoTransmit {
  private documents = new Map<string, DocumentState>();
  private operations = new Map<string, { key: string; binding: string; generation: string; documentId: string; awaitingIdle: boolean }>();
  private queueTail: Promise<void> = Promise.resolve();
  private transmitting = false;
  private transmitPending = false;
  private retryTimer?: ReturnType<typeof setTimeout>;
  private retryDelay = 1000;
  private requiresReview = false;
  private stopped = false;
  private activeBinding?: string;

  constructor(private ports: AutoTransmitPorts) {}

  openSession(scope: TransmitScope): void {
    if (this.stopped) throw new Error("scheduler_stopped");
    if (this.activeBinding !== undefined && scope.binding !== this.activeBinding) throw new Error("stale_registration");
    const identity = key(scope);
    if (this.documents.has(identity)) throw new Error("session_already_initialized");
    this.documents.set(identity, { scope: { ...scope }, latest: 0n, durable: 0n, queued: 0n, composing: false, awaitingComposition: false, active: true, closing: false, restored: false, queueRetryDelay: 1000 });
  }

  activateBinding(binding: string): void {
    if (this.stopped) throw new Error("scheduler_stopped");
    if (!binding) throw new Error("invalid_scope");
    if (this.activeBinding === binding) return;
    const previous = this.activeBinding;
    this.activeBinding = binding;
    if (previous !== undefined) {
      for (const [identity, state] of this.documents) {
        if (!state.active && state.scope.binding === previous) this.documents.delete(identity);
      }
      for (const [id, operation] of this.operations) {
        if (operation.binding === previous) this.operations.delete(id);
      }
      clearTimeout(this.retryTimer);
      this.retryTimer = undefined;
      this.retryDelay = 1000;
      this.requiresReview = false;
      this.transmitPending = true;
      this.startTransmission();
    }
  }

  async restore(scope: TransmitScope, generation: string): Promise<void> {
    const state = this.document(scope);
    const restored = number(generation);
    if (state.restored || state.latest !== 0n || state.durable !== 0n || state.queued !== 0n || state.pending) throw new Error("session_already_initialized");
    state.restored = true;
    state.latest = restored;
    state.durable = restored;
    if (restored === 0n) return;
    state.firstPendingAt = Date.now();
    state.lastEditAt = Date.now();
    try {
      await this.enqueue(state);
    } catch (error) {
      if (!staleCheckpoint(error)) throw error;
    }
  }

  edit(scope: TransmitScope, generation: string): void {
    const state = this.document(scope);
    const next = number(generation);
    if (next <= state.latest) throw new Error("stale_generation");
    state.latest = next;
    state.firstPendingAt ??= Date.now();
    state.lastEditAt = Date.now();
    this.schedule(state);
  }

  durable(scope: TransmitScope, generation: string): void {
    const state = this.document(scope);
    const next = number(generation);
    if (next < state.durable || next > state.latest) throw new Error("invalid_durable_generation");
    state.durable = next;
    this.schedule(state);
    this.startTransmission();
  }

  composition(scope: TransmitScope, active: boolean): void {
    const state = this.document(scope);
    if (active) state.awaitingComposition = true;
    state.composing = active;
    this.schedule(state);
    if (!active) this.startTransmission();
  }

  acknowledge(scope: TransmitScope, receipt: QueuedOperation, durableConfirmed: boolean): void {
    if (this.activeBinding !== undefined && scope.binding !== this.activeBinding) return;
    const identity = key(scope);
    const operation = this.operations.get(receipt.operationId);
    if (!operation || operation.key !== identity || operation.documentId !== receipt.documentId ||
      operation.generation !== receipt.generation) return;
    const state = this.documents.get(identity);
    const generation = number(receipt.generation);
    const current = durableConfirmed && !this.stopped && !!state && state.active && !state.closing && !state.composing && state.latest === generation && state.durable === generation && state.queued >= generation;
    this.ports.acknowledged({ ...scope }, receipt.generation, current);
    if (durableConfirmed) this.operations.delete(receipt.operationId);
  }

  async flushDocument(scope: TransmitScope): Promise<void> {
    const state = this.documents.get(key(scope));
    if (!state) return;
    if (state.composing) throw new Error("composition_in_progress");
    clearTimeout(state.timer);
    state.timer = undefined;
    if (state.latest !== state.durable) throw new Error("checkpoint_pending");
    while (state.durable > state.queued) {
      const target = state.pendingTarget ?? state.durable;
      try {
        await this.enqueue(state);
      } catch (error) {
        if (!staleCheckpoint(error) || state.composing || state.latest !== state.durable || state.durable <= target) throw error;
        clearTimeout(state.timer);
        state.timer = undefined;
        continue;
      }
      if (state.composing) throw new Error("composition_in_progress");
      if (state.latest !== state.durable) throw new Error("checkpoint_pending");
    }
  }

  async deactivate(scope: TransmitScope, close?: () => Promise<void>): Promise<void> {
    const state = this.documents.get(key(scope));
    if (!state) { await close?.(); return; }
    if (!state.active) return;
    if (state.composing) throw new Error("composition_in_progress");
    await this.flushDocument(scope);
    await this.closeSession(state, close);
  }

  async deactivatePreservingDraft(scope: TransmitScope, close: () => Promise<void>): Promise<void> {
    const state = this.documents.get(key(scope));
    if (!state) { await close(); return; }
    if (!state.active) return;
    if (state.composing) throw new Error("composition_in_progress");
    if (state.latest !== state.durable || state.pending) throw new Error("checkpoint_pending");
    await this.closeSession(state, close);
  }

  resume(): void {
    if (this.stopped) return;
    clearTimeout(this.retryTimer);
    this.retryTimer = undefined;
    this.retryDelay = 1000;
    this.requiresReview = false;
    this.transmitPending = true;
    this.startTransmission();
  }

  kick(): void {
    if (this.stopped) return;
    this.transmitPending = true;
    this.startTransmission();
  }

  async syncNow(scope: TransmitScope): Promise<void> {
    if (this.stopped) throw new Error("scheduler_stopped");
    if (this.activeBinding !== undefined && scope.binding !== this.activeBinding) throw new Error("stale_registration");
    const state = this.documents.get(key(scope));
    const previousQueued = state?.queued;
    await this.flushDocument(scope);
    if (this.activeBinding !== undefined && scope.binding !== this.activeBinding) throw new Error("stale_registration");
    if (state && !state.active) throw new Error("stale_session");
    if (this.requiresReview || this.stopped) return;
    clearTimeout(this.retryTimer);
    this.retryTimer = undefined;
    this.retryDelay = 1000;
    if (!this.transmitting && !this.transmitPending && state && previousQueued !== undefined && state.queued > previousQueued) return;
    if (!this.transmitting) {
      this.transmitPending = true;
      this.startTransmission();
    }
  }

  async dispose(): Promise<void> {
    if (this.stopped) return;
    for (const state of this.documents.values()) if (state.active) await this.flushDocument(state.scope);
    this.stopped = true;
    clearTimeout(this.retryTimer);
    for (const state of this.documents.values()) clearTimeout(state.timer);
  }

  private document(scope: TransmitScope): DocumentState {
    if (this.stopped) throw new Error("scheduler_stopped");
    if (this.activeBinding !== undefined && scope.binding !== this.activeBinding) throw new Error("stale_registration");
    const identity = key(scope);
    const state = this.documents.get(identity);
    if (!state || !state.active) throw new Error("stale_session");
    if (state.closing) throw new Error("session_closing");
    return state;
  }

  private async closeSession(state: DocumentState, close?: () => Promise<void>): Promise<void> {
    if (state.closing) throw new Error("session_closing");
    if (state.composing) throw new Error("composition_in_progress");
    if (state.latest !== state.durable || state.pending) throw new Error("checkpoint_pending");
    state.closing = true;
    clearTimeout(state.timer);
    state.timer = undefined;
    try {
      await close?.();
      state.active = false;
      if (state.queued === 0n) state.awaitingComposition = false;
      if (!state.awaitingComposition) this.documents.delete(key(state.scope));
      this.startTransmission();
    } finally {
      state.closing = false;
    }
  }

  private schedule(state: DocumentState): void {
    clearTimeout(state.timer);
    state.timer = undefined;
    if (!state.active || state.composing || (this.activeBinding !== undefined && state.scope.binding !== this.activeBinding) || state.durable <= state.queued || state.firstPendingAt === undefined || state.lastEditAt === undefined) return;
    const deadline = Math.max(Math.min(state.lastEditAt + 750, state.firstPendingAt + 5000), state.queueRetryAt ?? 0);
    state.timer = setTimeout(() => {
      state.timer = undefined;
      if (this.activeBinding !== undefined && state.scope.binding !== this.activeBinding) return;
      void this.enqueue(state).catch((error) => { if (!staleCheckpoint(error)) this.ports.failed(error); });
    }, Math.max(0, deadline - Date.now()));
  }

  private enqueue(state: DocumentState): Promise<void> {
    if (state.pending) return state.pending;
    const target = state.durable;
    if (target <= state.queued) return Promise.resolve();
    let success = false;
    let stale = false;
    let retryable = false;
    const task = this.queueTail.then(async () => {
      const operation = await this.ports.queue({ ...state.scope }, target.toString());
      if (this.activeBinding !== undefined && state.scope.binding !== this.activeBinding) throw new Error("stale_registration");
      const actual = number(operation.generation);
      if (!operation.operationId || operation.documentId !== state.scope.documentId || actual !== target ||
        (operation.committed !== undefined && typeof operation.committed !== "boolean")) {
        throw new Error("invalid_queued_operation");
      }
      const existing = this.operations.get(operation.operationId);
      this.operations.set(operation.operationId, { key: key(state.scope), binding: state.scope.binding, documentId: operation.documentId, generation: operation.generation, awaitingIdle: existing?.binding === state.scope.binding && existing.awaitingIdle });
      state.queued = actual;
      state.queueRetryAt = undefined;
      state.queueRetryDelay = 1000;
      this.ports.queueRecovered?.({ ...state.scope });
      if (state.latest === actual) state.firstPendingAt = undefined;
      for (const old of this.documents.values()) {
        if (!old.active && old.awaitingComposition && old.scope.binding === state.scope.binding &&
          old.scope.documentId === state.scope.documentId && actual >= old.durable) {
          old.awaitingComposition = false;
          this.documents.delete(key(old.scope));
        }
      }
      success = true;
      if (operation.committed) this.acknowledge(state.scope, operation, true);
      else {
        this.transmitPending = true;
        this.startTransmission();
      }
    }).catch((error: unknown) => {
      stale = staleCheckpoint(error);
      retryable = retryableQueueError(error);
      if (retryable) {
        this.ports.queueStorageFailed?.({ ...state.scope });
        throw new QueueStorageError(error);
      }
      throw error;
    });
    this.queueTail = task.catch(() => {});
    state.pendingTarget = target;
    state.pending = task.finally(() => {
      state.pending = undefined;
      state.pendingTarget = undefined;
      if (retryable) {
        state.queueRetryAt = Date.now() + state.queueRetryDelay;
        state.queueRetryDelay = Math.min(state.queueRetryDelay * 2, 30_000);
      }
      if ((success || retryable || (stale && state.durable > target)) && state.durable > state.queued) this.schedule(state);
    });
    return state.pending;
  }

  private startTransmission(): void {
    if (this.stopped || this.transmitting || !this.transmitPending || this.requiresReview || this.retryTimer !== undefined || this.blocked()) return;
    this.transmitPending = false;
    this.transmitting = true;
    void this.drain().catch((error) => {
      if (!this.stopped) {
        this.ports.failed(error);
        this.retry();
      }
    }).finally(() => {
      this.transmitting = false;
      this.startTransmission();
    });
  }

  private blocked(): boolean {
    return [...this.documents.values()].some((state) => {
      if (this.activeBinding !== undefined && state.scope.binding !== this.activeBinding) return false;
      if (state.active && state.composing) return true;
      if (state.awaitingComposition && state.latest === state.durable && state.durable === state.queued) {
        state.awaitingComposition = false;
        if (!state.active) this.documents.delete(key(state.scope));
      }
      return state.awaitingComposition;
    });
  }

  private async drain(): Promise<void> {
    const seen = new Set<string>();
    await Promise.resolve();
    while (!this.stopped) {
      if (this.blocked()) {
        this.transmitPending = true;
        return;
      }
      const binding = this.activeBinding;
      let result: TransmitResult;
      try {
        result = await this.ports.transmit();
      } catch (error) {
        if (binding !== this.activeBinding) { this.transmitPending = true; return; }
        throw error;
      }
      if (this.stopped) return;
      if (binding !== this.activeBinding) { this.transmitPending = true; return; }
      if (result.status === "idle") {
        this.retryDelay = 1000;
        for (const [id, operation] of this.operations) {
          if (operation.binding === binding && operation.awaitingIdle) {
            this.operations.delete(id);
            this.ports.receiptConfirmed?.(id);
          }
        }
        return;
      }
      if (result.status === "needs_review") {
        this.requiresReview = true;
        this.transmitPending = false;
        return;
      }
      if (result.status === "auth_required" || result.status === "gateway_auth_required") {
        if (result.operationId) {
          const operation = this.operations.get(result.operationId);
          if (operation && operation.binding === binding) operation.awaitingIdle = true;
        }
        this.requiresReview = true;
        this.transmitPending = false;
        this.ports.authRequired(result.status);
        return;
      }
      if (result.status === "pending" || result.status === "offline" || result.status === "ack_pending") {
        if (result.status === "ack_pending" && result.operationId) {
          const operation = this.operations.get(result.operationId);
          if (operation && operation.binding === binding) operation.awaitingIdle = true;
        }
        this.retry();
        return;
      }
      if (!("operationId" in result) || !result.operationId || seen.has(result.operationId)) {
        this.requiresReview = true;
        this.transmitPending = false;
        this.ports.failed(new Error("transmission_no_progress"));
        return;
      }
      seen.add(result.operationId);
      if (result.status !== "committed") this.operations.delete(result.operationId);
      this.retryDelay = 1000;
    }
  }

  private retry(): void {
    if (this.stopped || this.retryTimer !== undefined) return;
    this.transmitPending = true;
    this.retryTimer = setTimeout(() => {
      this.retryTimer = undefined;
      this.startTransmission();
    }, this.retryDelay);
    this.retryDelay = Math.min(this.retryDelay * 2, 30_000);
  }
}
