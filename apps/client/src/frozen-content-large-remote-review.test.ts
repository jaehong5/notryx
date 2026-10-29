// @vitest-environment jsdom
import { act, createElement } from "react";
import { createRoot } from "react-dom/client";
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { FrozenContentReview, type FrozenContentReviewItem, type FrozenContentReviewPorts, type FrozenContentReviewSnapshot } from "./frozen-content-review";
import type { WorkspaceRegistration } from "../../../packages/protocol-types";

const binding:WorkspaceRegistration = { server_origin:"https://example.invalid", workspace_id:"workspace", workspace_epoch:"current-epoch", device_id:"device", device_stream_id:"stream" };
const old:WorkspaceRegistration = { ...binding, workspace_epoch:"old-epoch", device_id:"old-device", device_stream_id:"old-stream" };
const item:FrozenContentReviewItem = { old_binding:old, operation_id:"operation", operation_seq:"1", document_id:"document", generation:"1", status:"sent" };
const bytes = (value:string) => new TextEncoder().encode(value);
const snapshot:FrozenContentReviewSnapshot = { ...item, binding, expected_revision:"base-revision", local_generation:"2", outcome_unknown:true, base_revision:"base-revision", remote_revision:"remote-revision", operation_bytes:bytes("payload"), base_bytes:bytes("base"), local_bytes:bytes("draft"), remote_bytes:bytes("remote") };
const localFields = [["operation_bytes","frozen-content-payload"],["base_bytes","frozen-content-base"],["local_bytes","frozen-content-draft"]] as const;

async function show(value:FrozenContentReviewSnapshot) {
  const container=document.createElement("div"); document.body.append(container); const root=createRoot(container);
  const ports:FrozenContentReviewPorts={ list:vi.fn(async (current) => ({ binding:current, entries:[item], next_after_id:null })), load:vi.fn(async () => value) };
  await act(async () => { root.render(createElement(FrozenContentReview,{ binding, ports })); });
  await vi.waitFor(() => expect(container.querySelector('[data-review-operation="operation"]')).not.toBeNull());
  await act(async () => { container.querySelector<HTMLButtonElement>('[data-review-operation="operation"]')!.click(); });
  await vi.waitFor(() => expect(container.querySelector('[data-testid="frozen-content-remote"]')).not.toBeNull());
  return { container, root, ports };
}
function section(container:HTMLElement,id:string) { return container.querySelector<HTMLElement>(`[data-testid="${id}"]`)!; }
function textarea(container:HTMLElement,id:string) { return section(container,id).querySelector<HTMLTextAreaElement>("textarea"); }

beforeEach(() => vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT",true));
afterEach(() => { document.body.replaceChildren(); vi.unstubAllGlobals(); });

test.each([5 * 1024 * 1024,5 * 1024 * 1024 + 1,25 * 1024 * 1024])("UTF-8 서버 원문 %s byte를 읽기 전용으로 표시하고 원본을 보존한다", async (size) => {
  const remote = new Uint8Array(size).fill(97);
  const { container, root } = await show({ ...snapshot, remote_bytes:remote });
  const field=textarea(container,"frozen-content-remote");
  expect(field).not.toBeNull(); expect(field!.readOnly).toBe(true);
  expect(field!.value.length).toBe(size); expect(/^a*$/.test(field!.value)).toBe(true);
  expect(remote.byteLength).toBe(size); expect(remote.every((value) => value === 97)).toBe(true);
  for (const [,id] of localFields) expect(textarea(container,id)?.readOnly).toBe(true);
  await act(async () => { root.unmount(); });
},30000);

test.each(localFields)("%s 로컬 원문은 5 MiB 이하만 표시한다", async (field,id) => {
  const limit=5 * 1024 * 1024;
  const atLimit=new Uint8Array(limit).fill(97);
  const first=await show({ ...snapshot, [field]:atLimit });
  const rendered=textarea(first.container,id);
  expect(rendered?.readOnly).toBe(true); expect(rendered?.value.length).toBe(limit); expect(/^a*$/.test(rendered!.value)).toBe(true);
  expect(atLimit.byteLength).toBe(limit); expect(atLimit.every((value) => value === 97)).toBe(true);
  await act(async () => { first.root.unmount(); });
  const over=new Uint8Array(limit+1).fill(98);
  const second=await show({ ...snapshot, [field]:over });
  expect(textarea(second.container,id)).toBeNull(); expect(section(second.container,id).textContent).toMatch(/표시|텍스트|크기/);
  expect(over.byteLength).toBe(limit+1); expect(over.every((value) => value === 98)).toBe(true);
  await act(async () => { second.root.unmount(); });
},30000);

test.each(["invalid-utf8","over-limit"] as const)("서버 원문이 %s이면 텍스트로 표시하지 않고 bytes를 보존한다", async (kind) => {
  const remote=kind === "invalid-utf8" ? new Uint8Array([255,254]) : new Uint8Array(25 * 1024 * 1024+1).fill(99);
  const size=remote.byteLength;
  const { container, root } = await show({ ...snapshot, remote_bytes:remote });
  expect(textarea(container,"frozen-content-remote")).toBeNull();
  expect(section(container,"frozen-content-remote").textContent).toMatch(/표시|텍스트|크기|UTF.?8/i);
  expect(remote.byteLength).toBe(size);
  expect(remote.every((value,index) => kind === "invalid-utf8" ? value === (index === 0 ? 255 : 254) : value === 99)).toBe(true);
  for (const [,id] of localFields) expect(textarea(container,id)?.readOnly).toBe(true);
  await act(async () => { root.unmount(); });
},30000);
