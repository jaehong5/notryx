// @vitest-environment jsdom
import { act, createElement } from "react";
import { createRoot } from "react-dom/client";
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { TrashPanel, type TrashPanelPorts } from "./trash-panel";
import type { WorkspaceRegistration } from "../../../packages/protocol-types";

const binding: WorkspaceRegistration = { server_origin: "https://example.invalid", workspace_id: "workspace", workspace_epoch: "epoch", device_id: "device", device_stream_id: "stream" };
const folder = { node_id: "folder", parent_id: null, name: "", kind: "folder" as const, deleted: false, content_rev: null, content_etag: null, meta_rev: "meta", meta_etag: "\"meta\"", node_etag: "\"node\"", content_hash: null, size: null, children_generation: "0", last_change_seq: "1" };
function ports(): TrashPanelPorts {
  return {
    trash: vi.fn(async () => ({ workspace_epoch: "epoch", nodes: [{ node_id:"file", parent_id:"folder", name:"file.md", kind:"file" as const, meta_rev:"old", restorable:true }, { node_id:"expired", parent_id:"folder", name:"expired.md", kind:"file" as const, meta_rev:"old2", restorable:false }, { node_id:"deleted-folder", parent_id:"folder", name:"folder", kind:"folder" as const, meta_rev:"old3", restorable:true }], next_after_id:null })),
    folders: vi.fn(async () => ({ nodes:[folder], next_after_id:null })),
    queue: vi.fn(async (_binding, node_id, request) => ({ operation_id:"op", operation_seq:"1", node_id, request, status:"queued" as const, reason:null })),
    status: vi.fn(async () => ({ entries:[], next_after_seq:null })),
    cancel: vi.fn(async () => {}),
  };
}
beforeEach(() => { vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true); });
afterEach(() => { document.body.replaceChildren(); vi.unstubAllGlobals(); });

test("휴지통에서 만료와 빈 폴더를 안내하고 명시적 복원은 전송 대기로 남긴다", async () => {
  const api = ports(), onQueued = vi.fn(); const container = document.createElement("div"); document.body.append(container); const root = createRoot(container);
  await act(async () => { root.render(createElement(TrashPanel, { binding, ports:api, onQueued, revision:0 })); });
  await vi.waitFor(() => expect(container.textContent).toContain("expired.md"));
  expect(container.textContent).toContain("복원할 수 없음"); expect(container.textContent).toContain("빈 폴더");
  const expired = container.querySelector<HTMLButtonElement>('[data-trash-node="expired"]'); expect(expired?.disabled).toBe(true);
  await act(async () => { container.querySelector<HTMLButtonElement>('[data-trash-node="file"]')!.click(); });
  const select = container.querySelector<HTMLSelectElement>('[data-testid="restore-parent"]')!;
  await act(async () => { select.value = "folder"; select.dispatchEvent(new Event("change", { bubbles:true })); });
  await act(async () => { container.querySelector<HTMLButtonElement>('[data-testid="restore-submit"]')!.click(); });
  await vi.waitFor(() => expect(api.queue).toHaveBeenCalledOnce());
  expect(api.queue).toHaveBeenCalledWith(binding, "file", { expected_meta_rev:"old", parent_id:"folder", name:"file.md" });
  expect(onQueued).toHaveBeenCalledOnce(); expect(container.textContent).toContain("전송 대기");
  await act(async () => { root.unmount(); });
});

test("등록이 바뀌면 지연된 이전 휴지통 응답을 표시하지 않는다", async () => {
  const api = ports(); let resolveOld!: (page: Awaited<ReturnType<TrashPanelPorts["trash"]>>) => void;
  api.trash = vi.fn((active) => active.workspace_epoch === "epoch" ? new Promise<Awaited<ReturnType<TrashPanelPorts["trash"]>>>((resolve) => { resolveOld = resolve; }) : Promise.resolve({ workspace_epoch:"new-epoch", nodes:[], next_after_id:null }));
  const container = document.createElement("div"); document.body.append(container); const root = createRoot(container);
  await act(async () => { root.render(createElement(TrashPanel, { binding, ports:api, onQueued:vi.fn(), revision:0 })); });
  const replacement = { ...binding, workspace_epoch:"new-epoch" };
  await act(async () => { root.render(createElement(TrashPanel, { binding:replacement, ports:api, onQueued:vi.fn(), revision:0 })); });
  await act(async () => { resolveOld({ workspace_epoch:"epoch", nodes:[{ node_id:"stale", parent_id:"folder", name:"stale.md", kind:"file" as const, meta_rev:"old", restorable:true }], next_after_id:null }); });
  expect(container.textContent).not.toContain("stale.md");
  await act(async () => { root.unmount(); });
});

