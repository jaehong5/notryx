import { beforeEach, expect, test, vi } from "vitest";
import type { NodeState, WorkspaceRegistration } from "../../../packages/protocol-types";
import * as ipc from "./ipc";

const mocked = vi.hoisted(() => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: mocked.invoke }));

const binding: WorkspaceRegistration = { server_origin: "https://example.invalid", workspace_id: "workspace", workspace_epoch: "epoch", device_id: "device", device_stream_id: "stream" };
const root = "019a1000-0000-7000-8000-000000000001";
const child = "019a1000-0000-7000-8000-000000000002";
const later = "019a1000-0000-7000-8000-000000000003";
const folder = (id: string, parent: string | null, name: string) => ({
  node_id: id, parent_id: parent, name, kind: "folder", deleted: false,
  content_rev: null, content_etag: null, meta_rev: "meta", meta_etag: '"meta"',
  node_etag: '"node"', content_hash: null, size: null, children_generation: "0", last_change_seq: "1",
});
const page = (nodes: unknown[], next: string | null = null) => ({ binding, workspace_epoch: binding.workspace_epoch, nodes, next_after_id: next });

// Runtime discovery makes absence of the new IPC boundary an assertion failure,
// rather than a TypeScript missing-export compile error.
function lookup() {
  const fn = (ipc as unknown as { workspaceCreateParents?: (binding: WorkspaceRegistration, after: string | null) => Promise<{ nodes: NodeState[]; next_after_id: string | null }> }).workspaceCreateParents;
  expect(fn, "offline parent-list IPC is missing").toBeTypeOf("function");
  return fn!;
}
function respond(value: unknown) {
  mocked.invoke.mockImplementation(async (command: string) => command === "m0_renderer_session" ? "owner" : value);
}
beforeEach(() => mocked.invoke.mockReset());

test("오프라인 생성 부모는 서버 inventory 대신 등록된 로컬 목록 명령을 조회한다", async () => {
  respond(page([folder(root, null, ""), folder(child, root, "Notes")], child));
  const result = await lookup()(binding, null);
  expect(result.nodes.map((node) => node.node_id)).toEqual([root, child]);
  expect(result.next_after_id).toBe(child);
  expect(mocked.invoke).toHaveBeenCalledWith("workspace_create_parents", { request: { renderer_session_id: "owner", binding, after_id: null } });
  expect(mocked.invoke).not.toHaveBeenCalledWith("workspace_inventory", expect.anything());
  respond(page([folder(later, root, "More")]));
  expect((await lookup()(binding, child)).nodes[0].node_id).toBe(later);
  expect(mocked.invoke).toHaveBeenCalledWith("workspace_create_parents", { request: { renderer_session_id: "owner", binding, after_id: child } });
});

test("다른 등록이나 epoch의 로컬 부모 목록을 거절한다", async () => {
  for (const response of [
    { ...page([folder(root, null, "")]), binding: { ...binding, server_origin: "https://other.invalid" } },
    { ...page([folder(root, null, "")]), binding: { ...binding, workspace_id: "other" } },
    { ...page([folder(root, null, "")]), binding: { ...binding, workspace_epoch: "other" } },
    { ...page([folder(root, null, "")]), binding: { ...binding, device_id: "other" } },
    { ...page([folder(root, null, "")]), binding: { ...binding, device_stream_id: "other" } },
    { ...page([folder(root, null, "")]), workspace_epoch: "other" },
  ]) {
    respond(response);
    await expect(lookup()(binding, null)).rejects.toThrow();
  }
});

test("페이지 cursor와 노드 순서 및 실제 폴더 상태를 검증한다", async () => {
  for (const response of [
    page([folder(later, root, "Notes"), folder(later, root, "Notes")]),
    page([folder(later, root, "Notes"), folder(child, root, "Notes")]),
    page([folder(later, root, "Notes")], child),
    page([], later),
    page([folder(root, null, "")], root),
    page([{ ...folder(later, root, "Notes"), deleted: true }]),
    page([{ ...folder(later, root, "Notes"), kind: "file" }]),
    page([{ ...folder(later, root, "Notes"), parent_id: later }]),
    page(Array.from({ length: 101 }, (_, index) => folder(`019a1000-0000-7000-8000-${(index + 4).toString().padStart(12, "0")}`, root, "Folder"))),
  ]) {
    respond(response);
    await expect(lookup()(binding, child)).rejects.toThrow();
  }
  mocked.invoke.mockClear();
  await expect(lookup()(binding, "invalid")).rejects.toThrow();
  expect(mocked.invoke).not.toHaveBeenCalledWith("workspace_create_parents", expect.anything());
});
