import { beforeEach, expect, test, vi } from "vitest";
import { cancelWorkspaceSearch, workspaceSearch } from "./ipc";
import type { WorkspaceRegistration } from "../../../packages/protocol-types";

const mocked = vi.hoisted(() => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: mocked.invoke }));
const binding: WorkspaceRegistration = { server_origin:"https://example.invalid", workspace_id:"workspace", workspace_epoch:"epoch", device_id:"device", device_stream_id:"stream" };
const nodeId = "019a1000-0000-7000-8000-000000000002";
const rootId = "019a1000-0000-7000-8000-000000000001";
const cursor = "019a1000-0000-7000-8000-000000000003";
const node = { node_id:nodeId, parent_id:rootId, name:"document.md", kind:"file", deleted:false, content_rev:"content", content_etag:'"content"', meta_rev:"meta", meta_etag:'"meta"', node_etag:'"node"', content_hash:"sha256:" + "a".repeat(64), size:"4", children_generation:"0", last_change_seq:"1" };
const request = { query:"가나", scope:"workspace" as const, after:null, request_id:"1" };
const page = { binding, request_id:"1", results:[{ node_id:nodeId, scope:"workspace", source:"draft", node }], available_documents:1, total_documents:2, recovery_available_documents:0, recovery_total_documents:0, next_after_id:cursor };

beforeEach(() => mocked.invoke.mockReset());
function response(value: unknown) {
  mocked.invoke.mockImplementation(async (command: string) => command === "m0_renderer_session" ? "owner" : value);
}

test("검색 요청과 응답은 renderer, 등록, 요청 식별자와 구간을 보존한다", async () => {
  response(page);
  const result = await workspaceSearch(binding, request);
  expect(result.results[0].node_id).toBe(nodeId);
  expect(result.next_after_id).toBe(cursor);
  expect(mocked.invoke).toHaveBeenCalledWith("workspace_search", { request:{ renderer_session_id:"owner", binding, query:"가나", scope:"workspace", after_id:null, request_id:"1" } });
});

test("등록이나 요청 식별자가 다르거나 응답 범위가 다르면 거절한다", async () => {
  for (const value of [
    { ...page, binding:{ ...binding, workspace_epoch:"other" } },
    { ...page, request_id:"2" },
    { ...page, results:[{ ...page.results[0], scope:"recovery" }] },
    { ...page, results:[{ ...page.results[0], source:"unknown" }] },
    { ...page, results:[{ ...page.results[0], node:{ ...node, node_id:cursor } }] },
    { ...page, results:[{ ...page.results[0], node:null }] },
  ]) {
    response(value);
    await expect(workspaceSearch(binding, request)).rejects.toThrow();
  }
});

test("coverage, 결과 순서와 최대 개수, cursor 진행을 검증한다", async () => {
  const many = Array.from({length:201}, (_, index) => ({ ...page.results[0], node_id:`019a1000-0000-7000-8000-${(index + 2).toString(16).padStart(12, "0")}` }));
  for (const value of [
    { ...page, available_documents:3, total_documents:2 },
    { ...page, available_documents:-1 },
    { ...page, total_documents:1.5 },
    { ...page, results:many },
    { ...page, results:[page.results[0], page.results[0]], available_documents:2 },
    { ...page, next_after_id:rootId },
    { ...page, next_after_id:"invalid" },
  ]) {
    response(value);
    await expect(workspaceSearch(binding, request)).rejects.toThrow();
  }
  response({ ...page, results:[], available_documents:0, total_documents:0, next_after_id:cursor });
  await expect(workspaceSearch(binding, { ...request, after:cursor })).rejects.toThrow();
});

test("검색 취소는 현재 renderer와 해당 검색 식별자를 명시한다", async () => {
  response(undefined);
  await cancelWorkspaceSearch(binding, "1");
  expect(mocked.invoke).toHaveBeenCalledWith("workspace_cancel_search", { request:{ renderer_session_id:"owner", binding, request_id:"1" } });
});

test("검색 요청 식별자는 정규 십진수 1부터 안전한 정수 상한까지만 허용한다", async () => {
  const max = String(Number.MAX_SAFE_INTEGER);
  response({ ...page, request_id:max });
  await expect(workspaceSearch(binding, { ...request, request_id:max })).resolves.toBeDefined();
  response(undefined);
  await expect(cancelWorkspaceSearch(binding, max)).resolves.toBeUndefined();
  for (const invalid of ["", "0", "01", "+1", "-1", "1.0", "1e2", " 1", "1 ", "9007199254740992", "9".repeat(100), 1 as unknown as string]) {
    mocked.invoke.mockClear();
    await expect(workspaceSearch(binding, { ...request, request_id:invalid })).rejects.toThrow();
    await expect(cancelWorkspaceSearch(binding, invalid)).rejects.toThrow();
    expect(mocked.invoke).not.toHaveBeenCalledWith("workspace_search", expect.anything());
    expect(mocked.invoke).not.toHaveBeenCalledWith("workspace_cancel_search", expect.anything());
  }
});
