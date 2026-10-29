import { beforeEach, expect, test, vi } from "vitest";

const { invoke } = vi.hoisted(() => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke }));

const binding = {
  server_origin: "https://example.invalid",
  workspace_id: "workspace",
  workspace_epoch: "epoch",
  device_id: "device",
  device_stream_id: "stream",
};

beforeEach(() => {
  vi.resetModules();
  invoke.mockReset();
  invoke.mockImplementation((command: string) => {
    if (command === "m0_renderer_session") return Promise.resolve("renderer");
    if (command === "workspace_sync_status") return Promise.resolve({ binding, status: "needs_review" });
    return Promise.reject(new Error("unexpected command"));
  });
});

test("sync status is requested with the current renderer and retains the binding", async () => {
  const { workspaceSyncState } = await import("./ipc");
  await expect(workspaceSyncState()).resolves.toEqual({ binding, status: "needs_review" });
  expect(invoke).toHaveBeenCalledWith("workspace_sync_status", { request: { renderer_session_id: "renderer" } });
});

test.each([
  { binding, status: "unknown" },
  { binding: { ...binding, device_id: null }, status: "ready" },
  { binding, status: null },
])("invalid sync status responses are rejected", async (response) => {
  invoke.mockImplementation((command: string) => command === "m0_renderer_session" ? Promise.resolve("renderer") : Promise.resolve(response));
  const { workspaceSyncState } = await import("./ipc");
  await expect(workspaceSyncState()).rejects.toThrow();
});

test.each([null, "storage", "damaged", "unexpected"] as const)("validates and retains cache warning %s", async (cache_warning) => {
  const opened = { binding, document_id: "document", edit_session_id: "session", generation: "0", bytes_base64: "", readonly: false, local_draft: false, has_session: true, cache_warning, open_notice:null };
  invoke.mockImplementation((command: string) => command === "m0_renderer_session" ? Promise.resolve("renderer") : Promise.resolve(opened));
  const { openWorkspaceDocument } = await import("./ipc");
  await expect(openWorkspaceDocument({ node_id: "document" } as never)).resolves.toEqual(opened);
});

test.each([undefined, "unknown", 1, {}])("rejects an invalid cache warning", async (cache_warning) => {
  const opened = { binding, document_id: "document", edit_session_id: "session", generation: "0", bytes_base64: "", readonly: false, local_draft: false, has_session: true, cache_warning, open_notice:null };
  invoke.mockImplementation((command: string) => command === "m0_renderer_session" ? Promise.resolve("renderer") : Promise.resolve(opened));
  const { openWorkspaceDocument } = await import("./ipc");
  await expect(openWorkspaceDocument({ node_id: "document" } as never)).rejects.toThrow("잘못된 캐시 상태입니다.");
});

test.each([undefined, null, "false", 0])("rejects an invalid session indicator", async (has_session) => {
  const opened = { binding, document_id: "document", edit_session_id: "session", generation: "0", bytes_base64: "", readonly: false, local_draft: false, has_session, cache_warning: null, open_notice:null };
  invoke.mockImplementation((command: string) => command === "m0_renderer_session" ? Promise.resolve("renderer") : Promise.resolve(opened));
  const { openWorkspaceDocument } = await import("./ipc");
  await expect(openWorkspaceDocument({ node_id: "document" } as never)).rejects.toThrow("잘못된 문서 응답입니다.");
});

test.each([true, false])("retains session indicator %s", async (has_session) => {
  const opened = { binding, document_id: "document", edit_session_id: "session", generation: "0", bytes_base64: "", readonly: false, local_draft: false, has_session, cache_warning: null, open_notice:null };
  invoke.mockImplementation((command: string) => command === "m0_renderer_session" ? Promise.resolve("renderer") : Promise.resolve(opened));
  const { openWorkspaceDocument } = await import("./ipc");
  await expect(openWorkspaceDocument({ node_id: "document" } as never)).resolves.toEqual(opened);
});

test.each([true, false])("preserves prior draft notice in readonly open with session %s", async (has_session) => {
  const opened = { binding, document_id:"document", edit_session_id:"session", generation:"0", bytes_base64:"YmFzZQ==", readonly:true, local_draft:false, has_session, cache_warning:null, open_notice:"prior_draft_preserved" };
  invoke.mockImplementation((command: string) => command === "m0_renderer_session" ? Promise.resolve("renderer") : Promise.resolve(opened));
  const { openWorkspaceDocument } = await import("./ipc");
  await expect(openWorkspaceDocument({ node_id:"document" } as never)).resolves.toEqual(opened);
});

test("retains a null open notice", async () => {
  const opened = { binding, document_id:"document", edit_session_id:"session", generation:"0", bytes_base64:"YmFzZQ==", readonly:false, local_draft:false, has_session:true, cache_warning:null, open_notice:null };
  invoke.mockImplementation((command: string) => command === "m0_renderer_session" ? Promise.resolve("renderer") : Promise.resolve(opened));
  const { openWorkspaceDocument } = await import("./ipc");
  await expect(openWorkspaceDocument({ node_id:"document" } as never)).resolves.toEqual(opened);
});

test("validates the notice for local draft opens too", async () => {
  const opened = { binding, document_id:"document", edit_session_id:"session", generation:"1", bytes_base64:"YmFzZQ==", readonly:false, local_draft:true, has_session:true, cache_warning:null, open_notice:null };
  invoke.mockImplementation((command: string) => command === "m0_renderer_session" ? Promise.resolve("renderer") : Promise.resolve(opened));
  const { openLocalDraft } = await import("./ipc");
  const draft = { binding, document_id:"document", generation:"1", current_registration:true, has_session:true };
  await expect(openLocalDraft(draft)).resolves.toEqual(opened);
  invoke.mockImplementation((command: string) => command === "m0_renderer_session" ? Promise.resolve("renderer") : Promise.resolve({ ...opened, open_notice:"prior_draft_preserved" }));
  await expect(openLocalDraft(draft)).rejects.toThrow();
});

test.each([undefined, "unknown", 1, {}])("rejects an invalid or missing open notice", async (open_notice) => {
  const opened = { binding, document_id:"document", edit_session_id:"session", generation:"0", bytes_base64:"YmFzZQ==", readonly:true, local_draft:false, has_session:true, cache_warning:null, open_notice };
  invoke.mockImplementation((command: string) => command === "m0_renderer_session" ? Promise.resolve("renderer") : Promise.resolve(opened));
  const { openWorkspaceDocument } = await import("./ipc");
  await expect(openWorkspaceDocument({ node_id:"document" } as never)).rejects.toThrow();
});

test.each([
  { readonly:false, local_draft:false },
  { readonly:true, local_draft:true },
  { readonly:false, local_draft:true },
])("rejects inconsistent prior draft notice and open mode", async ({ readonly, local_draft }) => {
  const opened = { binding, document_id:"document", edit_session_id:"session", generation:"0", bytes_base64:"YmFzZQ==", readonly, local_draft, has_session:true, cache_warning:null, open_notice:"prior_draft_preserved" };
  invoke.mockImplementation((command: string) => command === "m0_renderer_session" ? Promise.resolve("renderer") : Promise.resolve(opened));
  const { openWorkspaceDocument } = await import("./ipc");
  await expect(openWorkspaceDocument({ node_id:"document" } as never)).rejects.toThrow();
});
