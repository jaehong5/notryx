import { afterEach, expect, test, vi } from "vitest";
import { EditorController, type EditorOptions, type EditorPorts } from "./editor-controller";
import type { WorkspaceConflictResolutionState, WorkspaceContentConflictChooseRemoteResponse, WorkspaceOpenedDocument } from "../../../packages/protocol-types";

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((done) => { resolve = done; });
  return { promise, resolve };
}

type Snapshot = ReturnType<EditorController["recoverySnapshot"]>;
type ReopenResult = { state: WorkspaceConflictResolutionState; document: WorkspaceOpenedDocument };
type ConflictController = EditorController & {
  withConflictSnapshot(expected: Snapshot, action: () => Promise<WorkspaceContentConflictChooseRemoteResponse>): Promise<WorkspaceContentConflictChooseRemoteResponse>;
  reopenConflictCheckpoint(action: (documentId: string, previousSessionId: string) => Promise<ReopenResult>): Promise<WorkspaceConflictResolutionState>;
};
const binding = { server_origin: "https://example.invalid", workspace_id: "workspace", workspace_epoch: "epoch", device_id: "device", device_stream_id: "stream" };
const opened = (bytes: string, generation = "2"): WorkspaceOpenedDocument => ({ binding, document_id: "a", edit_session_id: "reopened-session", generation, bytes_base64: btoa(bytes), readonly: false, local_draft: true, has_session: true, cache_warning: null, open_notice: null });
const pending: WorkspaceConflictResolutionState = { status: "pending", version: "0", generation: "1", choice: "choose_remote", needs_review: false, frozen: false };
const notFound: WorkspaceConflictResolutionState = { status: "not_found" };
const completed: WorkspaceConflictResolutionState = { status: "completed", generation: "2", frozen: false };

function fixture() {
  const views: Array<{ options: EditorOptions; editable: boolean; destroy: ReturnType<typeof vi.fn>; show: ReturnType<typeof vi.fn> }> = [];
  const openedHook = vi.fn();
  const ports: EditorPorts = {
    openDocument: vi.fn(async (id) => ({ document_id: id, edit_session_id: "prior-session", generation: "0", bytes_base64: btoa("base") })),
    saveDocument: vi.fn(async (request) => ({ edit_session_id: request.edit_session_id, generation: request.generation })),
    closeDocument: vi.fn(async () => {}),
    exit: vi.fn(async () => {}),
    createEditor: vi.fn((options) => {
      const view = { options, editable: true, destroy: vi.fn(), show: vi.fn() };
      views.push(view);
      return { show: view.show, destroy: view.destroy, setEditable: (enabled: boolean) => { view.editable = enabled; }, isComposing: () => false, replaceSource: vi.fn() };
    }),
    stateChanged: vi.fn(),
    opened: openedHook,
    holdConflictSession: vi.fn(async () => {}), releaseConflictSession: vi.fn(), retireConflictSession: vi.fn(),
  };
  return { controller: new EditorController(ports) as ConflictController, ports, views, openedHook };
}

async function block(controller: ConflictController, result: "completed" | "unknown") {
  await controller.open("a");
  const shown = controller.recoverySnapshot();
  if (result === "completed") await controller.withConflictSnapshot(shown, async () => ({ status: "completed", generation: "2" }));
  else await expect(controller.withConflictSnapshot(shown, async () => { throw new Error("response lost"); })).rejects.toThrow();
}

afterEach(() => { vi.useRealTimers(); });

