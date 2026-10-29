import { afterEach, expect, test, vi } from "vitest";
import { AutoTransmit, type AutoTransmitPorts, type QueuedOperation, type TransmitScope } from "./auto-transmit";
import { EditorController, type EditorOptions, type EditorPorts } from "./editor-controller";
import type { M0SavedCheckpoint, WorkspaceContentConflictChooseRemoteResponse } from "../../../packages/protocol-types";

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (cause: unknown) => void;
  const promise = new Promise<T>((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}

type ConflictScheduler = AutoTransmit & {
  holdConflictSession(scope: TransmitScope): Promise<void>;
  releaseConflictSession(scope: TransmitScope): void;
  retireConflictSession(scope: TransmitScope): void;
};
type Snapshot = ReturnType<EditorController["recoverySnapshot"]>;
function choose(controller: EditorController, expected: Snapshot, action: () => Promise<WorkspaceContentConflictChooseRemoteResponse>) {
  return (controller as EditorController & { withConflictSnapshot(expected: Snapshot, action: () => Promise<WorkspaceContentConflictChooseRemoteResponse>): Promise<WorkspaceContentConflictChooseRemoteResponse> }).withConflictSnapshot(expected, action);
}

function fixture() {
  vi.useFakeTimers();
  vi.setSystemTime(0);
  const scope: TransmitScope = { binding: "binding", documentId: "a", editSessionId: "session-a" };
  const other: TransmitScope = { binding: "binding", documentId: "b", editSessionId: "session-b" };
  const schedulerPorts: AutoTransmitPorts = {
    queue: vi.fn(async (current, generation) => ({ operationId: `op-${current.documentId}-${generation}`, documentId: current.documentId, generation })),
    transmit: vi.fn(async () => ({ status: "idle" as const })),
    acknowledged: vi.fn(), authRequired: vi.fn(), failed: vi.fn(),
  };
  const scheduler = new AutoTransmit(schedulerPorts);
  const sessions = new Map<string, TransmitScope>();
  const nativeClose = vi.fn(async () => {});
  const nativeExit = vi.fn(async () => {});
  const checkpoint = vi.fn(async (request: { edit_session_id: string; generation: string; bytes_base64: string }): Promise<M0SavedCheckpoint> => ({ edit_session_id: request.edit_session_id, generation: request.generation }));
  let editor: EditorOptions | undefined;
  const ports: EditorPorts = {
    openDocument: vi.fn(async (id) => ({ document_id: id, edit_session_id: `session-${id}`, generation: "0", bytes_base64: btoa("base") })),
    saveDocument: async (request) => {
      const saved = await checkpoint(request);
      const current = sessions.get(request.edit_session_id);
      if (current) scheduler.durable(current, saved.generation);
      return saved;
    },
    closeDocument: async (request) => {
      const current = sessions.get(request.edit_session_id);
      if (current) {
        let queued = true;
        try { await scheduler.flushDocument(current); }
        catch (cause) {
          if (/checkpoint_pending|composition_in_progress/.test(String(cause))) throw cause;
          queued = false;
        }
        if (queued) await scheduler.deactivate(current, () => nativeClose());
        else await scheduler.deactivatePreservingDraft(current, () => nativeClose());
        sessions.delete(request.edit_session_id);
      } else await nativeClose();
    },
    exit: async () => {
      for (const current of sessions.values()) await scheduler.flushDocument(current);
      await nativeExit();
    },
    createEditor: (options) => {
      editor = options;
      return { show() {}, destroy() {}, setEditable() {}, isComposing: () => false, replaceSource() {} };
    },
    stateChanged: () => {},
    opened: (_id, session) => {
      sessions.set(session, scope);
      scheduler.activateBinding(scope.binding);
      scheduler.openSession(scope);
    },
    edited: (_id, session, generation) => {
      const current = sessions.get(session);
      if (current) scheduler.edit(current, generation);
    },
  };
  Object.assign(ports, {
    holdConflictSession: async (_id: string, session: string) => (scheduler as ConflictScheduler).holdConflictSession(sessions.get(session)!),
    releaseConflictSession: (_id: string, session: string) => (scheduler as ConflictScheduler).releaseConflictSession(sessions.get(session)!),
    retireConflictSession: (_id: string, session: string) => {
      (scheduler as ConflictScheduler).retireConflictSession(sessions.get(session)!);
      sessions.delete(session);
    },
  });
  const controller = new EditorController(ports);
  return { controller, scheduler, schedulerPorts, checkpoint, nativeClose, nativeExit, scope, other,
    openOther: () => { scheduler.openSession(other); sessions.set(other.editSessionId, other); },
    get editor() { if (!editor) throw new Error("missing editor"); return editor; } };
}

async function settle() { for (let i = 0; i < 20; i++) await Promise.resolve(); }
afterEach(() => { vi.useRealTimers(); });

test("선택 대기 중에는 해당 session의 자동 queue를 막고 다른 문서는 계속 전송한다", async () => {
  const rig = fixture();
  await rig.controller.open("a");
  rig.editor.change("local");
  const shown = rig.controller.recoverySnapshot();
  const pending = deferred<WorkspaceContentConflictChooseRemoteResponse>();
  const action = vi.fn(() => pending.promise);
  const choosing = choose(rig.controller, shown, action);
  await settle();
  expect(action).toHaveBeenCalledOnce();
  rig.scheduler.openSession(rig.other);
  rig.scheduler.edit(rig.other, "1"); rig.scheduler.durable(rig.other, "1");
  await vi.advanceTimersByTimeAsync(1000);
  expect(rig.schedulerPorts.queue).not.toHaveBeenCalledWith(rig.scope, "1");
  expect(rig.schedulerPorts.queue).toHaveBeenCalledWith(rig.other, "1");
  expect(rig.schedulerPorts.transmit).toHaveBeenCalled();
  pending.resolve({ status: "pending", version: "0" });
  await choosing;
  await rig.controller.dispose();
  await rig.scheduler.dispose();
});

test("이미 시작한 queue가 끝나기 전에는 선택을 시작하지 않는다", async () => {
  const rig = fixture();
  const queued = deferred<QueuedOperation>();
  vi.mocked(rig.schedulerPorts.queue).mockReturnValueOnce(queued.promise);
  await rig.controller.open("a");
  rig.editor.change("local");
  await rig.controller.flush();
  const shown = rig.controller.recoverySnapshot();
  await vi.advanceTimersByTimeAsync(1000);
  expect(rig.schedulerPorts.queue).toHaveBeenCalledOnce();
  const action = vi.fn(async () => ({ status: "pending" as const, version: "0" }));
  const choosing = choose(rig.controller, shown, action);
  await settle();
  expect(action).not.toHaveBeenCalled();
  queued.resolve({ operationId: "op-a-1", documentId: "a", generation: "1" });
  await expect(choosing).resolves.toEqual({ status: "pending", version: "0" });
  expect(action).toHaveBeenCalledOnce();
  await rig.controller.dispose();
  await rig.scheduler.dispose();
});

test("pending 선택 뒤 새 입력은 저장하고 미해결 412의 queue 거절에도 보존한다", async () => {
  const rig = fixture();
  vi.mocked(rig.schedulerPorts.queue).mockRejectedValue("OutboxBlocked");
  await rig.controller.open("a");
  rig.editor.change("first");
  const shown = rig.controller.recoverySnapshot();
  await choose(rig.controller, shown, async () => ({ status: "pending", version: "0" }));
  rig.editor.change("new local input");
  await rig.controller.flush();
  await vi.advanceTimersByTimeAsync(1000);
  expect(rig.schedulerPorts.queue).toHaveBeenCalledWith(rig.scope, "2");
  expect(rig.schedulerPorts.transmit).not.toHaveBeenCalled();
  expect(new TextDecoder().decode(rig.controller.recoverySnapshot().bytes)).toBe("new local input");
  expect(atob(rig.checkpoint.mock.calls.at(-1)![0].bytes_base64)).toBe("new local input");
  await rig.controller.dispose();
  await rig.scheduler.dispose();
});

test.each(["completed", "unknown"] as const)("%s 이후 App 정리에서 이전 session을 queue하거나 닫지 않는다", async (outcome) => {
  const rig = fixture();
  await rig.controller.open("a");
  rig.editor.change("local");
  const shown = rig.controller.recoverySnapshot();
  const action = outcome === "completed"
    ? vi.fn(async () => ({ status: "completed" as const, generation: "2" }))
    : vi.fn(async () => { throw new Error("response lost"); });
  const choosing = choose(rig.controller, shown, action);
  if (outcome === "completed") await expect(choosing).resolves.toEqual({ status: "completed", generation: "2" });
  else await expect(choosing).rejects.toThrow("response lost");
  await rig.controller.close();
  await rig.controller.dispose();
  await rig.scheduler.dispose();
  await vi.advanceTimersByTimeAsync(1000);
  expect(rig.schedulerPorts.queue).not.toHaveBeenCalledWith(rig.scope, "1");
  expect(rig.nativeClose).not.toHaveBeenCalled();
  expect(rig.checkpoint).toHaveBeenCalledOnce();
});

test("선택 전 checkpoint 실패 시 기존 초안을 재시도해 저장할 수 있다", async () => {
  const rig = fixture();
  vi.mocked(rig.checkpoint).mockRejectedValueOnce(new Error("storage failed"));
  await rig.controller.open("a");
  rig.editor.change("unsaved local");
  const shown = rig.controller.recoverySnapshot();
  const action = vi.fn(async () => ({ status: "pending" as const, version: "0" }));
  await expect(choose(rig.controller, shown, action)).rejects.toThrow("storage failed");
  expect(action).not.toHaveBeenCalled();
  expect(new TextDecoder().decode(rig.controller.recoverySnapshot().bytes)).toBe("unsaved local");
  await rig.controller.flush();
  expect(rig.checkpoint).toHaveBeenCalledTimes(2);
  expect(atob(rig.checkpoint.mock.calls[1][0].bytes_base64)).toBe("unsaved local");
  await vi.advanceTimersByTimeAsync(1000);
  expect(rig.schedulerPorts.queue).toHaveBeenCalledWith(rig.scope, "1");
  await rig.controller.dispose();
  await rig.scheduler.dispose();
});

test("선택 요청 중 unmount는 늦은 결과를 기다리지 않고 이전 session을 다시 queue하지 않는다", async () => {
  const rig = fixture();
  await rig.controller.open("a");
  rig.editor.change("local");
  const shown = rig.controller.recoverySnapshot();
  const pending = deferred<WorkspaceContentConflictChooseRemoteResponse>();
  const action = vi.fn(() => pending.promise);
  const choosing = choose(rig.controller, shown, action).catch(() => undefined);
  await settle();
  expect(action).toHaveBeenCalledOnce();
  await rig.controller.dispose();
  await rig.scheduler.dispose();
  pending.resolve({ status: "completed", generation: "2" });
  await choosing;
  await vi.advanceTimersByTimeAsync(1000);
  expect(rig.schedulerPorts.queue).not.toHaveBeenCalledWith(rig.scope, "1");
  expect(rig.nativeClose).not.toHaveBeenCalled();
});

test("checkpoint 중 unmount는 선택을 시작하지 않고 저장 완료를 기다린다", async () => {
  const rig = fixture();
  const saved = deferred<M0SavedCheckpoint>();
  rig.checkpoint.mockReturnValueOnce(saved.promise);
  await rig.controller.open("a");
  rig.editor.change("local");
  const shown = rig.controller.recoverySnapshot();
  const action = vi.fn(async () => ({ status: "pending" as const, version: "0" }));
  const choosing = choose(rig.controller, shown, action).catch(() => undefined);
  await settle();
  expect(rig.checkpoint).toHaveBeenCalledOnce();
  const disposing = rig.controller.dispose();
  saved.resolve({ edit_session_id: shown.session, generation: shown.generation });
  await Promise.all([choosing, disposing]);
  await rig.scheduler.dispose();
  expect(action).not.toHaveBeenCalled();
  expect(atob(rig.checkpoint.mock.calls[0][0].bytes_base64)).toBe("local");
});

test.each(["completed", "unknown"] as const)("%s 이후 안전하게 창을 닫으며 다른 session만 전송 준비한다", async (outcome) => {
  const rig = fixture();
  await rig.controller.open("a");
  rig.editor.change("local");
  const shown = rig.controller.recoverySnapshot();
  const choosing = choose(rig.controller, shown, outcome === "completed"
    ? async () => ({ status: "completed", generation: "2" })
    : async () => { throw new Error("response lost"); });
  if (outcome === "completed") await expect(choosing).resolves.toEqual({ status: "completed", generation: "2" });
  else await expect(choosing).rejects.toThrow("response lost");
  rig.openOther();
  rig.scheduler.edit(rig.other, "1"); rig.scheduler.durable(rig.other, "1");
  await rig.controller.close();
  expect(rig.nativeExit).toHaveBeenCalledOnce();
  expect(rig.schedulerPorts.queue).toHaveBeenCalledWith(rig.other, "1");
  expect(rig.schedulerPorts.queue).not.toHaveBeenCalledWith(rig.scope, "1");
  expect(rig.nativeClose).not.toHaveBeenCalled();
  expect(atob(rig.checkpoint.mock.calls[0][0].bytes_base64)).toBe("local");
  await rig.controller.dispose();
  await rig.scheduler.dispose();
});

test("결과 불명 상태에서 종료가 실패하면 이전 session을 재개하지 않고 종료를 재시도할 수 있다", async () => {
  const rig = fixture();
  rig.nativeExit.mockRejectedValueOnce(new Error("exit failed"));
  await rig.controller.open("a");
  rig.editor.change("local");
  const shown = rig.controller.recoverySnapshot();
  await expect(choose(rig.controller, shown, async () => { throw new Error("response lost"); })).rejects.toThrow();
  await rig.controller.close();
  expect(() => rig.controller.recoverySnapshot()).toThrow();
  await rig.controller.close();
  expect(rig.nativeExit).toHaveBeenCalledTimes(2);
  expect(rig.schedulerPorts.queue).not.toHaveBeenCalledWith(rig.scope, "1");
  expect(rig.nativeClose).not.toHaveBeenCalled();
  expect(atob(rig.checkpoint.mock.calls[0][0].bytes_base64)).toBe("local");
  await rig.controller.dispose();
  await rig.scheduler.dispose();
});
