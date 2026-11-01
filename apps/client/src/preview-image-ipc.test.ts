import { beforeEach, expect, test, vi } from "vitest";
import { previewWorkspaceImage, openWorkspaceDiagram } from "./ipc";
import type { NodeState } from "../../../packages/protocol-types";

const mocked = vi.hoisted(() => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: mocked.invoke }));
const node = {} as NodeState;
function respond(patch: Record<string, unknown>) {
  mocked.invoke.mockImplementation(async (command: string) => command === "m0_renderer_session" ? "owner" : {
    png_base64: "iVBORw==", width: 1, height: 1, drawio_status: "none", ...patch,
  });
}
beforeEach(() => mocked.invoke.mockReset());

test("opens a diagram using the current document and renderer session without returning XML", async () => {
  respond({});
  await expect(openWorkspaceDiagram("document", "edit", node)).resolves.toBeUndefined();
  expect(mocked.invoke).toHaveBeenCalledWith("drawio_open", {
    request: { renderer_session_id: "owner", document_id: "document", edit_session_id: "edit", node },
  });
});

test.each(["none", "embedded_xml", "invalid_xml", "unsupported_xml", "limit_exceeded"])("reads the native %s diagram status with the passive image", async (status) => {
  respond({ drawio_status: status });
  await expect(previewWorkspaceImage("document", "edit", node)).resolves.toEqual({
    bytes: new Uint8Array([137, 80, 78, 71]), drawioStatus: status,
  });
  expect(mocked.invoke).toHaveBeenCalledWith("workspace_preview_image", {
    request: { renderer_session_id: "owner", document_id: "document", edit_session_id: "edit", node },
  });
});

test.each([
  { drawio_status: undefined }, { drawio_status: "editable" }, { drawio_status: { xml: "<mxfile/>" } },
  { width: 8193 }, { width: 4001, height: 4000 }, { height: 0 }, { width: 1.5 },
])("rejects an invalid preview response %j", async (patch) => {
  respond(patch);
  await expect(previewWorkspaceImage("document", "edit", node)).rejects.toThrow();
});
