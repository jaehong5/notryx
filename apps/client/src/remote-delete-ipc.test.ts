import { beforeEach, expect, test, vi } from "vitest";
import { inspectWorkspaceRemoteChange } from "./ipc";

const mocked = vi.hoisted(() => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: mocked.invoke }));
const node = "019a1000-0000-7000-8000-000000000002";
beforeEach(() => mocked.invoke.mockReset());

test("원격 삭제 상태를 변경이나 일시 오류와 구별해 전달한다", async () => {
  mocked.invoke.mockImplementation(async (command: string) => command === "m0_renderer_session" ? "owner" : "deleted");
  expect(await inspectWorkspaceRemoteChange(node, "session", "1")).toBe("deleted");
  expect(mocked.invoke).toHaveBeenCalledWith("workspace_inspect_dirty_remote", { request: { renderer_session_id:"owner", document_id:node, edit_session_id:"session", generation:"1" } });
});
