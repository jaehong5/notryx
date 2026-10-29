import { afterEach, expect, test, vi } from "vitest";
import { EditorController, type EditorOptions, type EditorPorts, type EditorState } from "./editor-controller";
import type { WorkspaceConflictResolutionState, WorkspaceContentConflictProgressResponse, WorkspaceOpenedDocument } from "../../../packages/protocol-types";

const binding = { server_origin:"https://example.invalid", workspace_id:"workspace", workspace_epoch:"epoch", device_id:"device", device_stream_id:"stream" };
const queued:WorkspaceContentConflictProgressResponse = { status:"pending", version:"0", requested_operation_id:"019a1000-0000-7000-8000-000000000004" };
const pending:WorkspaceConflictResolutionState = { status:"pending", version:"0", generation:"1", choice:"save_merged", needs_review:false, frozen:false };
const staleDocument:WorkspaceOpenedDocument = { binding, document_id:"a", edit_session_id:"new-session", generation:"0", bytes_base64:btoa("old checkpoint"), readonly:false, local_draft:true, has_session:true, cache_warning:null, open_notice:null };

function fixture() {
  const views:Array<{ options:EditorOptions; editable:boolean; destroyed:boolean }> = [];
  const states:EditorState[] = [];
  const ports:EditorPorts = {
    openDocument:vi.fn(async (id) => ({ document_id:id, edit_session_id:`session-${id}`, generation:"0", bytes_base64:btoa("old checkpoint") })),
    saveDocument:vi.fn(async (request) => ({ edit_session_id:request.edit_session_id, generation:request.generation })),
    closeDocument:vi.fn(async () => {}), exit:vi.fn(async () => {}),
    createEditor:vi.fn((options) => {
      const view = { options, editable:true, destroyed:false }; views.push(view);
      return { show:vi.fn(), destroy:() => { view.destroyed = true; }, setEditable:(enabled:boolean) => { view.editable = enabled; }, isComposing:() => false, replaceSource:vi.fn() };
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

function recoverySource(controller:EditorController):Snapshot {
  try { return controller.recoverySnapshot(); }
  catch {
    const alternative = controller as EditorController & { recoverySourceSnapshot?:() => Snapshot };
    if (!alternative.recoverySourceSnapshot) throw new Error("unsaved source unavailable");
    return alternative.recoverySourceSnapshot();
  }
}

async function failedCheckpointAndRelease() {
  const result = fixture();
  const { controller, ports, views } = result;
  const storage = { failed:true };
  vi.mocked(ports.saveDocument).mockImplementation(async (request) => {
    if (storage.failed) throw new Error("storage failed");
    return { edit_session_id:request.edit_session_id, generation:request.generation };
  });
  vi.mocked(ports.releaseConflictSession!).mockImplementation(() => { throw new Error("release failed"); });
  vi.mocked(ports.retireConflictSession!).mockImplementation(() => { throw new Error("retire failed"); });
  await controller.open("a");
  views[0].options.change("unsaved latest draft");
  const selected = controller.recoverySnapshot();
  const action = vi.fn(async () => queued);
  await expect(queueMerge(controller, selected, action)).rejects.toThrow();
  expect(action).not.toHaveBeenCalled();
  expect(ports.releaseConflictSession).toHaveBeenCalled();
  return { ...result, storage, selected, action };
}

afterEach(() => { vi.useRealTimers(); });

test("checkpoint와 hold 해제가 함께 실패하면 저장 전 재열기와 닫기는 원문을 폐기하지 않는다", async () => {
  const { controller, ports, views } = await failedCheckpointAndRelease();
  const reopen = vi.fn(async () => ({ state:pending, document:staleDocument }));
  await expect(controller.reopenConflictCheckpoint(reopen)).rejects.toThrow();
  expect(reopen).not.toHaveBeenCalled();
  await controller.close();
  expect(ports.exit).not.toHaveBeenCalled();
  expect(ports.closeDocument).not.toHaveBeenCalled();
  expect(views[0].destroyed).toBe(false);
});

test("checkpoint와 hold 해제가 함께 실패해도 원문을 복구하고 저장을 다시 시도할 수 있다", async () => {
  const { controller, ports, views, storage, selected, action } = await failedCheckpointAndRelease();
  const readable = recoverySource(controller);
  expect(readable.session).toBe("session-a");
  expect(readable.generation).toBe(selected.generation);
  expect(new TextDecoder().decode(readable.bytes)).toBe("unsaved latest draft");
  const attemptsBeforeRetry = vi.mocked(ports.saveDocument).mock.calls.length;
  await controller.save().catch(() => {});
  expect(vi.mocked(ports.saveDocument).mock.calls.length).toBeGreaterThan(attemptsBeforeRetry);
  const failedRetry = vi.mocked(ports.saveDocument).mock.calls.at(-1)![0];
  expect(failedRetry.generation).toBe(selected.generation);
  expect(atob(failedRetry.bytes_base64)).toBe("unsaved latest draft");
  expect(new TextDecoder().decode(recoverySource(controller).bytes)).toBe("unsaved latest draft");
  await controller.close();
  expect(ports.exit).not.toHaveBeenCalled();
  expect(views[0].destroyed).toBe(false);

  storage.failed = false;
  const attemptsBeforeSuccess = vi.mocked(ports.saveDocument).mock.calls.length;
  await controller.save();
  expect(vi.mocked(ports.saveDocument).mock.calls.length).toBeGreaterThan(attemptsBeforeSuccess);
  const saved = vi.mocked(ports.saveDocument).mock.calls.at(-1)![0];
  expect(saved.generation).toBe(selected.generation);
  expect(atob(saved.bytes_base64)).toBe("unsaved latest draft");
  expect(views[0].destroyed).toBe(false);
  expect(action).not.toHaveBeenCalled();
  await controller.dispose();
});

test("native queue 뒤 retire에 실패하면 저장된 초안을 잠그고 오류를 표시한다", async () => {
  const { controller, ports, views, states } = fixture();
  vi.mocked(ports.retireConflictSession!).mockImplementation(() => { throw new Error("retire failed"); });
  await controller.open("a");
  views[0].options.change("saved merge");
  const selected = controller.recoverySnapshot();
  const action = vi.fn(async () => queued);
  await expect(queueMerge(controller, selected, action)).rejects.toThrow();
  expect(action).toHaveBeenCalledOnce();
  expect(atob(vi.mocked(ports.saveDocument).mock.calls[0][0].bytes_base64)).toBe("saved merge");
  expect(ports.retireConflictSession).toHaveBeenCalledOnce();
  expect(views[0].editable).toBe(false);
  expect(states.at(-1)?.error).toBeTruthy();
  expect(views[0].destroyed).toBe(false);
  const reopen = vi.fn(async () => ({ state:pending, document:{ ...staleDocument, generation:selected.generation, bytes_base64:btoa("saved merge") } }));
  await expect(controller.reopenConflictCheckpoint(reopen)).rejects.toThrow();
  expect(reopen).not.toHaveBeenCalled();
  vi.mocked(ports.retireConflictSession!).mockImplementation(() => {});
  await expect(controller.reopenConflictCheckpoint(reopen)).resolves.toEqual(pending);
  expect(reopen).toHaveBeenCalledExactlyOnceWith("a", "session-a");
  expect(new TextDecoder().decode(recoverySource(controller).bytes)).toBe("saved merge");
  await controller.dispose();
});

test("저장 성공 응답이 늦는 동안 등록이 변경되면 이전 scope를 해제하지 않는다", async () => {
  const { controller, ports, views } = fixture();
  let registrationVersion = 1;
  let finishSave!:(response:{ edit_session_id:string; generation:string }) => void;
  ports.remoteInspectionVersion = () => registrationVersion;
  vi.mocked(ports.saveDocument).mockRejectedValueOnce(new Error("storage failed")).mockImplementationOnce(() => new Promise((resolve) => { finishSave = resolve; }));
  vi.mocked(ports.releaseConflictSession!).mockImplementationOnce(() => { throw new Error("release failed"); });
  await controller.open("a");
  views[0].options.change("unsaved latest draft");
  const selected = controller.recoverySnapshot();
  const action = vi.fn(async () => queued);
  await expect(queueMerge(controller, selected, action)).rejects.toThrow();
  expect(action).not.toHaveBeenCalled();
  expect(ports.releaseConflictSession).toHaveBeenCalledOnce();
  const saving = controller.save();
  await vi.waitFor(() => expect(ports.saveDocument).toHaveBeenCalledTimes(2));
  registrationVersion++;
  finishSave({ edit_session_id:selected.session, generation:selected.generation });
  await saving;
  const saved = vi.mocked(ports.saveDocument).mock.calls.at(-1)![0];
  expect(saved.generation).toBe(selected.generation);
  expect(atob(saved.bytes_base64)).toBe("unsaved latest draft");
  expect(ports.releaseConflictSession).toHaveBeenCalledOnce();
  expect(action).not.toHaveBeenCalled();
  expect(views[0].destroyed).toBe(false);
});