test.each([
  ["completed", completed, "remote", "2"],
  ["unknown", pending, "local checkpoint", "1"],
  ["unknown", notFound, "unselected local checkpoint", "2"],
] as const)("%s 결과를 확인해 durable checkpoint를 새 session으로 열고 복구 상태를 전달한다", async (result, state, bytes, generation) => {
  const { controller, ports, views, openedHook } = fixture();
  await block(controller, result);
  const reopen = vi.fn(async () => ({ state, document: opened(bytes, generation) }));
  await expect(controller.reopenConflictCheckpoint(reopen)).resolves.toEqual(state);
  expect(reopen).toHaveBeenCalledExactlyOnceWith("a", "prior-session");
  expect(ports.openDocument).toHaveBeenCalledTimes(1);
  expect(ports.closeDocument).not.toHaveBeenCalledWith({ document_id: "a", edit_session_id: "prior-session" });
  expect(views[0].destroy).toHaveBeenCalledOnce();
  expect(views[1].editable).toBe(true);
  expect(controller.recoverySnapshot().session).toBe("reopened-session");
  expect(new TextDecoder().decode(controller.recoverySnapshot().bytes)).toBe(bytes);
  expect(openedHook).toHaveBeenLastCalledWith("a", "reopened-session", generation, true, state);
  await controller.dispose();
});

test("재열기가 실패하면 이전 editor는 계속 잠기고 새 session은 표시하지 않는다", async () => {
  const { controller, ports, views } = fixture();
  await block(controller, "unknown");
  await expect(controller.reopenConflictCheckpoint(async () => { throw new Error("status unavailable"); })).rejects.toThrow();
  expect(views[0].editable).toBe(false);
  expect(views[0].destroy).not.toHaveBeenCalled();
  expect(() => controller.recoverySnapshot()).toThrow();
  expect(ports.closeDocument).not.toHaveBeenCalled();
  await controller.dispose();
});

test("다른 문서나 이전 session을 반환한 재열기 결과는 표시하지 않는다", async () => {
  for (const wrong of [{ ...opened("remote"), document_id: "other" }, { ...opened("remote"), edit_session_id: "prior-session" }]) {
    const { controller, views } = fixture();
    await block(controller, "completed");
    await expect(controller.reopenConflictCheckpoint(async () => ({ state: completed, document: wrong }))).rejects.toThrow();
    expect(views[0].editable).toBe(false);
    expect(() => controller.recoverySnapshot()).toThrow();
    await controller.dispose();
  }
});

test("재열기 중 중복 요청은 시작하지 않고 결과가 도착할 때까지 이전 editor를 잠근다", async () => {
  const { controller, views } = fixture();
  await block(controller, "unknown");
  const next = deferred<ReopenResult>();
  const first = vi.fn(() => next.promise);
  const second = vi.fn(async () => ({ state: pending, document: opened("local", "1") }));
  const reopening = controller.reopenConflictCheckpoint(first);
  await expect(controller.reopenConflictCheckpoint(second)).rejects.toThrow();
  expect(second).not.toHaveBeenCalled();
  expect(views[0].editable).toBe(false);
  next.resolve({ state: pending, document: opened("local", "1") });
  await reopening;
  expect(new TextDecoder().decode(controller.recoverySnapshot().bytes)).toBe("local");
  await controller.dispose();
});

test("controller가 폐기된 뒤 도착한 새 session은 표시하지 않고 정리한다", async () => {
  const { controller, ports, views } = fixture();
  await block(controller, "completed");
  const next = deferred<ReopenResult>();
  const reopening = controller.reopenConflictCheckpoint(() => next.promise).catch(() => undefined);
  await controller.dispose();
  next.resolve({ state: completed, document: opened("remote") });
  await reopening;
  expect(views).toHaveLength(1);
  expect(ports.closeDocument).toHaveBeenCalledWith({ document_id: "a", edit_session_id: "reopened-session" });
});

test("복구 session 등록 실패는 새 editor를 활성화하거나 복구 완료로 반환하지 않는다", async () => {
  const { controller, ports, views, openedHook } = fixture();
  await block(controller, "completed");
  openedHook.mockImplementation((_id: string, session: string) => {
    if (session === "reopened-session") throw new Error("scheduler registration failed");
  });
  await expect(controller.reopenConflictCheckpoint(async () => ({ state: completed, document: opened("remote") }))).rejects.toThrow();
  expect(views.at(-1)!.editable).toBe(false);
  expect(() => controller.recoverySnapshot()).toThrow();
  expect(ports.closeDocument).not.toHaveBeenCalledWith({ document_id: "a", edit_session_id: "prior-session" });
  await controller.dispose();
});
