// @vitest-environment jsdom
import { act, createElement } from "react";
import { createRoot } from "react-dom/client";
import { afterEach, expect, test, vi } from "vitest";
import { CreatePanel, type CreatePanelPorts } from "./create-panel";
import { FrozenCreateReview } from "./frozen-create-review";
import { queueWorkspaceCreate, workspaceCreateStatus, workspaceFrozenCreates } from "./ipc";
import type { NodeState, WorkspaceRegistration } from "../../../packages/protocol-types";

const mocked = vi.hoisted(() => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: mocked.invoke }));
const binding: WorkspaceRegistration = { server_origin:"https://example.invalid", workspace_id:"workspace", workspace_epoch:"epoch", device_id:"device", device_stream_id:"stream" };
const parent = "019a1000-0000-7000-8000-000000000001";
const node = "019a1000-0000-7000-8000-000000000002";
const operation = "019a1000-0000-7000-8000-000000000003";
const folder: NodeState = { node_id:parent, parent_id:null, name:"", kind:"folder", deleted:false, content_rev:null, content_etag:null, meta_rev:"meta", meta_etag:'"meta"', node_etag:'"node"', content_hash:null, size:null, children_generation:"0", last_change_seq:"1" };
const request = { node_id:node, parent_id:parent, name:"새 문서.md", kind:"file", upload_id:null };
const queued = { operation_id:operation, operation_seq:"1", node_id:node, request, status:"queued", receipt:null };
const createdNode = { ...folder, node_id:node, parent_id:parent, name:request.name, kind:"file", content_rev:"content", content_etag:'"content"', content_hash:"sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855", size:"0", last_change_seq:"9" };
const receipt = { status:"created", operation_id:operation, operation_seq:"1", workspace_epoch:binding.workspace_epoch, node:createdNode, commit_seq:"9" };
function respond(value: unknown) { mocked.invoke.mockImplementation(async (command: string) => command === "m0_renderer_session" ? "owner" : value); }
afterEach(() => { document.body.replaceChildren(); vi.unstubAllGlobals(); mocked.invoke.mockReset(); });

test("빈 파일 생성 요청의 종류를 전달하고 응답 종류를 확인한다", async () => {
  respond(queued);
  const value = await queueWorkspaceCreate(binding, { parent_id:parent, name:request.name, kind:"file" });
  expect(value).toMatchObject({ node_id:node, kind:"file", status:"queued" });
  expect(mocked.invoke).toHaveBeenCalledWith("workspace_queue_create", { request:{ renderer_session_id:"owner", binding, parent_id:parent, name:request.name, kind:"file" } });
  respond({ ...queued, request:{ ...request, kind:"folder" } });
  await expect(queueWorkspaceCreate(binding, { parent_id:parent, name:request.name, kind:"file" })).rejects.toThrow();
});

test("파일 생성 결과의 원문은 빈 파일이어야 한다", async () => {
  const page = (nodeValue: unknown) => ({ binding, entries:[{ ...queued, status:"committed", receipt:{ ...receipt, node:nodeValue } }], next_after_seq:null });
  respond(page(createdNode));
  expect((await workspaceCreateStatus(binding,"0")).entries[0]).toMatchObject({ kind:"file", status:"committed" });
  for (const bad of [{ ...createdNode, size:"1" }, { ...createdNode, content_hash:"sha256:bad" }, { ...createdNode, content_etag:'"other"' }, { ...createdNode, kind:"folder" }]) {
    respond(page(bad)); await expect(workspaceCreateStatus(binding,"0")).rejects.toThrow();
  }
});

test("이전 등록의 파일 생성 종류를 대조 화면에 전달한다", async () => {
  const old_binding = { ...binding, device_stream_id:"old-stream" };
  respond({ binding, entries:[{ old_binding, operation_id:operation, operation_seq:"1", request, status:"sent", receipt:null, current_same_id:null, current_same_name:[] }], next_after_id:null });
  expect((await workspaceFrozenCreates(binding,null)).entries[0]).toMatchObject({ kind:"file", name:request.name });
});

