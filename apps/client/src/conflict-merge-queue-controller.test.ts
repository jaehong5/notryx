import { afterEach, expect, test, vi } from "vitest";
import { EditorController, type EditorOptions, type EditorPorts, type EditorState } from "./editor-controller";
import type { WorkspaceConflictResolutionState, WorkspaceContentConflictProgressResponse, WorkspaceOpenedDocument } from "../../../packages/protocol-types";

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (cause: unknown) => void;
  const promise = new Promise<T>((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}

const binding = { server_origin:"https://example.invalid", workspace_id:"workspace", workspace_epoch:"epoch", device_id:"device", device_stream_id:"stream" };
const mergeOperationId = "019a1000-0000-7000-8000-000000000004";
const queued: WorkspaceContentConflictProgressResponse = { status:"pending", version:"0", requested_operation_id:mergeOperationId };
const pending: WorkspaceConflictResolutionState = { status:"pending", version:"0", generation:"1", choice:"save_merged", needs_review:false, frozen:false };
const reopened: WorkspaceOpenedDocument = { binding, document_id:"a", edit_session_id:"new-session", generation:"2", bytes_base64:btoa("merged"), readonly:false, local_draft:true, has_session:true, cache_warning:null, open_notice:null };

function fixture() {
  const states: EditorState[] = [];
  const views: Array<{ options:EditorOptions; editable:boolean; composing:boolean; destroy:ReturnType<typeof vi.fn> }> = [];
  const ports: EditorPorts = {
    openDocument:vi.fn(async (id) => ({ document_id:id, edit_session_id:`session-${id}`, generation:"0", bytes_base64:btoa("base") })),
    saveDocument:vi.fn(async (request) => ({ edit_session_id:request.edit_session_id, generation:request.generation })),
    closeDocument:vi.fn(async () => {}),
    exit:vi.fn(async () => {}),
    createEditor:vi.fn((options) => {
      const view = { options, editable:true, composing:false, destroy:vi.fn() };
      views.push(view);
      return { show:vi.fn(), destroy:view.destroy, setEditable:(enabled:boolean) => { view.editable = enabled; }, isComposing:() => view.composing, replaceSource:vi.fn() };
    }),
    stateChanged:(state) => { states.push(state); },
    holdConflictSession:vi.fn(async () => {}), releaseConflictSession:vi.fn(), retireConflictSession:vi.fn(),
  };
  return { controller:new EditorController(ports), ports, views, states };
}

type Snapshot = ReturnType<EditorController["recoverySnapshot"]>;
function queueMerge(controller:EditorController, snapshot:Snapshot, action:() => Promise<WorkspaceContentConflictProgressResponse>):Promise<WorkspaceContentConflictProgressResponse> {
  return (controller as EditorController & { withMergeQueueSnapshot(expected:Snapshot, action:() => Promise<WorkspaceContentConflictProgressResponse>):Promise<WorkspaceContentConflictProgressResponse> }).withMergeQueueSnapshot(snapshot, action);
}

afterEach(() => { vi.useRealTimers(); });

test("합본 전송 준비는 checkpoint 완료를 기다리고 pending 응답 뒤 이전 session을 퇴역해 재열 수 있다", async () => {
  const { controller, ports, views, states } = fixture();
  const saved = deferred<{ edit_session_id:string; generation:string }>();
  const response = deferred<WorkspaceContentConflictProgressResponse>();
  vi.mocked(ports.saveDocument).mockReturnValueOnce(saved.promise);
  await controller.open("a");
  views[0].options.change("merged");
  const shown = controller.recoverySnapshot();
  const action = vi.fn(() => response.promise);
  const work = queueMerge(controller, shown, action);
  expect(views[0].editable).toBe(false);
  await vi.waitFor(() => expect(ports.saveDocument).toHaveBeenCalledOnce());
  expect(ports.holdConflictSession).toHaveBeenCalledExactlyOnceWith("a", "session-a");
  expect(action).not.toHaveBeenCalled();
  saved.resolve({ edit_session_id:shown.session, generation:shown.generation });
  await vi.waitFor(() => expect(action).toHaveBeenCalledOnce());
  expect(views[0].editable).toBe(false);
  response.resolve(queued);
  await expect(work).resolves.toEqual(queued);
  expect(ports.retireConflictSession).toHaveBeenCalledExactlyOnceWith("a", "session-a");
  expect(ports.releaseConflictSession).not.toHaveBeenCalled();
  expect(views[0].editable).toBe(false);
  expect(() => controller.recoverySnapshot()).toThrow();
  expect(states.at(-1)?.notice ?? "").not.toMatch(/완료/);

  const reopen = vi.fn(async () => ({ state:pending, document:reopened }));
  await expect(controller.reopenConflictCheckpoint(reopen)).resolves.toEqual(pending);
  expect(reopen).toHaveBeenCalledExactlyOnceWith("a", "session-a");
  expect(views[0].destroy).toHaveBeenCalledOnce();
  expect(views[1].editable).toBe(true);
  expect(controller.recoverySnapshot().session).toBe("new-session");
  expect(new TextDecoder().decode(controller.recoverySnapshot().bytes)).toBe("merged");
  await controller.dispose();
});

test("합본 준비 전 한글 조합이 시작되면 native 작업을 호출하지 않는다", async () => {
  const { controller, ports, views } = fixture();
  await controller.open("a");
  views[0].options.change("입력");
  const shown = controller.recoverySnapshot();
  views[0].composing = true;
  const action = vi.fn(async () => queued);
  await expect(queueMerge(controller, shown, action)).rejects.toThrow();
  expect(action).not.toHaveBeenCalled();
  expect(ports.holdConflictSession).not.toHaveBeenCalled();
  expect(ports.saveDocument).not.toHaveBeenCalled();
  expect(views[0].editable).toBe(true);
  views[0].composing = false;
  expect(new TextDecoder().decode(controller.recoverySnapshot().bytes)).toBe("입력");
  await controller.dispose();
});

test("checkpoint 실패는 native 작업을 시작하지 않고 편집기와 초안을 유지한다", async () => {
  const { controller, ports, views } = fixture();
  vi.mocked(ports.saveDocument).mockRejectedValueOnce(new Error("storage failed"));
  await controller.open("a");
  views[0].options.change("unsaved merge");
  const shown = controller.recoverySnapshot();
  const action = vi.fn(async () => queued);
  await expect(queueMerge(controller, shown, action)).rejects.toThrow();
  expect(action).not.toHaveBeenCalled();
  expect(ports.releaseConflictSession).toHaveBeenCalledExactlyOnceWith("a", "session-a");
  expect(ports.retireConflictSession).not.toHaveBeenCalled();
  expect(views[0].editable).toBe(true);
  expect(new TextDecoder().decode(controller.recoverySnapshot().bytes)).toBe("unsaved merge");
  await controller.dispose();
});

test("queue 호출 뒤 응답이 유실되면 이전 session을 퇴역하고 확인 전까지 잠근다", async () => {
  const { controller, ports, views, states } = fixture();
  await controller.open("a");
  views[0].options.change("merged");
  const shown = controller.recoverySnapshot();
  const action = vi.fn(async () => { throw new Error("response lost"); });
  await expect(queueMerge(controller, shown, action)).rejects.toThrow("response lost");
  expect(action).toHaveBeenCalledOnce();
  const saved = vi.mocked(ports.saveDocument).mock.calls[0][0];
  expect(saved.generation).toBe(shown.generation);
  expect(atob(saved.bytes_base64)).toBe("merged");
  expect(ports.retireConflictSession).toHaveBeenCalledExactlyOnceWith("a", "session-a");
  expect(ports.releaseConflictSession).not.toHaveBeenCalled();
  expect(views[0].editable).toBe(false);
  expect(states.at(-1)?.error).toBeTruthy();
  expect(() => controller.recoverySnapshot()).toThrow();
  const reopen = vi.fn(async () => ({ state:pending, document:reopened }));
  await expect(controller.reopenConflictCheckpoint(reopen)).resolves.toEqual(pending);
  expect(new TextDecoder().decode(controller.recoverySnapshot().bytes)).toBe("merged");
  await controller.dispose();
});

test("대기 중 등록이 바뀌거나 표시 후 session이 바뀌면 이전 session을 재활성화하지 않는다", async () => {
  for (const change of ["registration", "session"] as const) {
    const { controller, ports, views } = fixture();
    let registrationVersion = 1;
    ports.remoteInspectionVersion = () => registrationVersion;
    await controller.open("a");
    const shown = controller.recoverySnapshot();
    const response = deferred<WorkspaceContentConflictProgressResponse>();
    const action = vi.fn(() => response.promise);
    if (change === "session") {
      await controller.open("b");
      await expect(queueMerge(controller, shown, action)).rejects.toThrow();
      expect(action).not.toHaveBeenCalled();
      expect(controller.recoverySnapshot().id).toBe("b");
    } else {
      const work = queueMerge(controller, shown, action);
      await vi.waitFor(() => expect(action).toHaveBeenCalledOnce());
      registrationVersion++;
      response.resolve(queued);
      await expect(work).rejects.toThrow();
      expect(views[0].editable).toBe(false);
      expect(() => controller.recoverySnapshot()).toThrow();
      expect(ports.releaseConflictSession).not.toHaveBeenCalled();
    }
    await controller.dispose();
  }
});

test("checkpoint 실패 후 편집 잠금 해제도 실패하면 native 작업을 막고 session을 잠근다", async () => {
  const { controller, ports, views } = fixture();
  vi.mocked(ports.saveDocument).mockRejectedValueOnce(new Error("storage failed"));
  vi.mocked(ports.releaseConflictSession!).mockImplementationOnce(() => { throw new Error("release failed"); });
  await controller.open("a");
  views[0].options.change("unsaved merge");
  const shown = controller.recoverySnapshot();
  const action = vi.fn(async () => queued);
  await expect(queueMerge(controller, shown, action)).rejects.toThrow();
  expect(action).not.toHaveBeenCalled();
  expect(ports.releaseConflictSession).toHaveBeenCalledOnce();
  expect(views[0].editable).toBe(false);
  expect(new TextDecoder().decode(controller.recoverySnapshot().bytes)).toBe("unsaved merge");
  await controller.dispose();
});
