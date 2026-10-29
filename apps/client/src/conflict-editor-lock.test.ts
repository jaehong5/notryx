import { afterEach, expect, test, vi } from "vitest";
import { EditorController, type EditorOptions, type EditorPorts, type EditorState } from "./editor-controller";
import type { WorkspaceContentConflictChooseRemoteResponse } from "../../../packages/protocol-types";

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (cause: unknown) => void;
  const promise = new Promise<T>((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}

function fixture() {
  const states: EditorState[] = [];
  const views: Array<{ options: EditorOptions; editable: boolean; composing: boolean; destroy: ReturnType<typeof vi.fn> }> = [];
  const ports: EditorPorts = {
    openDocument: vi.fn(async (id) => ({ document_id: id, edit_session_id: `session-${id}`, generation: "0", bytes_base64: btoa("base") })),
    saveDocument: vi.fn(async (request) => ({ edit_session_id: request.edit_session_id, generation: request.generation })),
    closeDocument: vi.fn(async () => {}),
    exit: vi.fn(async () => {}),
    createEditor: vi.fn((options) => {
      const view = { options, editable: true, composing: false, destroy: vi.fn() };
      views.push(view);
      return { show: vi.fn(), destroy: view.destroy, setEditable: (enabled: boolean) => { view.editable = enabled; }, isComposing: () => view.composing, replaceSource: vi.fn() };
    }),
    stateChanged: (state) => { states.push(state); },
    holdConflictSession: vi.fn(async () => {}),
    releaseConflictSession: vi.fn(),
    retireConflictSession: vi.fn(),
  };
  return { controller: new EditorController(ports), ports, views, states };
}

type Snapshot = ReturnType<EditorController["recoverySnapshot"]>;
function withSnapshot(controller: EditorController, expected: Snapshot, action: () => Promise<WorkspaceContentConflictChooseRemoteResponse>): Promise<WorkspaceContentConflictChooseRemoteResponse> {
  return (controller as EditorController & { withConflictSnapshot(expected: Snapshot, action: () => Promise<WorkspaceContentConflictChooseRemoteResponse>): Promise<WorkspaceContentConflictChooseRemoteResponse> }).withConflictSnapshot(expected, action);
}

afterEach(() => { vi.useRealTimers(); });

test("기준 snapshot의 입력을 잠그고 checkpoint 완료 후에만 선택 동작을 시작한다", async () => {
  const { controller, ports, views } = fixture();
  const saved = deferred<{ edit_session_id: string; generation: string }>();
  const actionResult = deferred<WorkspaceContentConflictChooseRemoteResponse>();
  vi.mocked(ports.saveDocument).mockReturnValueOnce(saved.promise);
  await controller.open("a");
  views[0].options.change("local input");
  const shown = controller.recoverySnapshot();
  const action = vi.fn(() => actionResult.promise);
  const choosing = withSnapshot(controller, shown, action);
  expect(views[0].editable).toBe(false);
  await vi.waitFor(() => expect(ports.saveDocument).toHaveBeenCalledOnce());
  expect(action).not.toHaveBeenCalled();
  await controller.open("b");
  expect(ports.openDocument).toHaveBeenCalledTimes(1);
  views[0].options.change("late input");
  saved.resolve({ edit_session_id: shown.session, generation: shown.generation });
  await vi.waitFor(() => expect(action).toHaveBeenCalledOnce());
  expect(views[0].editable).toBe(false);
  actionResult.resolve({ status: "pending", version: "0" });
  await expect(choosing).resolves.toEqual({ status: "pending", version: "0" });
  expect(views[0].editable).toBe(true);
  expect(new TextDecoder().decode(controller.recoverySnapshot().bytes)).toBe("local input");
  await controller.dispose();
});

test("비교 후 한글 조합이 시작됐다면 조합을 유지하고 선택을 시작하지 않는다", async () => {
  const { controller, ports, views } = fixture();
  await controller.open("a");
  views[0].options.change("local input");
  const shown = controller.recoverySnapshot();
  views[0].composing = true;
  const action = vi.fn(async () => ({ status: "pending" as const, version: "0" }));
  await expect(withSnapshot(controller, shown, action)).rejects.toThrow();
  expect(action).not.toHaveBeenCalled();
  expect(ports.saveDocument).not.toHaveBeenCalled();
  expect(views[0].editable).toBe(true);
  views[0].composing = false;
  expect(new TextDecoder().decode(controller.recoverySnapshot().bytes)).toBe("local input");
  await controller.dispose();
});

test("비교 후 새 입력 또는 다른 문서 전환이 발생하면 이전 snapshot을 선택하지 않는다", async () => {
  for (const change of ["edit", "switch"] as const) {
    const { controller, views } = fixture();
    await controller.open("a");
    const shown = controller.recoverySnapshot();
    if (change === "edit") views[0].options.change("new input");
    else await controller.open("b");
    const action = vi.fn(async () => ({ status: "pending" as const, version: "0" }));
    await expect(withSnapshot(controller, shown, action)).rejects.toThrow();
    expect(action).not.toHaveBeenCalled();
    expect(controller.recoverySnapshot().id).toBe(change === "edit" ? "a" : "b");
    if (change === "edit") expect(new TextDecoder().decode(controller.recoverySnapshot().bytes)).toBe("new input");
    await controller.dispose();
  }
});

test("checkpoint 저장 실패는 선택을 시작하지 않고 편집과 원문을 유지한다", async () => {
  const { controller, ports, views } = fixture();
  vi.mocked(ports.saveDocument).mockRejectedValueOnce(new Error("storage failed"));
  await controller.open("a");
  views[0].options.change("unsaved input");
  const shown = controller.recoverySnapshot();
  const action = vi.fn(async () => ({ status: "pending" as const, version: "0" }));
  await expect(withSnapshot(controller, shown, action)).rejects.toThrow();
  expect(action).not.toHaveBeenCalled();
  expect(views[0].editable).toBe(true);
  expect(new TextDecoder().decode(controller.recoverySnapshot().bytes)).toBe("unsaved input");
  await controller.dispose();
});

test("첫 선택이 잠금 중이면 같은 snapshot의 중복 선택과 창 종료를 시작하지 않는다", async () => {
  const { controller, ports, views } = fixture();
  await controller.open("a");
  const shown = controller.recoverySnapshot();
  const pending = deferred<WorkspaceContentConflictChooseRemoteResponse>();
  const first = vi.fn(() => pending.promise);
  const second = vi.fn(async () => ({ status: "pending" as const, version: "0" }));
  const choosing = withSnapshot(controller, shown, first);
  await vi.waitFor(() => expect(first).toHaveBeenCalledOnce());
  await expect(withSnapshot(controller, shown, second)).rejects.toThrow();
  await controller.close();
  expect(second).not.toHaveBeenCalled();
  expect(ports.exit).not.toHaveBeenCalled();
  expect(views[0].editable).toBe(false);
  pending.resolve({ status: "pending", version: "0" });
  await choosing;
  expect(views[0].editable).toBe(true);
  await controller.dispose();
});

test("원격판 선택이 완료되면 닫힌 기존 session을 다시 편집할 수 없다", async () => {
  const { controller, ports, views } = fixture();
  await controller.open("a");
  const shown = controller.recoverySnapshot();
  const action = vi.fn(async () => ({ status: "completed" as const, generation: "2" }));
  await expect(withSnapshot(controller, shown, action)).resolves.toEqual({ status: "completed", generation: "2" });
  expect(views[0].editable).toBe(false);
  expect(() => controller.recoverySnapshot()).toThrow();
  await controller.open("b");
  await controller.save();
  expect(ports.openDocument).toHaveBeenCalledTimes(1);
  expect(ports.saveDocument).not.toHaveBeenCalled();
  expect(ports.closeDocument).not.toHaveBeenCalled();
});

test("선택 호출 후 응답을 잃으면 원문 checkpoint를 보존하고 기존 session을 잠근다", async () => {
  const { controller, ports, views, states } = fixture();
  await controller.open("a");
  views[0].options.change("saved before request");
  const shown = controller.recoverySnapshot();
  const action = vi.fn(async () => { throw new Error("response lost"); });
  await expect(withSnapshot(controller, shown, action)).rejects.toThrow("response lost");
  expect(action).toHaveBeenCalledOnce();
  expect(ports.saveDocument).toHaveBeenCalledOnce();
  const saved = vi.mocked(ports.saveDocument).mock.calls[0][0];
  expect(atob(saved.bytes_base64)).toBe("saved before request");
  expect(saved.generation).toBe(shown.generation);
  expect(views[0].editable).toBe(false);
  expect(states.at(-1)?.error).toBeTruthy();
  expect(() => controller.recoverySnapshot()).toThrow();
  await controller.open("b");
  await controller.save();
  expect(ports.openDocument).toHaveBeenCalledTimes(1);
  expect(ports.saveDocument).toHaveBeenCalledOnce();
  expect(ports.closeDocument).not.toHaveBeenCalled();
});

test("선택 응답 전 등록이 바뀌면 이전 session을 재활성화하지 않는다", async () => {
  const { controller, ports, views } = fixture();
  let registrationVersion = 1;
  ports.remoteInspectionVersion = () => registrationVersion;
  await controller.open("a");
  const shown = controller.recoverySnapshot();
  const pending = deferred<WorkspaceContentConflictChooseRemoteResponse>();
  const action = vi.fn(() => pending.promise);
  const choosing = withSnapshot(controller, shown, action);
  await vi.waitFor(() => expect(action).toHaveBeenCalledOnce());
  registrationVersion++;
  pending.resolve({ status: "pending", version: "0" });
  await expect(choosing).rejects.toThrow();
  expect(views[0].editable).toBe(false);
  expect(() => controller.recoverySnapshot()).toThrow();
  expect(ports.closeDocument).not.toHaveBeenCalled();
});

test.each(["completed", "unknown"] as const)("%s 이후 창 종료와 dispose가 이전 session의 전송 준비나 닫기를 시도하지 않는다", async (outcome) => {
  const { controller, ports, views } = fixture();
  const queued = vi.fn();
  let retired = false;
  ports.retireConflictSession = vi.fn(() => { retired = true; });
  ports.closeDocument = vi.fn(async () => { queued("close"); });
  ports.exit = vi.fn(async () => { if (!retired) queued("exit"); });
  await controller.open("a");
  views[0].options.change("local checkpoint");
  const shown = controller.recoverySnapshot();
  const action = outcome === "completed"
    ? vi.fn(async () => ({ status: "completed" as const, generation: "2" }))
    : vi.fn(async () => { throw new Error("response lost"); });
  const choosing = withSnapshot(controller, shown, action);
  if (outcome === "completed") await expect(choosing).resolves.toEqual({ status: "completed", generation: "2" });
  else await expect(choosing).rejects.toThrow("response lost");
  expect(ports.saveDocument).toHaveBeenCalledOnce();
  await controller.close();
  await controller.dispose();
  expect(ports.exit).toHaveBeenCalledOnce();
  expect(queued).not.toHaveBeenCalled();
  expect(ports.saveDocument).toHaveBeenCalledOnce();
  expect(ports.closeDocument).not.toHaveBeenCalled();
});
