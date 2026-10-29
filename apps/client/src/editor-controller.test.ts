import { afterEach, expect, test, vi } from "vitest";
import { EditorController, type EditorOptions, type EditorPorts, type EditorState } from "./editor-controller";

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason: unknown) => void;
  const promise = new Promise<T>((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}

const opened = (id: string) => ({ document_id: id, edit_session_id: `session-${id}`, generation: "0", bytes_base64: btoa("base") });

function fixture() {
  const states: EditorState[] = [];
  const views: Array<{ options: EditorOptions; editable: boolean; composing: boolean; show: ReturnType<typeof vi.fn>; destroy: ReturnType<typeof vi.fn>; replaceSource: ReturnType<typeof vi.fn> }> = [];
  const ports: EditorPorts = {
    openDocument: vi.fn(async (id) => opened(id)),
    saveDocument: vi.fn(async (request) => ({ edit_session_id: request.edit_session_id, generation: request.generation })),
    closeDocument: vi.fn(async () => {}),
    exit: vi.fn(async () => {}),
    createEditor: vi.fn((options) => {
      const view = { options, editable: true, composing: false, show: vi.fn(), destroy: vi.fn(), replaceSource: vi.fn() };
      views.push(view);
      return {
        show: view.show,
        destroy: view.destroy,
        setEditable: (enabled: boolean) => { view.editable = enabled; },
        isComposing: () => view.composing,
        replaceSource: view.replaceSource,
      };
    }),
    stateChanged: (state) => { states.push(state); },
  };
  return { controller: new EditorController(ports), ports, states, views };
}

afterEach(() => { vi.useRealTimers(); });

function inspectRemote(controller: EditorController): Promise<void> {
  return (controller as EditorController & { inspectRemoteChange(): Promise<void> }).inspectRemoteChange();
}

function remoteChanged(states: EditorState[]): boolean | undefined {
  return (states.at(-1) as (EditorState & { remoteChanged?: boolean }) | undefined)?.remoteChanged;
}

test("remote change notice preserves active composition, local input, and existing notice", async () => {
  const { controller, ports, views, states } = fixture();
  vi.mocked(ports.openDocument).mockResolvedValueOnce({ ...opened("a"), bytes_base64: btoa("base\r\n") });
  const inspect = vi.fn(async () => "changed");
  Object.assign(ports, { inspectRemoteChange: inspect });
  await controller.open("a");
  const notice = states.at(-1)?.notice;
  expect(notice).toContain("LF");
  views[0].options.change("local input");
  views[0].composing = true;
  await inspectRemote(controller);
  expect(inspect).toHaveBeenCalledExactlyOnceWith("a", "session-a", "1");
  expect(remoteChanged(states)).toBe(true);
  expect(states.at(-1)?.notice).toBe(notice);
  expect(ports.saveDocument).not.toHaveBeenCalled();
  views[0].composing = false;
  expect(new TextDecoder().decode(controller.recoverySnapshot().bytes)).toBe("local input");
  await controller.dispose();
});

test("remote notice checks a durable dirty draft without changing its checkpoint or generation", async () => {
  const { controller, ports, views, states } = fixture();
  Object.assign(ports, { inspectRemoteChange: vi.fn(async () => "changed") });
  await controller.open("a");
  views[0].options.change("durable local");
  await controller.flush();
  const snapshot = controller.recoverySnapshot();
  const calls = vi.mocked(ports.saveDocument).mock.calls.length;
  await inspectRemote(controller);
  expect(remoteChanged(states)).toBe(true);
  expect(controller.recoverySnapshot()).toEqual(snapshot);
  expect(ports.saveDocument).toHaveBeenCalledTimes(calls);
  await controller.dispose();
});

test("pending or failed remote checks do not clear an existing notice", async () => {
  const { controller, ports, states } = fixture();
  Object.assign(ports, { inspectRemoteChange: vi.fn().mockResolvedValueOnce("changed").mockResolvedValueOnce("pending").mockRejectedValueOnce(new Error("temporary failure")) });
  await controller.open("a");
  await inspectRemote(controller);
  expect(remoteChanged(states)).toBe(true);
  await inspectRemote(controller);
  expect(remoteChanged(states)).toBe(true);
  await inspectRemote(controller);
  expect(remoteChanged(states)).toBe(true);
  await controller.dispose();
});

test("a remote result for the previous session is discarded and the current session is checked", async () => {
  const { controller, ports, states } = fixture();
  const old = deferred<"changed" | "same" | "pending">();
  const inspect = vi.fn().mockReturnValueOnce(old.promise).mockResolvedValue("same");
  Object.assign(ports, { inspectRemoteChange: inspect });
  await controller.open("a");
  const checking = inspectRemote(controller);
  await controller.open("b");
  old.resolve("changed");
  await checking;
  expect(remoteChanged(states)).not.toBe(true);
  expect(inspect).toHaveBeenLastCalledWith("b", "session-b", "0");
  await controller.dispose();
});

test("a remote result for an old generation is discarded and rechecked", async () => {
  const { controller, ports, views, states } = fixture();
  const old = deferred<"changed" | "same" | "pending">();
  const inspect = vi.fn().mockReturnValueOnce(old.promise).mockResolvedValue("changed");
  Object.assign(ports, { inspectRemoteChange: inspect });
  await controller.open("a");
  views[0].options.change("one");
  const checking = inspectRemote(controller);
  views[0].options.change("one plus two");
  old.resolve("same");
  await checking;
  expect(inspect).toHaveBeenNthCalledWith(1, "a", "session-a", "1");
  expect(inspect).toHaveBeenLastCalledWith("a", "session-a", "2");
  expect(remoteChanged(states)).toBe(true);
  expect(controller.recoverySnapshot().generation).toBe("2");
  expect(new TextDecoder().decode(controller.recoverySnapshot().bytes)).toBe("one plus two");
  await controller.dispose();
});

test("remote refresh keeps input enabled during fetch and rejects a candidate after an edit", async () => {
  const { controller, ports, views } = fixture();
  const candidate = deferred<{ token: string; bytes_base64: string } | null>();
  ports.prepareRemoteRefresh = vi.fn(() => candidate.promise);
  ports.commitRemoteRefresh = vi.fn(async () => true);
  await controller.open("a");
  const refresh = controller.refreshRemote();
  expect(views[0].editable).toBe(true);
  views[0].options.change("local edit");
  candidate.resolve({ token: "candidate", bytes_base64: btoa("remote") });
  await refresh;
  expect(ports.commitRemoteRefresh).not.toHaveBeenCalled();
  expect(views[0].replaceSource).not.toHaveBeenCalled();
  expect(new TextDecoder().decode(controller.recoverySnapshot().bytes)).toBe("local edit");
  await controller.dispose();
});

test("remote refresh rejects a response after document switch or composition starts", async () => {
  for (const invalidation of ["switch", "composition"] as const) {
    const { controller, ports, views } = fixture();
    const candidate = deferred<{ token: string; bytes_base64: string } | null>();
    ports.prepareRemoteRefresh = vi.fn(() => candidate.promise);
    ports.commitRemoteRefresh = vi.fn(async () => true);
    await controller.open("a");
    const refresh = controller.refreshRemote();
    if (invalidation === "switch") await controller.open("b");
    else views[0].composing = true;
    candidate.resolve({ token: "candidate", bytes_base64: btoa("remote") });
    await refresh;
    expect(ports.commitRemoteRefresh).not.toHaveBeenCalled();
    expect(views[0].replaceSource).not.toHaveBeenCalled();
    views[0].composing = false;
    await controller.dispose();
  }
});

test("a metadata notification during refresh retries once after a stale candidate", async () => {
  const { controller, ports, views } = fixture();
  const first = deferred<{ token: string; bytes_base64: string } | null>();
  ports.prepareRemoteRefresh = vi.fn().mockReturnValueOnce(first.promise)
    .mockResolvedValueOnce({ token: "latest", bytes_base64: btoa("latest") });
  ports.commitRemoteRefresh = vi.fn().mockResolvedValueOnce(false).mockResolvedValueOnce(true);
  await controller.open("a");
  const refresh = controller.refreshRemote();
  const next = controller.refreshRemote();
  first.resolve({ token: "stale", bytes_base64: btoa("old") });
  await Promise.all([refresh, next]);
  expect(ports.prepareRemoteRefresh).toHaveBeenCalledTimes(2);
  expect(ports.commitRemoteRefresh).toHaveBeenCalledTimes(2);
  expect(views[0].replaceSource).toHaveBeenCalledOnce();
  expect(new TextDecoder().decode(controller.recoverySnapshot().bytes)).toBe("latest");
  await controller.dispose();
});

test("remote refresh locks the editor during commit and updates the source without a local save", async () => {
  const { controller, ports, views } = fixture();
  const commit = deferred<boolean>();
  ports.prepareRemoteRefresh = vi.fn(async () => ({ token: "candidate", bytes_base64: btoa("remote") }));
  ports.commitRemoteRefresh = vi.fn(() => commit.promise);
  await controller.open("a");
  const refresh = controller.refreshRemote();
  await vi.waitFor(() => expect(ports.commitRemoteRefresh).toHaveBeenCalledOnce());
  expect(views[0].editable).toBe(false);
  await controller.open("b");
  expect(ports.openDocument).toHaveBeenCalledTimes(1);
  commit.resolve(true);
  await refresh;
  expect(views[0].replaceSource).toHaveBeenCalledOnce();
  expect(new TextDecoder().decode(controller.recoverySnapshot().bytes)).toBe("remote");
  expect(ports.saveDocument).not.toHaveBeenCalled();
  expect(views[0].editable).toBe(true);
  await controller.dispose();
});

test("a rejected remote commit retains the original editable document", async () => {
  const { controller, ports, views } = fixture();
  ports.prepareRemoteRefresh = vi.fn(async () => ({ token: "candidate", bytes_base64: btoa("remote") }));
  ports.commitRemoteRefresh = vi.fn(async () => false);
  await controller.open("a");
  await controller.refreshRemote();
  expect(views[0].replaceSource).not.toHaveBeenCalled();
  expect(new TextDecoder().decode(controller.recoverySnapshot().bytes)).toBe("base");
  expect(views[0].editable).toBe(true);
  await controller.dispose();
});

test.each(["unknown commit", "view failure"])("%s never re-enables the old text", async (failure) => {
  const { controller, ports, views, states } = fixture();
  ports.prepareRemoteRefresh = vi.fn(async () => ({ token: "candidate", bytes_base64: btoa("remote") }));
  ports.commitRemoteRefresh = failure === "unknown commit" ? vi.fn(async () => { throw new Error("lost reply"); }) : vi.fn(async () => true);
  await controller.open("a");
  if (failure === "view failure") views[0].replaceSource.mockImplementation(() => { throw new Error("view failure"); });
  await controller.refreshRemote();
  expect(views[0].editable).toBe(false);
  expect(states.at(-1)?.error).toContain("다시 열어");
  await controller.open("b");
  expect(ports.openDocument).toHaveBeenCalledTimes(1);
  await controller.save();
  expect(ports.saveDocument).not.toHaveBeenCalled();
  await controller.dispose();
});

test("manual save flushes the current edit before prompting server transmission", async () => {
  const { controller, ports, views } = fixture();
  const manualSync = vi.fn(async () => {});
  Object.assign(ports, { manualSync });
  await controller.open("a");
  views[0].options.change("latest");
  views[0].options.save();
  await vi.waitFor(() => expect(manualSync).toHaveBeenCalledExactlyOnceWith("a", "session-a"));
  expect(ports.saveDocument).toHaveBeenCalledExactlyOnceWith({ document_id: "a", edit_session_id: "session-a", generation: "1", bytes_base64: btoa("latest") });
  await controller.dispose();
});

test("manual save does not transmit an uncommitted checkpoint", async () => {
  const { controller, ports, views, states } = fixture();
  ports.manualSync = vi.fn(async () => {});
  vi.mocked(ports.saveDocument).mockRejectedValueOnce(new Error("disk unavailable"));
  await controller.open("a");
  views[0].options.change("unsaved");
  await expect(controller.save()).rejects.toThrow("disk unavailable");
  expect(ports.manualSync).not.toHaveBeenCalled();
  expect(states.at(-1)?.status).toBe("저장 오류, 입력 유지 중");
  expect(new TextDecoder().decode(controller.recoverySnapshot().bytes)).toBe("unsaved");
  await controller.dispose();
});

test("manual save waits for composition to finish", async () => {
  const { controller, ports, views, states } = fixture();
  ports.manualSync = vi.fn(async () => {});
  await controller.open("a");
  views[0].options.change("조합 중");
  views[0].composing = true;
  await controller.save();
  expect(ports.saveDocument).not.toHaveBeenCalled();
  expect(ports.manualSync).not.toHaveBeenCalled();
  expect(states.at(-1)?.error).toContain("조합을 마친 뒤");
  views[0].composing = false;
  await controller.dispose();
});

test("a rejected edit is reported without replacing the current draft", async () => {
  const { controller, views, states } = fixture();
  await controller.open("a");
  views[0].options.inputRejected();
  expect(states.at(-1)?.error).toContain("입력은 적용되지 않았습니다");
  expect(new TextDecoder().decode(controller.recoverySnapshot().bytes)).toBe("base");
  await controller.dispose();
});

test("recovery snapshot keeps failed input in its current session and waits for composition", async () => {
  const { controller, ports, views } = fixture();
  vi.mocked(ports.saveDocument).mockRejectedValueOnce(new Error("disk"));
  await controller.open("a");
  views[0].options.change("미저장");
  await expect(controller.flush()).rejects.toThrow("disk");
  const snapshot = controller.recoverySnapshot();
  expect(snapshot.id).toBe("a");
  expect(snapshot.session).toBe("session-a");
  expect(controller.isCurrentSnapshot(snapshot.session, snapshot.generation, snapshot.revision)).toBe(true);
  expect(new TextDecoder().decode(snapshot.bytes)).toBe("미저장");
  views[0].options.change("추가 입력");
  expect(controller.isCurrentSnapshot(snapshot.session, snapshot.generation, snapshot.revision)).toBe(false);
  expect(controller.isCurrentSession(snapshot.session)).toBe(true);
  views[0].composing = true;
  expect(() => controller.recoverySnapshot()).toThrow("조합");
  views[0].composing = false;
  await controller.dispose();
});

test("remote source replacement invalidates an earlier snapshot even when session and generation are unchanged", async () => {
  const { controller, ports, views } = fixture();
  ports.prepareRemoteRefresh = vi.fn(async () => ({ token: "candidate", bytes_base64: btoa("[new](new.md)") }));
  ports.commitRemoteRefresh = vi.fn(async () => true);
  vi.mocked(ports.openDocument).mockResolvedValueOnce({ ...opened("a"), bytes_base64: btoa("[old](old.md)") });
  await controller.open("a");
  const snapshot = controller.recoverySnapshot() as ReturnType<EditorController["recoverySnapshot"]> & { revision: number };
  const isCurrent = () => (controller.isCurrentSnapshot as (session: string, generation: string, revision: number) => boolean)(snapshot.session, snapshot.generation, snapshot.revision);
  expect(isCurrent()).toBe(true);
  await controller.refreshRemote();
  const refreshed = controller.recoverySnapshot();
  expect(refreshed.session).toBe(snapshot.session);
  expect(refreshed.generation).toBe(snapshot.generation);
  expect(new TextDecoder().decode(refreshed.bytes)).toBe("[new](new.md)");
  expect(views[0].replaceSource).toHaveBeenCalledOnce();
  expect(isCurrent()).toBe(false);
  await controller.dispose();
});

test("starting IME composition invalidates snapshot use before a text change", async () => {
  const { controller, views } = fixture();
  await controller.open("a");
  const snapshot = controller.recoverySnapshot() as ReturnType<EditorController["recoverySnapshot"]> & { revision: number };
  const isCurrent = () => (controller.isCurrentSnapshot as (session: string, generation: string, revision: number) => boolean)(snapshot.session, snapshot.generation, snapshot.revision);
  expect(isCurrent()).toBe(true);
  views[0].composing = true;
  expect(controller.isCurrentSession(snapshot.session)).toBe(true);
  expect(isCurrent()).toBe(false);
  views[0].composing = false;
  await controller.dispose();
});

test("automatic local save starts one second after the latest input", async () => {
  vi.useFakeTimers();
  const { controller, ports, views } = fixture();
  await controller.open("a");
  views[0].options.change("first");
  await vi.advanceTimersByTimeAsync(900);
  views[0].options.change("latest");
  await vi.advanceTimersByTimeAsync(999);
  expect(ports.saveDocument).not.toHaveBeenCalled();
  await vi.advanceTimersByTimeAsync(1);
  expect(ports.saveDocument).toHaveBeenCalledExactlyOnceWith({ document_id: "a", edit_session_id: "session-a", generation: "2", bytes_base64: btoa("latest") });
  await controller.dispose();
});

test("continuous input starts a local checkpoint after five seconds", async () => {
  vi.useFakeTimers();
  const { controller, ports, views } = fixture();
  await controller.open("a");
  views[0].options.change("0");
  for (let i = 1; i <= 9; i++) {
    await vi.advanceTimersByTimeAsync(500);
    views[0].options.change(String(i));
  }
  await vi.advanceTimersByTimeAsync(499);
  expect(ports.saveDocument).not.toHaveBeenCalled();
  await vi.advanceTimersByTimeAsync(1);
  expect(ports.saveDocument).toHaveBeenCalledExactlyOnceWith({ document_id: "a", edit_session_id: "session-a", generation: "10", bytes_base64: btoa("9") });
  await controller.dispose();
});

test("input during an automatic checkpoint waits for its own idle deadline", async () => {
  vi.useFakeTimers();
  const { controller, ports, views, states } = fixture();
  await controller.open("a");
  const response = deferred<{ edit_session_id: string; generation: string }>();
  vi.mocked(ports.saveDocument).mockReturnValueOnce(response.promise);
  views[0].options.change("first");
  await vi.advanceTimersByTimeAsync(1000);
  expect(ports.saveDocument).toHaveBeenCalledTimes(1);
  views[0].options.change("second");
  await vi.advanceTimersByTimeAsync(100);
  response.resolve({ edit_session_id: "session-a", generation: "1" });
  await vi.advanceTimersByTimeAsync(0);
  expect(ports.saveDocument).toHaveBeenCalledTimes(1);
  expect(states.at(-1)?.status).not.toBe("이 기기에 저장됨");
  await vi.advanceTimersByTimeAsync(899);
  expect(ports.saveDocument).toHaveBeenCalledTimes(1);
  await vi.advanceTimersByTimeAsync(1);
  expect(ports.saveDocument).toHaveBeenCalledTimes(2);
  expect(ports.saveDocument).toHaveBeenLastCalledWith({ document_id: "a", edit_session_id: "session-a", generation: "2", bytes_base64: btoa("second") });
  await controller.dispose();
});

test("an overdue five-second checkpoint waits for the prior write, then saves the newest input", async () => {
  vi.useFakeTimers();
  const { controller, ports, views } = fixture();
  await controller.open("a");
  const response = deferred<{ edit_session_id: string; generation: string }>();
  vi.mocked(ports.saveDocument).mockReturnValueOnce(response.promise);
  views[0].options.change("first");
  await vi.advanceTimersByTimeAsync(1000);
  for (let i = 1; i <= 11; i++) {
    await vi.advanceTimersByTimeAsync(500);
    views[0].options.change(`later-${i}`);
  }
  expect(ports.saveDocument).toHaveBeenCalledTimes(1);
  response.resolve({ edit_session_id: "session-a", generation: "1" });
  await vi.advanceTimersByTimeAsync(1);
  expect(ports.saveDocument).toHaveBeenCalledTimes(2);
  expect(ports.saveDocument).toHaveBeenLastCalledWith({ document_id: "a", edit_session_id: "session-a", generation: "12", bytes_base64: btoa("later-11") });
  await controller.dispose();
});

test("switching flushes immediately and does not reuse the old document's timer", async () => {
  vi.useFakeTimers();
  const { controller, ports, views } = fixture();
  await controller.open("a");
  views[0].options.change("a-input");
  await vi.advanceTimersByTimeAsync(500);
  await controller.open("b");
  expect(ports.saveDocument).toHaveBeenCalledExactlyOnceWith({ document_id: "a", edit_session_id: "session-a", generation: "1", bytes_base64: btoa("a-input") });
  views[1].options.change("b-input");
  await vi.advanceTimersByTimeAsync(999);
  expect(ports.saveDocument).toHaveBeenCalledTimes(1);
  await vi.advanceTimersByTimeAsync(1);
  expect(ports.saveDocument).toHaveBeenCalledTimes(2);
  expect(ports.saveDocument).toHaveBeenLastCalledWith({ document_id: "b", edit_session_id: "session-b", generation: "1", bytes_base64: btoa("b-input") });
  await controller.dispose();
});

test("manual save waits for an in-flight automatic checkpoint and flushes later edits", async () => {
  vi.useFakeTimers();
  const { controller, ports, views } = fixture();
  ports.manualSync = vi.fn(async () => {});
  await controller.open("a");
  const response = deferred<{ edit_session_id: string; generation: string }>();
  vi.mocked(ports.saveDocument).mockReturnValueOnce(response.promise);
  views[0].options.change("first");
  await vi.advanceTimersByTimeAsync(1000);
  views[0].options.change("second");
  const saving = controller.save();
  expect(ports.saveDocument).toHaveBeenCalledTimes(1);
  response.resolve({ edit_session_id: "session-a", generation: "1" });
  await saving;
  expect(ports.saveDocument).toHaveBeenCalledTimes(2);
  expect(ports.saveDocument).toHaveBeenLastCalledWith({ document_id: "a", edit_session_id: "session-a", generation: "2", bytes_base64: btoa("second") });
  expect(ports.manualSync).toHaveBeenCalledExactlyOnceWith("a", "session-a");
  await vi.advanceTimersByTimeAsync(5000);
  expect(ports.saveDocument).toHaveBeenCalledTimes(2);
  await controller.dispose();
});

test.each(["stale_session", "StaleSession", "invalid_generation", "InvalidGeneration", "document_too_large", "UnsupportedFormat", "unknown"]) ("terminal save error %s does not retry even after another edit", async (failure) => {
  vi.useFakeTimers();
  const { controller, ports, views, states } = fixture();
  vi.mocked(ports.saveDocument).mockRejectedValueOnce(failure);
  await controller.open("a");
  views[0].options.change("first");
  await vi.advanceTimersByTimeAsync(1000);
  views[0].options.change("second");
  await vi.advanceTimersByTimeAsync(60_000);
  expect(ports.saveDocument).toHaveBeenCalledTimes(1);
  expect(states.at(-1)?.status).toBe("저장 오류, 입력 유지 중");
  expect(states.at(-1)?.error).toContain(failure);
  expect(new TextDecoder().decode(controller.recoverySnapshot().bytes)).toBe("second");
  await controller.dispose();
});

test("one failed checkpoint shared by automatic and manual saves schedules one backoff", async () => {
  vi.useFakeTimers();
  const { controller, ports, views } = fixture();
  await controller.open("a");
  const response = deferred<{ edit_session_id: string; generation: string }>();
  vi.mocked(ports.saveDocument).mockReturnValueOnce(response.promise);
  views[0].options.change("first");
  await vi.advanceTimersByTimeAsync(1000);
  const saving = controller.flush();
  response.reject("Storage");
  await expect(saving).rejects.toBe("Storage");
  await vi.advanceTimersByTimeAsync(999);
  expect(ports.saveDocument).toHaveBeenCalledTimes(1);
  await vi.advanceTimersByTimeAsync(1);
  expect(ports.saveDocument).toHaveBeenCalledTimes(2);
  await controller.dispose();
});

test("failed automatic checkpoints retry with bounded backoff and preserve the draft", async () => {
  vi.useFakeTimers();
  const { controller, ports, views, states } = fixture();
  vi.mocked(ports.saveDocument).mockRejectedValueOnce("Storage").mockRejectedValueOnce("Storage");
  await controller.open("a");
  views[0].options.change("first");
  await vi.advanceTimersByTimeAsync(1000);
  expect(states.at(-1)?.status).toBe("저장 오류, 입력 유지 중");
  expect(new TextDecoder().decode(controller.recoverySnapshot().bytes)).toBe("first");
  await vi.advanceTimersByTimeAsync(999);
  expect(ports.saveDocument).toHaveBeenCalledTimes(1);
  await vi.advanceTimersByTimeAsync(1);
  expect(ports.saveDocument).toHaveBeenCalledTimes(2);
  await vi.advanceTimersByTimeAsync(1999);
  expect(ports.saveDocument).toHaveBeenCalledTimes(2);
  await vi.advanceTimersByTimeAsync(1);
  expect(ports.saveDocument).toHaveBeenCalledTimes(3);
  expect(ports.saveDocument).toHaveBeenLastCalledWith({ document_id: "a", edit_session_id: "session-a", generation: "1", bytes_base64: btoa("first") });
  expect(states.at(-1)?.status).toBe("이 기기에 저장됨");
  await controller.dispose();
});

test("persistent local storage failures stop increasing backoff at thirty seconds", async () => {
  vi.useFakeTimers();
  const { controller, ports, views } = fixture();
  vi.mocked(ports.saveDocument).mockRejectedValue("Storage");
  await controller.open("a");
  views[0].options.change("unsent");
  await vi.advanceTimersByTimeAsync(1000);
  for (const delay of [1000, 2000, 4000, 8000, 16000]) await vi.advanceTimersByTimeAsync(delay);
  expect(ports.saveDocument).toHaveBeenCalledTimes(6);
  await vi.advanceTimersByTimeAsync(29_999);
  expect(ports.saveDocument).toHaveBeenCalledTimes(6);
  await vi.advanceTimersByTimeAsync(1);
  expect(ports.saveDocument).toHaveBeenCalledTimes(7);
  vi.mocked(ports.saveDocument).mockImplementation(async (request) => ({ edit_session_id: request.edit_session_id, generation: request.generation }));
  await controller.dispose();
});

test("new input after a failed checkpoint still waits for one second of inactivity", async () => {
  vi.useFakeTimers();
  const { controller, ports, views } = fixture();
  vi.mocked(ports.saveDocument).mockRejectedValueOnce("Storage");
  await controller.open("a");
  views[0].options.change("first");
  await vi.advanceTimersByTimeAsync(1000);
  await vi.advanceTimersByTimeAsync(500);
  views[0].options.change("second");
  await vi.advanceTimersByTimeAsync(999);
  expect(ports.saveDocument).toHaveBeenCalledTimes(1);
  await vi.advanceTimersByTimeAsync(1);
  expect(ports.saveDocument).toHaveBeenCalledTimes(2);
  expect(ports.saveDocument).toHaveBeenLastCalledWith({ document_id: "a", edit_session_id: "session-a", generation: "2", bytes_base64: btoa("second") });
  await controller.dispose();
});

test("manual save bypasses local retry backoff and resets it after success", async () => {
  vi.useFakeTimers();
  const { controller, ports, views } = fixture();
  ports.manualSync = vi.fn(async () => {});
  vi.mocked(ports.saveDocument).mockRejectedValueOnce("Storage");
  await controller.open("a");
  views[0].options.change("first");
  await vi.advanceTimersByTimeAsync(1000);
  await controller.save();
  expect(ports.saveDocument).toHaveBeenCalledTimes(2);
  expect(ports.manualSync).toHaveBeenCalledOnce();
  vi.mocked(ports.saveDocument).mockRejectedValueOnce("Storage");
  views[0].options.change("second");
  await vi.advanceTimersByTimeAsync(1000);
  expect(ports.saveDocument).toHaveBeenCalledTimes(3);
  await vi.advanceTimersByTimeAsync(999);
  expect(ports.saveDocument).toHaveBeenCalledTimes(3);
  await vi.advanceTimersByTimeAsync(1);
  expect(ports.saveDocument).toHaveBeenCalledTimes(4);
  await controller.dispose();
});

test("new input can retry after an in-flight automatic checkpoint fails", async () => {
  vi.useFakeTimers();
  const { controller, ports, views } = fixture();
  await controller.open("a");
  const response = deferred<{ edit_session_id: string; generation: string }>();
  vi.mocked(ports.saveDocument).mockReturnValueOnce(response.promise);
  views[0].options.change("first");
  await vi.advanceTimersByTimeAsync(1000);
  views[0].options.change("later input");
  await vi.advanceTimersByTimeAsync(1000);
  expect(ports.saveDocument).toHaveBeenCalledTimes(1);
  response.reject("Storage");
  await vi.advanceTimersByTimeAsync(0);
  await vi.advanceTimersByTimeAsync(999);
  expect(ports.saveDocument).toHaveBeenCalledTimes(1);
  await vi.advanceTimersByTimeAsync(1);
  expect(ports.saveDocument).toHaveBeenCalledTimes(2);
  expect(ports.saveDocument).toHaveBeenLastCalledWith({ document_id: "a", edit_session_id: "session-a", generation: "2", bytes_base64: btoa("later input") });
  await vi.advanceTimersByTimeAsync(5000);
  expect(ports.saveDocument).toHaveBeenCalledTimes(2);
  await controller.dispose();
});

test("input due during a failed manual checkpoint is retried without another edit", async () => {
  vi.useFakeTimers();
  const { controller, ports, views, states } = fixture();
  await controller.open("a");
  const response = deferred<{ edit_session_id: string; generation: string }>();
  vi.mocked(ports.saveDocument).mockReturnValueOnce(response.promise);
  views[0].options.change("first");
  const saving = controller.flush();
  views[0].options.change("later input");
  await vi.advanceTimersByTimeAsync(1000);
  expect(ports.saveDocument).toHaveBeenCalledTimes(1);
  response.reject("Storage");
  await expect(saving).rejects.toBe("Storage");
  expect(states.at(-1)?.status).toBe("저장 오류, 입력 유지 중");
  await vi.advanceTimersByTimeAsync(999);
  expect(ports.saveDocument).toHaveBeenCalledTimes(1);
  await vi.advanceTimersByTimeAsync(1);
  expect(ports.saveDocument).toHaveBeenCalledTimes(2);
  expect(ports.saveDocument).toHaveBeenLastCalledWith({ document_id: "a", edit_session_id: "session-a", generation: "2", bytes_base64: btoa("later input") });
  await controller.dispose();
});

test("automatic local checkpoints do not interrupt an active composition", async () => {
  vi.useFakeTimers();
  const { controller, ports, views } = fixture();
  await controller.open("a");
  views[0].composing = true;
  views[0].options.composition(true);
  views[0].options.change("한글");
  await vi.advanceTimersByTimeAsync(1000);
  expect(ports.saveDocument).toHaveBeenCalledOnce();
  expect(views[0].composing).toBe(true);
  expect(views[0].editable).toBe(true);
  views[0].composing = false;
  views[0].options.composition(false);
  await controller.dispose();
});

test("continuous edits do not repeat the same save status and preserve the latest generation", async () => {
  const { controller, ports, states, views } = fixture();
  const edited = vi.fn();
  ports.edited = edited;
  await controller.open("a");
  views[0].options.change("first");
  const notifications = states.length;
  views[0].options.change("second");
  expect(states).toHaveLength(notifications);
  expect(edited).toHaveBeenLastCalledWith("a", "session-a", "2");
  await controller.flush();
  expect(ports.saveDocument).toHaveBeenLastCalledWith({ document_id: "a", edit_session_id: "session-a", generation: "2", bytes_base64: btoa("second") });
  await controller.dispose();
});

test("saving an unchanged document does not claim a local checkpoint", async () => {
  const { controller, ports, states } = fixture();
  await controller.open("a");
  await controller.flush();
  expect(ports.saveDocument).not.toHaveBeenCalled();
  expect(states.at(-1)?.status).toBe("수정 없음");
  await controller.dispose();
});

test("transmission receives the actual edit session and each edit generation", async () => {
  const { controller, ports, views } = fixture();
  const edited = vi.fn();
  ports.edited = edited;
  await controller.open("a");
  views[0].options.change("one");
  views[0].options.change("two");
  expect(edited).toHaveBeenNthCalledWith(1, "a", "session-a", "1");
  expect(edited).toHaveBeenNthCalledWith(2, "a", "session-a", "2");
  await controller.dispose();
});

test("composition events are bound to the active edit session", async () => {
  const { controller, ports, views } = fixture();
  const compositionChanged = vi.fn();
  ports.compositionChanged = compositionChanged;
  await controller.open("a");
  views[0].options.composition(true);
  views[0].options.composition(false);
  expect(compositionChanged).toHaveBeenNthCalledWith(1, "a", "session-a", true);
  expect(compositionChanged).toHaveBeenNthCalledWith(2, "a", "session-a", false);
  await controller.open("b");
  views[0].options.composition(true);
  expect(compositionChanged).toHaveBeenCalledTimes(2);
  await controller.dispose();
});

test("server acknowledgement is current only after the latest edit is durable", async () => {
  const { controller, views } = fixture();
  await controller.open("a");
  views[0].options.change("one");
  expect(controller.isCurrentSaved("session-a", "1")).toBe(false);
  await controller.flush();
  expect(controller.isCurrentSaved("session-a", "1")).toBe(true);
  views[0].options.change("two");
  expect(controller.isCurrentSaved("session-a", "1")).toBe(false);
  expect(controller.isCurrentSaved("other-session", "2")).toBe(false);
  await controller.dispose();
});

test("opening a durable local draft reports its generation after the session is active", async () => {
  const { controller, ports } = fixture();
  const restored = vi.fn((_, sessionId: string, generation: string) => {
    expect(controller.isCurrentSaved(sessionId, generation)).toBe(true);
  });
  ports.opened = restored;
  vi.mocked(ports.openDocument).mockResolvedValueOnce({ ...opened("draft"), generation: "3", local_draft: true });
  await controller.open("draft");
  expect(restored).toHaveBeenCalledExactlyOnceWith("draft", "session-draft", "3", true);
  await controller.dispose();
});

test("preview receives only the active document and its subsequent edits", async () => {
  const { controller, ports, views } = fixture();
  const sourceChanged = vi.fn();
  ports.sourceChanged = sourceChanged;
  await controller.open("a");
  expect(sourceChanged).toHaveBeenLastCalledWith("base", true);
  views[0].options.change("updated");
  expect(sourceChanged).toHaveBeenLastCalledWith("updated", false);
  await controller.open("b");
  expect(sourceChanged).toHaveBeenLastCalledWith("base", true);
  views[0].options.change("stale view");
  expect(sourceChanged).toHaveBeenLastCalledWith("base", true);
  await controller.dispose();
});

test("workspace change flushes and closes the editor before pairing", async () => {
  const { controller, ports, views, states } = fixture();
  await controller.open("a");
  views[0].options.change("pending");
  await expect(controller.prepareForWorkspaceChange()).resolves.toBe(true);
  expect(ports.saveDocument).toHaveBeenCalledOnce();
  expect(ports.closeDocument).toHaveBeenCalledExactlyOnceWith({ document_id: "a", edit_session_id: "session-a" });
  expect(views[0].destroy).toHaveBeenCalledOnce();
  expect(states.at(-1)?.selected).toBe("");
  await controller.dispose();
});

test("workspace change does not discard input when flush fails", async () => {
  const { controller, ports, views, states } = fixture();
  await controller.open("a");
  views[0].options.change("pending");
  vi.mocked(ports.saveDocument).mockRejectedValueOnce(new Error("disk"));
  await expect(controller.prepareForWorkspaceChange()).resolves.toBe(false);
  expect(ports.closeDocument).not.toHaveBeenCalled();
  expect(views[0].destroy).not.toHaveBeenCalled();
  expect(views[0].editable).toBe(true);
  expect(states.at(-1)?.selected).toBe("a");
  await controller.dispose();
});

test("oversized UTF-8 documents can be opened read-only without saving", async () => {
  const { controller, ports, views, states } = fixture();
  const bytes = btoa("x".repeat(5 * 1024 * 1024 + 2));
  vi.mocked(ports.openDocument).mockResolvedValueOnce({ ...opened("large"), bytes_base64: bytes });
  await controller.open("large");
  expect(states.at(-1)?.selected).toBe("large");
  expect(views[0].options.source.readonly).toBe(true);
  expect(views[0].editable).toBe(false);
  await controller.flush();
  expect(ports.saveDocument).not.toHaveBeenCalled();
  await controller.dispose();
});

test("native read-only decisions cannot be overridden by an editable source", async () => {
  const { controller, ports, views } = fixture();
  vi.mocked(ports.openDocument).mockResolvedValueOnce({ ...opened("restricted"), readonly: true });
  await controller.open("restricted");
  expect(views[0].options.source.readonly).toBe(true);
  expect(views[0].editable).toBe(false);
  views[0].options.change("must not save");
  await controller.flush();
  expect(ports.saveDocument).not.toHaveBeenCalled();
  await controller.dispose();
});

test("a recovered local draft is not described as a confirmed server version", async () => {
  const { controller, ports, states } = fixture();
  vi.mocked(ports.openDocument).mockResolvedValueOnce({ ...opened("draft"), generation: "2", local_draft: true });
  await controller.open("draft");
  expect(states.at(-1)?.notice).toContain("서버 상태는 아직 확인하지 않았습니다");
  expect(states.at(-1)?.status).toBe("이 기기에 저장됨");
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

test("close requests during a delayed document switch are fulfilled once after the switch", async () => {
  const { controller, ports, views, states } = fixture();
  await controller.open("a");
  views[0].options.change("last input in a");
  const response = deferred<ReturnType<typeof opened>>();
  const started = deferred<void>();
  vi.mocked(ports.openDocument).mockImplementationOnce(() => {
    started.resolve();
    return response.promise;
  });
  const switching = controller.open("b");
  await started.promise;
  const firstClose = controller.close();
  const repeatedClose = controller.close();
  expect(ports.exit).not.toHaveBeenCalled();
  response.resolve(opened("b"));
  await Promise.all([switching, firstClose, repeatedClose]);
  expect(ports.saveDocument).toHaveBeenCalledExactlyOnceWith({ document_id: "a", edit_session_id: "session-a", generation: "1", bytes_base64: btoa("last input in a") });
  expect(ports.closeDocument).toHaveBeenCalledExactlyOnceWith({ document_id: "a", edit_session_id: "session-a" });
  expect(states.at(-1)?.selected).toBe("b");
  expect(ports.exit).toHaveBeenCalledOnce();
  expect(views[1].editable).toBe(false);
  await controller.dispose();
});

test("a queued close keeps dirty input and its session when switching cannot save", async () => {
  const { controller, ports, views, states } = fixture();
  await controller.open("a");
  views[0].options.change("unsaved input");
  vi.mocked(ports.saveDocument).mockRejectedValue(new Error("disk unavailable"));
  await Promise.all([controller.open("b"), controller.close()]);
  expect(ports.exit).not.toHaveBeenCalled();
  expect(ports.openDocument).toHaveBeenCalledTimes(1);
  expect(ports.closeDocument).not.toHaveBeenCalled();
  expect(states.at(-1)?.selected).toBe("a");
  expect(states.at(-1)?.error).toContain("disk unavailable");
  expect(views[0].editable).toBe(true);

  views[0].options.change("input after failure");
  vi.mocked(ports.saveDocument).mockImplementation(async (request) => ({ edit_session_id: request.edit_session_id, generation: request.generation }));
  await controller.close();
  expect(ports.saveDocument).toHaveBeenLastCalledWith({ document_id: "a", edit_session_id: "session-a", generation: "2", bytes_base64: btoa("input after failure") });
  expect(ports.exit).toHaveBeenCalledOnce();
  await controller.dispose();
});

test("a queued close still exits after the new document fails to open", async () => {
  const { controller, ports, views } = fixture();
  await controller.open("a");
  views[0].options.change("saved before the failed switch");
  const response = deferred<ReturnType<typeof opened>>();
  const started = deferred<void>();
  vi.mocked(ports.openDocument).mockImplementationOnce(() => {
    started.resolve();
    return response.promise;
  });
  const switching = controller.open("b");
  await started.promise;
  const closing = controller.close();
  response.reject(new Error("cannot open b"));
  await Promise.all([switching, closing]);
  expect(ports.closeDocument).not.toHaveBeenCalled();
  expect(ports.exit).toHaveBeenCalledOnce();
  expect(views[0].editable).toBe(false);
  await controller.dispose();
});

test("disposal cancels a queued close before a delayed open response arrives", async () => {
  const { controller, ports, states } = fixture();
  await controller.open("a");
  const response = deferred<ReturnType<typeof opened>>();
  const started = deferred<void>();
  vi.mocked(ports.openDocument).mockImplementationOnce(() => {
    started.resolve();
    return response.promise;
  });
  const switching = controller.open("b");
  await started.promise;
  const closing = controller.close();
  await controller.dispose();
  const count = states.length;
  response.resolve(opened("b"));
  await Promise.all([switching, closing]);
  expect(ports.exit).not.toHaveBeenCalled();
  expect(ports.createEditor).toHaveBeenCalledTimes(1);
  expect(states).toHaveLength(count);
  expect(ports.closeDocument).toHaveBeenNthCalledWith(1, { document_id: "a", edit_session_id: "session-a" });
  expect(ports.closeDocument).toHaveBeenNthCalledWith(2, { document_id: "b", edit_session_id: "session-b" });
});

test("repeated requests during a failed exit do not close a later document switch", async () => {
  const { controller, ports, views } = fixture();
  await controller.open("a");
  const response = deferred<void>();
  const started = deferred<void>();
  vi.mocked(ports.exit).mockImplementationOnce(() => {
    started.resolve();
    return response.promise;
  });
  const closing = controller.close();
  await started.promise;
  const repeatedClose = controller.close();
  response.reject(new Error("exit failed"));
  await Promise.all([closing, repeatedClose]);
  expect(views[0].editable).toBe(true);
  await controller.open("b");
  expect(ports.exit).toHaveBeenCalledTimes(1);
  expect(views[1].editable).toBe(true);
  await controller.close();
  expect(ports.exit).toHaveBeenCalledTimes(2);
  await controller.dispose();
});

test.each([
  ["storage", "기기 캐시에 저장하지 못했습니다"],
  ["damaged", "기기 캐시 상태를 확인해야 합니다"],
  ["unexpected", "기기 캐시 오류를 확인해야 합니다"],
] as const)("opens the document and displays the %s cache warning", async (warning, message) => {
  const { controller, ports, states, views } = fixture();
  vi.mocked(ports.openDocument).mockResolvedValueOnce({ ...opened("a"), cache_warning: warning });
  await controller.open("a");
  expect(states.at(-1)?.selected).toBe("a");
  expect(states.at(-1)?.notice).toContain(message);
  expect(views[0].editable).toBe(true);
  await controller.dispose();
});

test("이전 초안을 보존하면 최신 본문을 읽기 전용으로 여는 이유를 표시한다", async () => {
  const { controller, ports, states, views } = fixture();
  const response = { ...opened("a"), readonly:true, local_draft:false, cache_warning:"storage" as const, open_notice:"prior_draft_preserved" as const };
  vi.mocked(ports.openDocument).mockResolvedValueOnce(response);
  await controller.open("a");
  expect(states.at(-1)?.notice).toContain("이전 초안을 보존해 최신 본문을 읽기 전용으로 표시합니다");
  expect(states.at(-1)?.notice).toContain("기기 캐시에 저장하지 못했습니다");
  expect(views[0].options.source.readonly).toBe(true);
  expect(views[0].editable).toBe(false);
  expect(new TextDecoder().decode(controller.recoverySnapshot().bytes)).toBe("base");
  await controller.dispose();
});

test("reopened confirmed content stays editable and only checkpoints the next edit", async () => {
  const { controller, ports, states, views } = fixture();
  const onOpen = vi.fn();
  ports.opened = onOpen;
  vi.mocked(ports.openDocument).mockResolvedValueOnce({
    ...opened("a"), generation: "1", bytes_base64: btoa("peer content"),
    readonly: false, local_draft: false, open_notice: null,
  });
  await controller.open("a");
  expect(views[0].editable).toBe(true);
  expect(onOpen).toHaveBeenCalledWith("a", "session-a", "1", false);
  expect(states.at(-1)?.notice).not.toContain("읽기 전용");
  await controller.save();
  expect(ports.saveDocument).not.toHaveBeenCalled();
  views[0].options.change("peer content plus local edit");
  await controller.flush();
  expect(ports.saveDocument).toHaveBeenCalledExactlyOnceWith({
    document_id: "a", edit_session_id: "session-a", generation: "2",
    bytes_base64: btoa("peer content plus local edit"),
  });
  await controller.dispose();
});


test("a durable edited session accepts native-approved refresh without resetting generation", async () => {
  const { controller, ports, views } = fixture();
  await controller.open("a");
  views[0].options.change("saved local");
  await controller.flush();
  const prepare = vi.fn(async () => ({ token: "approved", bytes_base64: btoa("remote") }));
  const commit = vi.fn(async () => true);
  Object.assign(ports, { prepareRemoteRefresh: prepare, commitRemoteRefresh: commit });
  await controller.refreshRemote();
  expect(views[0].replaceSource).toHaveBeenCalled();
  expect(controller.recoverySnapshot().generation).toBe("1");
  views[0].options.change("remote plus local");
  await controller.flush();
  expect(ports.saveDocument).toHaveBeenLastCalledWith(expect.objectContaining({ generation: "2", bytes_base64: btoa("remote plus local") }));
  await controller.dispose();
});