test("복원 요청의 취소 처리 중과 취소 완료를 구분한다", async () => {
  const api = ports();
  const request = { expected_meta_rev:"old", parent_id:"folder", name:"file.md" };
  api.status = vi.fn(async () => ({ entries:[
    { operation_id:"first", operation_seq:"1", node_id:"file", request, status:"abandoning" as const, reason:null },
    { operation_id:"second", operation_seq:"2", node_id:"other", request, status:"abandoned" as const, reason:null },
  ], next_after_seq:null }));
  const container = document.createElement("div"); document.body.append(container); const root = createRoot(container);
  await act(async () => { root.render(createElement(TrashPanel, { binding, ports:api, onQueued:vi.fn(), revision:0 })); });
  await vi.waitFor(() => expect(container.textContent).toContain("취소 처리 중"));
  expect(container.textContent).toContain("취소 완료");
  await act(async () => { root.unmount(); });
});

test("권한 거절된 복원은 사용자가 취소를 선택한 뒤에만 전송 재개를 알린다", async () => {
  const api = ports() as TrashPanelPorts & { cancel: (binding: WorkspaceRegistration, operationId: string) => Promise<void> };
  const request = { expected_meta_rev:"old", parent_id:"folder", name:"file.md" };
  api.status = vi.fn(async () => ({ entries:[{ operation_id:"op", operation_seq:"1", node_id:"file", request, status:"sent" as const, reason:null }], next_after_seq:null }));
  api.cancel = vi.fn(async () => {});
  const onCancelled = vi.fn();
  const props = { binding, ports:api, onQueued:vi.fn(), onCancelled, permissionDeniedOperationId:"op", revision:0 };
  const container = document.createElement("div"); document.body.append(container); const root = createRoot(container);
  await act(async () => { root.render(createElement(TrashPanel, props)); });
  await vi.waitFor(() => expect(container.textContent).toContain("권한"));
  expect(api.cancel).not.toHaveBeenCalled();
  expect(onCancelled).not.toHaveBeenCalled();
  const button = container.querySelector<HTMLButtonElement>('[data-restore-cancel="op"]');
  expect(button).not.toBeNull();
  await act(async () => { button!.click(); });
  expect(api.cancel).toHaveBeenCalledExactlyOnceWith(binding, "op");
  expect(onCancelled).toHaveBeenCalledExactlyOnceWith("op");
  expect(container.textContent).toContain("취소 처리 중");
  await act(async () => { root.unmount(); });
});

test("권한이 바뀌면 사용자가 같은 복원 요청을 명시적으로 다시 시도한다", async () => {
  const api = ports();
  const request = { expected_meta_rev:"old", parent_id:"folder", name:"file.md" };
  api.status = vi.fn(async () => ({ entries:[{ operation_id:"op", operation_seq:"1", node_id:"file", request, status:"sent" as const, reason:null }], next_after_seq:null }));
  const onRetry = vi.fn();
  const props = { binding, ports:api, onQueued:vi.fn(), onRetry, permissionDeniedOperationId:"op", revision:0 };
  const container = document.createElement("div"); document.body.append(container); const root = createRoot(container);
  await act(async () => { root.render(createElement(TrashPanel, props)); });
  await vi.waitFor(() => expect(container.textContent).toContain("복원 권한이 없습니다"));
  expect(onRetry).not.toHaveBeenCalled();
  const button = container.querySelector<HTMLButtonElement>('[data-restore-retry="op"]');
  expect(button).not.toBeNull();
  await act(async () => { button!.click(); });
  expect(onRetry).toHaveBeenCalledExactlyOnceWith("op");
  expect(api.cancel).not.toHaveBeenCalled();
  expect(api.queue).not.toHaveBeenCalled();
  await act(async () => { root.unmount(); });
});

test("등록을 바꾸면 이전 복원 취소 응답은 전송을 재개하지 않는다", async () => {
  const api = ports() as TrashPanelPorts & { cancel: (binding: WorkspaceRegistration, operationId: string) => Promise<void> };
  const request = { expected_meta_rev:"old", parent_id:"folder", name:"file.md" };
  api.status = vi.fn(async (active) => ({ entries:active.workspace_epoch === "epoch" ? [{ operation_id:"op", operation_seq:"1", node_id:"file", request, status:"sent" as const, reason:null }] : [], next_after_seq:null }));
  let release!: () => void;
  api.cancel = vi.fn(() => new Promise<void>((resolve) => { release = resolve; }));
  const onCancelled = vi.fn();
  const props = { binding, ports:api, onQueued:vi.fn(), onCancelled, permissionDeniedOperationId:"op", revision:0 };
  const container = document.createElement("div"); document.body.append(container); const root = createRoot(container);
  await act(async () => { root.render(createElement(TrashPanel, props)); });
  await vi.waitFor(() => expect(container.querySelector('[data-restore-cancel="op"]')).not.toBeNull());
  await act(async () => { container.querySelector<HTMLButtonElement>('[data-restore-cancel="op"]')!.click(); });
  const replacement = { ...binding, workspace_epoch:"new-epoch" };
  await act(async () => { root.render(createElement(TrashPanel, { ...props, binding:replacement })); });
  await act(async () => { release(); });
  expect(onCancelled).not.toHaveBeenCalled();
  expect(container.textContent).not.toContain("취소 처리 중");
  await act(async () => { root.unmount(); });
});

