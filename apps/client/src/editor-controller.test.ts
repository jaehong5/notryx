import { afterEach, expect, test, vi } from "vitest";
import { EditorController, type EditorOptions, type EditorPorts, type EditorState } from "./editor-controller";

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason: Error) => void;
  const promise = new Promise<T>((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}

const opened = (id: string) => ({ document_id: id, edit_session_id: `session-${id}`, generation: "0", bytes_base64: btoa("base") });

function fixture() {
  const states: EditorState[] = [];
  const views: Array<{ options: EditorOptions; editable: boolean; composing: boolean; show: ReturnType<typeof vi.fn>; destroy: ReturnType<typeof vi.fn> }> = [];
  const ports: EditorPorts = {
    openDocument: vi.fn(async (id) => opened(id)),
    saveDocument: vi.fn(async (request) => ({ edit_session_id: request.edit_session_id, generation: request.generation })),
    closeDocument: vi.fn(async () => {}),
    exit: vi.fn(async () => {}),
    createEditor: vi.fn((options) => {
      const view = { options, editable: true, composing: false, show: vi.fn(), destroy: vi.fn() };
      views.push(view);
      return {
        show: view.show,
        destroy: view.destroy,
        setEditable: (enabled: boolean) => { view.editable = enabled; },
        isComposing: () => view.composing,
      };
    }),
    stateChanged: (state) => { states.push(state); },
  };
  return { controller: new EditorController(ports), ports, states, views };
}

afterEach(() => { vi.useRealTimers(); });

test("continuous edits keep the first 250 ms deadline and save the latest text", async () => {
  vi.useFakeTimers();
  const { controller, ports, views } = fixture();
  await controller.open("a");
  views[0].options.change("first");
  await vi.advanceTimersByTimeAsync(200);
  views[0].options.change("latest");
  await vi.advanceTimersByTimeAsync(49);
  expect(ports.saveDocument).not.toHaveBeenCalled();
  await vi.advanceTimersByTimeAsync(1);
  expect(ports.saveDocument).toHaveBeenCalledExactlyOnceWith({ document_id: "a", edit_session_id: "session-a", generation: "2", bytes_base64: btoa("latest") });
  await controller.dispose();
});

test("failed local persistence keeps the editor and prevents document switch and exit", async () => {
  const { controller, ports, views, states } = fixture();
  await controller.open("a");
  views[0].options.change("unsaved");
  vi.mocked(ports.saveDocument).mockRejectedValue(new Error("disk"));
  await controller.open("b");
  await controller.close();
  expect(ports.openDocument).toHaveBeenCalledTimes(1);
  expect(ports.closeDocument).not.toHaveBeenCalled();
  expect(ports.exit).not.toHaveBeenCalled();
  expect(views[0].editable).toBe(true);
  expect(states.at(-1)?.selected).toBe("a");
  expect(states.at(-1)?.error).toContain("disk");
  vi.mocked(ports.saveDocument).mockImplementation(async (request) => ({ edit_session_id: request.edit_session_id, generation: request.generation }));
  await controller.flush();
  expect(states.at(-1)?.status).toBe("이 기기에 저장됨");
  await controller.dispose();
});

test("failed editor preparation closes only the new session and preserves the previous editor", async () => {
  const { controller, ports, views, states } = fixture();
  await controller.open("a");
  vi.mocked(ports.createEditor).mockImplementationOnce(() => { throw new Error("view creation"); });
  await controller.open("b");
  expect(ports.closeDocument).toHaveBeenCalledExactlyOnceWith({ document_id: "b", edit_session_id: "session-b" });
  expect(views[0].destroy).not.toHaveBeenCalled();
  expect(views[0].editable).toBe(true);
  expect(states.at(-1)?.selected).toBe("a");
  await controller.dispose();
});

test("failed mounting leaves the previous native session active", async () => {
  const { controller, ports, views } = fixture();
  await controller.open("a");
  const create = ports.createEditor;
  ports.createEditor = (options) => {
    const view = create(options);
    view.show = () => { throw new Error("mount"); };
    return view;
  };
  await controller.open("b");
  expect(ports.closeDocument).toHaveBeenCalledExactlyOnceWith({ document_id: "b", edit_session_id: "session-b" });
  expect(views[0].destroy).not.toHaveBeenCalled();
  expect(views[0].editable).toBe(true);
  await controller.dispose();
});

test("failed previous-session close restores its editor and discards the prepared session", async () => {
  const { controller, ports, views, states } = fixture();
  await controller.open("a");
  vi.mocked(ports.closeDocument).mockRejectedValueOnce(new Error("close failed"));
  await controller.open("b");
  expect(ports.closeDocument).toHaveBeenNthCalledWith(1, { document_id: "a", edit_session_id: "session-a" });
  expect(ports.closeDocument).toHaveBeenNthCalledWith(2, { document_id: "b", edit_session_id: "session-b" });
  expect(views[1].destroy).toHaveBeenCalledOnce();
  expect(views[0].show).toHaveBeenCalledTimes(2);
  expect(views[0].editable).toBe(true);
  expect(states.at(-1)?.selected).toBe("a");
  await controller.dispose();
});

test("exit failure leaves the edit session usable for later edits and retry", async () => {
  const { controller, ports, views } = fixture();
  await controller.open("a");
  views[0].options.change("first");
  vi.mocked(ports.exit).mockRejectedValueOnce(new Error("exit failed"));
  await controller.close();
  expect(ports.closeDocument).not.toHaveBeenCalled();
  expect(views[0].editable).toBe(true);
  views[0].options.change("second");
  await controller.close();
  expect(ports.saveDocument).toHaveBeenLastCalledWith({ document_id: "a", edit_session_id: "session-a", generation: "2", bytes_base64: btoa("second") });
  expect(ports.exit).toHaveBeenCalledTimes(2);
  expect(views[0].editable).toBe(false);
  await controller.open("b");
  expect(ports.openDocument).toHaveBeenCalledTimes(1);
  await controller.dispose();
});

test("a late open response after disposal is released without mounting or publishing state", async () => {
  const { controller, ports, states } = fixture();
  const response = deferred<ReturnType<typeof opened>>();
  vi.mocked(ports.openDocument).mockReturnValue(response.promise);
  const opening = controller.open("a");
  await Promise.resolve();
  await controller.dispose();
  const count = states.length;
  response.resolve(opened("a"));
  await opening;
  expect(ports.createEditor).not.toHaveBeenCalled();
  expect(ports.closeDocument).toHaveBeenCalledExactlyOnceWith({ document_id: "a", edit_session_id: "session-a" });
  expect(states).toHaveLength(count);
});

test("disposal cancels the batching timer and ignores a late save response", async () => {
  vi.useFakeTimers();
  const { controller, ports, views, states } = fixture();
  await controller.open("a");
  const response = deferred<{ edit_session_id: string; generation: string }>();
  vi.mocked(ports.saveDocument).mockReturnValue(response.promise);
  views[0].options.change("latest");
  const saving = controller.flush();
  const disposed = controller.dispose();
  const count = states.length;
  response.resolve({ edit_session_id: "session-a", generation: "1" });
  await Promise.all([saving, disposed]);
  await vi.advanceTimersByTimeAsync(500);
  expect(ports.saveDocument).toHaveBeenCalledOnce();
  expect(states).toHaveLength(count);
  expect(views[0].destroy).toHaveBeenCalledOnce();
});

test("disposal flushes a pending batch before releasing its native session", async () => {
  vi.useFakeTimers();
  const { controller, ports, views } = fixture();
  await controller.open("a");
  const response = deferred<{ edit_session_id: string; generation: string }>();
  vi.mocked(ports.saveDocument).mockReturnValue(response.promise);
  views[0].options.change("pending batch");
  const disposed = controller.dispose();
  await vi.advanceTimersByTimeAsync(500);
  expect(ports.saveDocument).toHaveBeenCalledOnce();
  expect(ports.closeDocument).not.toHaveBeenCalled();
  response.resolve({ edit_session_id: "session-a", generation: "1" });
  await disposed;
  expect(ports.closeDocument).toHaveBeenCalledExactlyOnceWith({ document_id: "a", edit_session_id: "session-a" });
});

test("a successful switch preserves the final draft and ignores callbacks from the old view", async () => {
  const { controller, ports, views, states } = fixture();
  await controller.open("a");
  views[0].options.change("last input in a");
  await controller.open("b");
  expect(ports.saveDocument).toHaveBeenCalledExactlyOnceWith({ document_id: "a", edit_session_id: "session-a", generation: "1", bytes_base64: btoa("last input in a") });
  expect(states.at(-1)?.selected).toBe("b");
  views[0].options.change("obsolete callback");
  await controller.flush();
  expect(ports.saveDocument).toHaveBeenCalledOnce();
  views[1].options.change("new input in b");
  await controller.flush();
  expect(ports.saveDocument).toHaveBeenLastCalledWith({ document_id: "b", edit_session_id: "session-b", generation: "1", bytes_base64: btoa("new input in b") });
  await controller.dispose();
});

test("composition prevents document switches and exit without forcing the editor to stop", async () => {
  const { controller, ports, views } = fixture();
  await controller.open("a");
  views[0].composing = true;
  await controller.open("b");
  await controller.close();
  expect(ports.openDocument).toHaveBeenCalledTimes(1);
  expect(ports.exit).not.toHaveBeenCalled();
  expect(views[0].editable).toBe(true);
  views[0].composing = false;
  await controller.dispose();
});
