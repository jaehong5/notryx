import { beforeEach, expect, test, vi } from "vitest";
import { previewWorkspaceImage } from "./ipc";
import type { NodeState } from "../../../packages/protocol-types";

const mocked = vi.hoisted(() => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: mocked.invoke }));
const node = {} as NodeState;
function respond(patch: Record<string, unknown>) {
  mocked.invoke.mockImplementation(async (command: string) => command === "m0_renderer_session" ? "owner" : {
    png_base64: "iVBORw==", width: 1, height: 1, ...patch,
  });
}
beforeEach(() => mocked.invoke.mockReset());

test("loads a passive image using the current document and renderer session", async () => {
  respond({});
  await expect(previewWorkspaceImage("document", "edit", node)).resolves.toEqual({ bytes: new Uint8Array([137, 80, 78, 71]) });
  expect(mocked.invoke).toHaveBeenCalledWith("workspace_preview_image", {
    request: { renderer_session_id: "owner", document_id: "document", edit_session_id: "edit", node },
  });
});

test.each([
  { width: 8193 }, { width: 4001, height: 4000 }, { height: 0 }, { width: 1.5 },
])("rejects an invalid preview response %j", async (patch) => {
  respond(patch);
  await expect(previewWorkspaceImage("document", "edit", node)).rejects.toThrow();
});