test("오프라인 파일 생성은 별도 요청으로 표시하고 서버 동기화 전 편집할 수 있다고 표시하지 않는다", async () => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  const api: CreatePanelPorts = {
    folders: vi.fn(async () => ({ nodes:[folder], next_after_id:null })),
    queue: vi.fn(async (_binding, value) => ({ operation_id:operation, operation_seq:"1", node_id:node, ...value, status:"queued" as const, reason:null })),
    status: vi.fn(async () => ({ entries:[], next_after_seq:null })), retry:vi.fn(async () => {}), cancel:vi.fn(async () => {}),
  };
  const container=document.createElement("div"); document.body.append(container); const root=createRoot(container);
  await act(async () => { root.render(createElement(CreatePanel,{ binding, ports:api, revision:0, onWorkReady:vi.fn() })); });
  await vi.waitFor(() => expect(container.querySelector(`[value="${parent}"]`)).not.toBeNull());
  const parentSelect=container.querySelector<HTMLSelectElement>('[data-testid="create-parent"]')!;
  const kindSelect=container.querySelector<HTMLSelectElement>('[data-testid="create-kind"]')!;
  const name=container.querySelector<HTMLInputElement>('[data-testid="create-name"]')!;
  await act(async () => {
    parentSelect.value=parent; parentSelect.dispatchEvent(new Event("change",{ bubbles:true }));
    kindSelect.value="file"; kindSelect.dispatchEvent(new Event("change",{ bubbles:true }));
    Object.getOwnPropertyDescriptor(HTMLInputElement.prototype,"value")!.set!.call(name,request.name); name.dispatchEvent(new Event("input",{ bubbles:true }));
  });
  await act(async () => { container.querySelector<HTMLButtonElement>('[data-testid="create-submit"]')!.click(); });
  expect(api.queue).toHaveBeenCalledExactlyOnceWith(binding,{ parent_id:parent, name:request.name, kind:"file" });
  expect(container.textContent).toContain("서버 반영과 동기화 후 열 수 있습니다");
  expect(container.querySelector(`[value="${node}"]`)).toBeNull();
  await act(async () => { root.unmount(); });
});


test("이전 파일 요청 대조에서 문서 종류와 현재 확인 범위를 표시한다", async () => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  const old_binding = { ...binding, device_stream_id:"old" };
  const api = {
    list: vi.fn(async () => ({ binding, entries:[{ old_binding, operation_id:operation, operation_seq:"1", node_id:node, parent_id:parent, name:request.name, kind:"file" as const, status:"sent" as const, current_same_id:null, current_same_name:[] }], next_after_id:null })),
    folders: vi.fn(async () => ({ nodes:[folder], next_after_id:null })),
    decide: vi.fn(async () => {}),
  };
  const container = document.createElement("div"); document.body.append(container); const root = createRoot(container);
  await act(async () => { root.render(createElement(FrozenCreateReview,{ binding, ports:api, onApplied:vi.fn() })); });
  await vi.waitFor(() => expect(container.querySelector(`[data-review-operation="${operation}"]`)).not.toBeNull());
  await act(async () => { container.querySelector<HTMLButtonElement>(`[data-review-operation="${operation}"]`)!.click(); });
  expect(container.textContent).toContain("이전 종류: 문서");
  expect(container.textContent).toContain("새 문서 이름");
  expect(container.textContent).toContain("이전 요청의 서버 결과는 확정하지 않습니다");
  await act(async () => { root.unmount(); });
});

