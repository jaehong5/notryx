// @vitest-environment jsdom
import { act, createElement } from "react";
import { createRoot } from "react-dom/client";
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { CreatePanel, type CreateDisplay, type CreatePanelPorts } from "./create-panel";
import { queueWorkspaceCreate, workspaceCreateStatus } from "./ipc";
import type { NodeState, WorkspaceRegistration } from "../../../packages/protocol-types";

const mocked = vi.hoisted(() => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: mocked.invoke }));
const binding: WorkspaceRegistration = { server_origin: "https://example.invalid", workspace_id: "workspace", workspace_epoch: "epoch", device_id: "device", device_stream_id: "stream" };
const parent = "019a1000-0000-7000-8000-000000000001";
const node = "019a1000-0000-7000-8000-000000000002";
const operation = "019a1000-0000-7000-8000-000000000003";
const upload = "019a1000-0000-4000-8000-000000000004";
const name = "새 그림.drawio.png";
const folder: NodeState = { node_id: parent, parent_id: null, name: "", kind: "folder", deleted: false, content_rev: null, content_etag: null, meta_rev: "meta", meta_etag: '"meta"', node_etag: '"node"', content_hash: null, size: null, children_generation: "0", last_change_seq: "1" };
const request = { node_id: node, parent_id: parent, name, kind: "file", upload_id: upload };
const queued = { operation_id: operation, operation_seq: "1", node_id: node, request, status: "queued", receipt: null };
const createdNode = { ...folder, node_id: node, parent_id: parent, name, kind: "file", content_rev: "content", content_etag: '"content"', content_hash: `sha256:${"a".repeat(64)}`, size: "512", last_change_seq: "9" };
const receipt = { status: "created", operation_id: operation, operation_seq: "1", workspace_epoch: binding.workspace_epoch, node: createdNode, commit_seq: "9" };
const entry: CreateDisplay = { operation_id: operation, operation_seq: "1", node_id: node, parent_id: parent, name, kind: "file", status: "queued", reason: null };
const page = (value: unknown) => ({ binding, entries: [value], next_after_seq: null });
function respond(value: unknown) { mocked.invoke.mockImplementation(async (command: string) => command === "m0_renderer_session" ? "owner" : value); }
function ports(): CreatePanelPorts {
  return { folders: vi.fn(async () => ({ nodes: [folder], next_after_id: null })), queue: vi.fn(async () => entry), status: vi.fn(async () => ({ entries: [], next_after_seq: null })), retry: vi.fn(async () => {}), cancel: vi.fn(async () => {}) };
}
beforeEach(() => { mocked.invoke.mockReset(); vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true); });
afterEach(() => { document.body.replaceChildren(); vi.unstubAllGlobals(); });

test("rejects diagram creation before invoking a native command", async () => {
  await expect(queueWorkspaceCreate(binding, { parent_id: parent, name, kind: "diagram" as "file" })).rejects.toThrow();
  expect(mocked.invoke).not.toHaveBeenCalled();
});

test("uploaded creation accepts a bounded nonempty file receipt while empty creation remains strict", async () => {
  respond(page({ ...queued, status: "committed", receipt }));
  expect((await workspaceCreateStatus(binding, "0")).entries[0].status).toBe("committed");
  for (const patch of [{ size: "0" }, { size: "26214401" }, { content_hash: "sha256:bad" }, { content_hash: `sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855` }, { content_etag: '"other"' }]) {
    respond(page({ ...queued, status: "committed", receipt: { ...receipt, node: { ...createdNode, ...patch } } }));
    await expect(workspaceCreateStatus(binding, "0")).rejects.toThrow();
  }
  respond(page({ ...queued, request: { ...request, upload_id: null }, status: "committed", receipt }));
  await expect(workspaceCreateStatus(binding, "0")).rejects.toThrow();
});

test("upload failure remains separate from an authoritative Created receipt", async () => {
  const abandoned = { status: "abandoned", operation_id: operation, operation_seq: "1", workspace_epoch: binding.workspace_epoch };
  respond(page({ ...queued, status: "abandoned", receipt: abandoned, upload_unavailable: true }));
  expect((await workspaceCreateStatus(binding, "0")).entries[0].reason).toBe("upload_unavailable");
  respond(page({ ...queued, status: "committed", receipt, upload_unavailable: true }));
  expect((await workspaceCreateStatus(binding, "0")).entries[0]).toMatchObject({ status: "committed", reason: null });
  respond(page({ ...queued, upload_unavailable: "yes" }));
  await expect(workspaceCreateStatus(binding, "0")).rejects.toThrow();
});

test("keeps legacy creation results visible without offering diagram creation or editing", async () => {
  const api = ports();
  api.status = vi.fn(async () => ({ entries: [{ ...entry, status: "committed" as const }, { ...entry, operation_id: "pending", status: "abandoning" as const, reason: "upload_unavailable" }, { ...entry, operation_id: "failed", status: "abandoned" as const, reason: "upload_unavailable" }], next_after_seq: null }));
  const container = document.createElement("div"); const root = createRoot(container);
  await act(async () => root.render(createElement(CreatePanel, { binding, ports: api, revision: 0, onWorkReady: vi.fn() })));
  expect([...container.querySelectorAll("[data-testid=create-kind] option")].map((option) => option.getAttribute("value"))).toEqual(["folder", "file"]);
  expect(container.querySelector("[data-create-open-diagram]")).toBeNull();
  expect(container.textContent).toContain("생성 완료");
  expect(container.textContent).toContain("취소 처리 중");
  expect(container.textContent).toContain("업로드를 사용할 수 없어 이전 생성 요청을 완료하지 못했습니다.");
  await act(async () => root.unmount());
});