test("등록 교체 후 이전 취소의 서버 결과 확인 요청은 무시한다", async () => {
  const api = ports();
  const request = { expected_meta_rev:"old", parent_id:"folder", name:"file.md" };
  api.status = vi.fn(async (active) => ({ entries:active.workspace_epoch === "epoch" ? [{ operation_id:"op", operation_seq:"1", node_id:"file", request, status:"sent" as const, reason:null }] : [], next_after_seq:null }));
  let reject!: (error: Error) => void;
  api.cancel = vi.fn(() => new Promise<void>((_resolve, fail) => { reject = fail; }));
  const onRecheck = vi.fn();
  const props = { binding, ports:api, onQueued:vi.fn(), onRecheck, permissionDeniedOperationId:"op", revision:0 };
  const container = document.createElement("div"); document.body.append(container); const root = createRoot(container);
  await act(async () => { root.render(createElement(TrashPanel, props)); });
  await vi.waitFor(() => expect(container.querySelector('[data-restore-cancel="op"]')).not.toBeNull());
  await act(async () => { container.querySelector<HTMLButtonElement>('[data-restore-cancel="op"]')!.click(); });
  const replacement = { ...binding, workspace_epoch:"new-epoch" };
  await act(async () => { root.render(createElement(TrashPanel, { ...props, binding:replacement })); });
  await act(async () => { reject(new Error("operation_has_result")); });
  expect(onRecheck).not.toHaveBeenCalled();
  expect(container.textContent).not.toContain("서버 결과를 다시 확인");
  await act(async () => { root.unmount(); });
});

test("서버에 이미 결과가 있으면 취소 완료로 표시하거나 전송을 재개하지 않는다", async () => {
  const api = ports();
  const request = { expected_meta_rev:"old", parent_id:"folder", name:"file.md" };
  api.status = vi.fn(async () => ({ entries:[{ operation_id:"op", operation_seq:"1", node_id:"file", request, status:"sent" as const, reason:null }], next_after_seq:null }));
  api.cancel = vi.fn(async () => { throw new Error("operation_has_result"); });
  const onCancelled = vi.fn();
  const onRecheck = vi.fn();
  const props = { binding, ports:api, onQueued:vi.fn(), onCancelled, onRecheck, permissionDeniedOperationId:"op", revision:0 };
  const container = document.createElement("div"); document.body.append(container); const root = createRoot(container);
  await act(async () => { root.render(createElement(TrashPanel, props)); });
  await vi.waitFor(() => expect(container.querySelector('[data-restore-cancel="op"]')).not.toBeNull());
  await act(async () => { container.querySelector<HTMLButtonElement>('[data-restore-cancel="op"]')!.click(); });
  expect(onCancelled).not.toHaveBeenCalled();
  expect(onRecheck).toHaveBeenCalledExactlyOnceWith("op");
  expect(container.textContent).toContain("처리 중이거나 결과를 확인");
  expect(container.textContent).not.toContain("취소 완료");
  await act(async () => { root.unmount(); });
});

test("늦은 취소 응답은 먼저 확인된 복원 완료 상태를 되돌리지 않는다", async () => {
  const api = ports();
  const request = { expected_meta_rev:"old", parent_id:"folder", name:"file.md" };
  const sent = { operation_id:"op", operation_seq:"1", node_id:"file", request, status:"sent" as const, reason:null };
  api.status = vi.fn(async () => ({ entries:[sent], next_after_seq:null }));
  let release!: () => void;
  api.cancel = vi.fn(() => new Promise<void>((resolve) => { release = resolve; }));
  const props = { binding, ports:api, onQueued:vi.fn(), onCancelled:vi.fn(), permissionDeniedOperationId:"op", revision:0 };
  const container = document.createElement("div"); document.body.append(container); const root = createRoot(container);
  await act(async () => { root.render(createElement(TrashPanel, props)); });
  await vi.waitFor(() => expect(container.querySelector('[data-restore-cancel="op"]')).not.toBeNull());
  await act(async () => { container.querySelector<HTMLButtonElement>('[data-restore-cancel="op"]')!.click(); });
  api.status = vi.fn(async () => ({ entries:[{ ...sent, status:"committed" as const }], next_after_seq:null }));
  await act(async () => { root.render(createElement(TrashPanel, { ...props, revision:1 })); });
  await vi.waitFor(() => expect(container.textContent).toContain("복원 완료"));
  await act(async () => { release(); });
  expect(container.textContent).toContain("복원 완료");
  expect(container.textContent).not.toContain("취소 처리 중");
  await act(async () => { root.unmount(); });
});