test("새 Markdown 문서에 확장자가 없으면 요청하지 않고 이유를 안내한다", async () => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  const api: CreatePanelPorts = {
    folders: vi.fn(async () => ({ nodes:[folder], next_after_id:null })),
    queue: vi.fn(async (_binding, value) => ({ operation_id:operation, operation_seq:"1", node_id:node, ...value, status:"queued" as const, reason:null })),
    status: vi.fn(async () => ({ entries:[], next_after_seq:null })), retry:vi.fn(async () => {}), cancel:vi.fn(async () => {}),
  };
  const container=document.createElement("div"); document.body.append(container); const root=createRoot(container);
  await act(async () => { root.render(createElement(CreatePanel,{ binding, ports:api, revision:0, onWorkReady:vi.fn() })); });
  await vi.waitFor(() => expect(container.querySelector(`[value="${parent}"]`)).not.toBeNull());
  await act(async () => {
    const parentSelect=container.querySelector<HTMLSelectElement>('[data-testid="create-parent"]')!; parentSelect.value=parent; parentSelect.dispatchEvent(new Event("change",{ bubbles:true }));
    const kindSelect=container.querySelector<HTMLSelectElement>('[data-testid="create-kind"]')!; kindSelect.value="file"; kindSelect.dispatchEvent(new Event("change",{ bubbles:true }));
    const name=container.querySelector<HTMLInputElement>('[data-testid="create-name"]')!; Object.getOwnPropertyDescriptor(HTMLInputElement.prototype,"value")!.set!.call(name,"확장자 없음"); name.dispatchEvent(new Event("input",{ bubbles:true }));
  });
  await act(async () => { container.querySelector<HTMLButtonElement>('[data-testid="create-submit"]')!.click(); });
  expect(api.queue).not.toHaveBeenCalled();
  expect(container.textContent).toContain(".md");
  await act(async () => { root.unmount(); });
});

test("이전 문서 재적용에서 확장자 없는 이름을 선택하면 요청을 막는다", async () => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  const old_binding = { ...binding, device_stream_id:"old" };
  const api = {
    list: vi.fn(async () => ({ binding, entries:[{ old_binding, operation_id:operation, operation_seq:"1", node_id:node, parent_id:parent, name:request.name, kind:"file" as const, status:"sent" as const, current_same_id:null, current_same_name:[] }], next_after_id:null })),
    folders: vi.fn(async () => ({ nodes:[folder], next_after_id:null })), decide:vi.fn(async () => {}),
  };
  const container = document.createElement("div"); document.body.append(container); const root = createRoot(container);
  await act(async () => { root.render(createElement(FrozenCreateReview,{ binding, ports:api, onApplied:vi.fn() })); });
  await vi.waitFor(() => expect(container.querySelector(`[data-review-operation="${operation}"]`)).not.toBeNull());
  await act(async () => { container.querySelector<HTMLButtonElement>(`[data-review-operation="${operation}"]`)!.click(); });
  await act(async () => {
    const select=container.querySelector<HTMLSelectElement>('[data-testid="review-parent"]')!; select.value=parent; select.dispatchEvent(new Event("change",{ bubbles:true }));
    const name=container.querySelector<HTMLInputElement>('[data-testid="review-name"]')!; Object.getOwnPropertyDescriptor(HTMLInputElement.prototype,"value")!.set!.call(name,"확장자 없음"); name.dispatchEvent(new Event("input",{ bubbles:true }));
  });
  await act(async () => { container.querySelector<HTMLButtonElement>('[data-testid="review-apply"]')!.click(); });
  expect(api.decide).not.toHaveBeenCalled(); expect(container.textContent).toContain(".md");
  await act(async () => { root.unmount(); });
});

test("파일 receipt 크기의 유효한 0 표기를 수용한다", async () => {
  respond({ binding, entries:[{ ...queued, status:"committed", receipt:{ ...receipt, node:{ ...createdNode, size:"00" } } }], next_after_seq:null });
  expect((await workspaceCreateStatus(binding,"0")).entries[0]).toMatchObject({ kind:"file", status:"committed" });
});
