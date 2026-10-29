// @vitest-environment jsdom
import { act, createElement } from "react";
import { createRoot } from "react-dom/client";
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { MovePanel, type MovePanelPorts } from "./move-panel";
import type { NodeState, WorkspaceRegistration } from "../../../packages/protocol-types";

const binding: WorkspaceRegistration = { server_origin:"https://example.invalid", workspace_id:"workspace", workspace_epoch:"epoch", device_id:"device", device_stream_id:"stream" };
const root: NodeState = { node_id:"root", parent_id:null, name:"", kind:"folder", deleted:false, content_rev:null, content_etag:null, content_hash:null, size:null, meta_rev:"meta", meta_etag:'"meta"', node_etag:'"node"', children_generation:"0", last_change_seq:"1" };
const destination: NodeState = { ...root, node_id:"destination", parent_id:"root", name:"destination" };
const file: NodeState = { ...root, node_id:"document", parent_id:"root", name:"Synthetic.md", kind:"file", content_rev:"content", content_etag:'"content"', content_hash:"sha256:" + "a".repeat(64), size:"1" };
const request = { expected_parent_id:"root", parent_id:"destination", name:"Synthetic.md" };
const entry = { operation_id:"operation", operation_seq:"1", node_id:"document", expected_meta_etag:'"meta"', request, status:"queued" as const, reason:null };
function change(element: HTMLInputElement | HTMLSelectElement, value: string) {
  const prototype = element instanceof HTMLInputElement ? HTMLInputElement.prototype : HTMLSelectElement.prototype;
  Object.getOwnPropertyDescriptor(prototype, "value")!.set!.call(element, value);
  element.dispatchEvent(new Event(element instanceof HTMLInputElement ? "input" : "change", { bubbles:true }));
}
function setup(inspect: () => { status:"checked"; affected:number; unresolved:number; isCurrent():boolean }) {
  const ports: MovePanelPorts & { inspect: typeof inspect } = { folders:vi.fn(async () => ({ nodes:[root,destination], next_after_id:null })), queue:vi.fn(async()=>entry), status:vi.fn(async()=>({entries:[],next_after_seq:null})), retry:vi.fn(async()=>{}), cancel:vi.fn(async()=>{}), inspect };
  const container = document.createElement("div"); document.body.append(container);
  const mounted = createRoot(container);
  const props = { binding, node:file, displayName:"Synthetic.md", ports, revision:0, onClose:vi.fn(), onWorkReady:vi.fn() };
  return { container, mounted, props, ports };
}
async function prepare(container: HTMLElement) {
  await vi.waitFor(() => expect(container.querySelector<HTMLSelectElement>('[data-testid="move-parent"]')?.options.length).toBeGreaterThan(1));
  await act(async () => { change(container.querySelector<HTMLSelectElement>('[data-testid="move-parent"]')!, "destination"); });
  await act(async () => { container.querySelector<HTMLButtonElement>('[data-testid="move-prepare"]')!.click(); });
  await vi.waitFor(() => expect(container.querySelector('[data-testid="move-confirm"]')).not.toBeNull());
}
beforeEach(() => { vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true); });
afterEach(() => { document.body.replaceChildren(); vi.unstubAllGlobals(); });

test("열린 문서에서 검사한 상대 링크 결과와 나머지 미검사 범위를 구분해 표시한다", async () => {
  const inspect = vi.fn(() => ({ status:"checked" as const, affected:1, unresolved:2, isCurrent:() => true }));
  const { container, mounted, props } = setup(inspect);
  await act(async () => { mounted.render(createElement(MovePanel, props)); });
  await prepare(container);
  expect(inspect).toHaveBeenCalledOnce();
  expect(container.textContent).toContain("열린 문서");
  expect(container.textContent).toContain("영향 가능 1");
  expect(container.textContent).toContain("확인하지 못한 링크 2");
  expect(container.textContent).toContain("다른 문서");
  expect(container.textContent).toContain("검사하지 않았습니다");
  await act(async () => { mounted.unmount(); });
});

test("검사 후 열린 문서 generation이 바뀌면 이전 결과를 확인에 사용하지 않는다", async () => {
  let current = true;
  const { container, mounted, props, ports } = setup(() => ({ status:"checked", affected:0, unresolved:0, isCurrent:() => current }));
  await act(async () => { mounted.render(createElement(MovePanel, props)); });
  await prepare(container);
  current = false;
  await act(async () => { container.querySelector<HTMLButtonElement>('[data-testid="move-confirm"]')!.click(); });
  expect(ports.queue).not.toHaveBeenCalled();
  expect(container.querySelector('[data-testid="move-confirm"]')).toBeNull();
  expect(container.textContent).toMatch(/문서.*변경|다시 검사/);
  await act(async () => { mounted.unmount(); });
});

test("목적지 재조회 중 세션이 바뀌어도 이전 검사 결과로 전송하지 않는다", async () => {
  let current = true, release!: () => void, reads = 0;
  const { container, mounted, props, ports } = setup(() => ({ status:"checked", affected:0, unresolved:0, isCurrent:() => current }));
  ports.folders = vi.fn(async () => { if (++reads > 1) await new Promise<void>((resolve) => { release = resolve; }); return { nodes:[root,destination], next_after_id:null }; });
  await act(async () => { mounted.render(createElement(MovePanel, props)); });
  await prepare(container);
  await act(async () => { container.querySelector<HTMLButtonElement>('[data-testid="move-confirm"]')!.click(); });
  current = false;
  await act(async () => { release(); });
  expect(ports.queue).not.toHaveBeenCalled();
  expect(container.textContent).toMatch(/문서.*변경|다시 검사/);
  await act(async () => { mounted.unmount(); });
});
