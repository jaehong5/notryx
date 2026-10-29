// @vitest-environment jsdom
import { createElement } from "react";
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import type { WorkspaceDraft, WorkspaceOutboxEntry, WorkspaceOutboxPage, WorkspaceRegistration } from "../../../packages/protocol-types";
import type { FrozenContentReviewPorts } from "./frozen-content-review";

const fakes = vi.hoisted(() => ({
  root:undefined as { unmount():void } | undefined,
  active:undefined as WorkspaceRegistration | undefined,
  status:"ready", listeners:new Map<string, () => void>(),
  review:undefined as { binding:WorkspaceRegistration; ports:FrozenContentReviewPorts } | undefined,
  drafts:vi.fn(), outbox:vi.fn(), inspect:vi.fn(), queue:vi.fn(), abandon:vi.fn(), send:vi.fn(), kick:vi.fn(),
}));
vi.mock("react-dom/client", async (original) => {
  const actual = await original<typeof import("react-dom/client")>();
  return { ...actual, createRoot:(...args:Parameters<typeof actual.createRoot>) => { const root=actual.createRoot(...args); fakes.root=root; return root; } };
});
vi.mock("@tauri-apps/api/event", () => ({ listen:async (name:string, callback:() => void) => { fakes.listeners.set(name, callback); return () => { fakes.listeners.delete(name); }; } }));
vi.mock("@tauri-apps/api/core", () => ({ invoke:vi.fn() }));
vi.mock("./migration-recovery", () => ({ StartupGate:({ ready }:{ ready:() => unknown }) => ready() }));
vi.mock("./frozen-content-review", () => ({ FrozenContentReview:(props:{ binding:WorkspaceRegistration; ports:FrozenContentReviewPorts }) => {
  fakes.review=props; return createElement("section", { "data-testid":"frozen-content-probe" });
} }));
vi.mock("./auto-transmit", async (original) => {
  const actual = await original<typeof import("./auto-transmit")>();
  return { ...actual, AutoTransmit:class extends actual.AutoTransmit { override kick() { fakes.kick(); super.kick(); } } };
});
vi.mock("./ipc", async (original) => {
  const actual = await original<typeof import("./ipc")>();
  return { ...actual, localDrafts:fakes.drafts, workspaceOutbox:fakes.outbox, workspaceInspectFrozenContent:fakes.inspect,
    pendingPairStatus:async () => "none", workspaceSyncState:async () => ({ binding:fakes.active, status:fakes.status }),
    workspaceInventory:async () => ({ nodes:[rootNode], next_after_id:null }),
    queueWorkspaceDocument:fakes.queue, requestWorkspaceRestoreAbandon:fakes.abandon, sendNextWorkspaceOperation:fakes.send,
  };
});

const current:WorkspaceRegistration = { server_origin:"https://example.invalid", workspace_id:"workspace", workspace_epoch:"new-epoch", device_id:"device", device_stream_id:"stream" };
const oldA:WorkspaceRegistration = { ...current, workspace_epoch:"old-epoch", device_id:"old-a", device_stream_id:"stream-a" };
const oldB:WorkspaceRegistration = { ...current, device_id:"old-b", device_stream_id:"stream-b" };
const foreign:WorkspaceRegistration = { ...oldA, workspace_id:"foreign-workspace", device_stream_id:"foreign-stream" };
const rootId="019a1000-0000-7000-8000-000000000001", docA="019a1000-0000-7000-8000-000000000002", docB="019a1000-0000-7000-8000-000000000003", missing="019a1000-0000-7000-8000-000000000004";
const op1="019a1000-0000-7000-8000-000000000011", op2="019a1000-0000-7000-8000-000000000012", op3="019a1000-0000-7000-8000-000000000013", ignored="019a1000-0000-7000-8000-000000000014";
const rootNode={ node_id:rootId, parent_id:null, name:"", kind:"folder" as const, deleted:false, content_rev:null, content_etag:null, meta_rev:"meta", meta_etag:'"meta"', node_etag:'"root"', content_hash:null, size:null, children_generation:"0", last_change_seq:"1" };
const draft = (binding:WorkspaceRegistration, document_id:string, current_registration=false):WorkspaceDraft => ({ binding, document_id, generation:"3", current_registration, has_session:false });
const drafts = [draft(oldA,docA), draft(oldA,docB), draft(oldB,docA), draft(current,docA,true), draft(foreign,docA)];
const entry = (operation_id:string, document_id:string, operation_seq:string, status:"queued"|"sent" = "sent"):WorkspaceOutboxEntry => ({ operation_id, document_id, operation_seq, generation:"1", status, frozen:true, abandon_state:"none", failure:null, payload_retired:false });
const entries = [entry(op1,docA,"1"), entry(op2,docB,"2","queued"), entry(op3,docA,"1"), entry(ignored,missing,"3")];
const raw = (value:string) => new TextEncoder().encode(value);
function inspection(previous:WorkspaceRegistration, operation:string, document:string) {
  const record = entries.find((item) => item.operation_id === operation)!;
  return { current_binding:current, old_binding:previous, document_id:document, operation_id:operation, operation_seq:record.operation_seq,
    operation_status:record.status, abandon_state:record.abandon_state, operation_generation:record.generation, local_generation:"3", expected_revision:record.status === "sent" ? "base-rev" : null, outcome_unknown:record.status === "sent",
    base_revision:"base-rev", remote_revision:"remote-rev", operation_bytes:raw("payload"), base_bytes:raw("base"), local_bytes:raw("draft"), remote_bytes:raw("remote") };
}
function deferred<T>() { let resolve!:(value:T) => void; const promise=new Promise<T>((done) => { resolve=done; }); return {promise,resolve}; }
function same(left:WorkspaceRegistration, right:WorkspaceRegistration) { return JSON.stringify(left) === JSON.stringify(right); }
function outboxPage(binding:WorkspaceRegistration, after:string):WorkspaceOutboxPage {
  if (same(binding,oldA) && after === "0") return { entries:[entries[0]], next_after_seq:"1" };
  if (same(binding,oldA) && after === "1") return { entries:[entries[1],entries[3]], next_after_seq:null };
  if (same(binding,oldB) && after === "0") return { entries:[entries[2]], next_after_seq:null };
  return { entries:[], next_after_seq:null };
}

beforeEach(() => {
  vi.resetModules(); fakes.listeners.clear(); fakes.active=current; fakes.status="ready"; fakes.review=undefined;
  fakes.drafts.mockReset().mockResolvedValue(drafts);
  fakes.outbox.mockReset().mockImplementation(async (binding:WorkspaceRegistration, after:string) => outboxPage(binding,after));
  fakes.inspect.mockReset().mockImplementation(async (_current:WorkspaceRegistration, previous:WorkspaceRegistration, operation:string, document:string) => inspection(previous,operation,document));
  fakes.queue.mockReset(); fakes.abandon.mockReset(); fakes.kick.mockReset();
  fakes.send.mockReset().mockResolvedValue({ status:"idle", binding:current });
});
afterEach(async () => { fakes.root?.unmount(); fakes.root=undefined; fakes.review=undefined; await new Promise((resolve) => setTimeout(resolve,0)); fakes.listeners.clear(); document.body.replaceChildren(); vi.restoreAllMocks(); });

async function enter() {
  document.body.innerHTML='<div id="root"></div>'; await import("./main");
  await vi.waitFor(() => expect([...document.querySelectorAll("button")].some((button) => button.textContent === "이전 본문 요청 대조")).toBe(true));
  [...document.querySelectorAll("button")].find((button) => button.textContent === "이전 본문 요청 대조")!.click();
  await vi.waitFor(() => expect(document.querySelector('[data-testid="frozen-content-probe"]')).not.toBeNull());
  return fakes.review!;
}
async function allPages(ports:FrozenContentReviewPorts, binding:WorkspaceRegistration) {
  const found = []; let cursor:string|null = null;
  for (let index=0; index<10; index++) {
    const page=await ports.list(binding,cursor); expect(page.binding).toEqual(binding); found.push(...page.entries);
    cursor=page.next_after_id; if (cursor === null) return found;
  }
  throw new Error("pagination did not finish");
}

// The review is a port probe; its independent component tests cover presentation and selection races.
test("이전 초안의 등록·문서별 content 작업을 outbox 전체 페이지에서 모으고 선택 때만 원문을 조회한다", async () => {
  const review = await enter(); fakes.kick.mockClear(); fakes.send.mockClear();
  const found = await allPages(review.ports,current);
  expect(found.map((item) => item.operation_id).sort()).toEqual([op1,op2,op3].sort());
  expect(found.find((item) => item.operation_id === op1)).toMatchObject({ old_binding:oldA, document_id:docA, operation_seq:"1", status:"sent" });
  expect(found.find((item) => item.operation_id === op2)).toMatchObject({ old_binding:oldA, document_id:docB, operation_seq:"2", status:"queued" });
  expect(found.find((item) => item.operation_id === op3)).toMatchObject({ old_binding:oldB, document_id:docA, operation_seq:"1", status:"sent" });
  expect(fakes.outbox).toHaveBeenCalledWith(oldA,"0"); expect(fakes.outbox).toHaveBeenCalledWith(oldA,"1"); expect(fakes.outbox).toHaveBeenCalledWith(oldB,"0");
  expect(fakes.outbox.mock.calls.some(([binding]) => same(binding,foreign) || same(binding,current))).toBe(false);
  expect(fakes.inspect).not.toHaveBeenCalled(); expect(fakes.queue).not.toHaveBeenCalled(); expect(fakes.abandon).not.toHaveBeenCalled(); expect(fakes.kick).not.toHaveBeenCalled(); expect(fakes.send).not.toHaveBeenCalled();
  const shown = await review.ports.load(current,oldA,op2);
  expect(fakes.inspect).toHaveBeenCalledExactlyOnceWith(current,oldA,op2,docB);
  expect(shown).toMatchObject({ binding:current, old_binding:oldA, document_id:docB, operation_id:op2, operation_seq:"2", status:"queued", expected_revision:null });
  expect(new TextDecoder().decode(shown.local_bytes)).toBe("draft");
  expect(fakes.queue).not.toHaveBeenCalled(); expect(fakes.abandon).not.toHaveBeenCalled(); expect(fakes.kick).not.toHaveBeenCalled(); expect(fakes.send).not.toHaveBeenCalled();
});

test("목록에 없는 다른 workspace나 다른 문서의 작업을 상세 조회하지 않는다", async () => {
  const review=await enter(); await allPages(review.ports,current);
  await expect(review.ports.load(current,foreign,op1)).rejects.toThrow();
  await expect(review.ports.load(current,oldA,ignored)).rejects.toThrow();
  expect(fakes.inspect).not.toHaveBeenCalled(); expect(fakes.queue).not.toHaveBeenCalled(); expect(fakes.abandon).not.toHaveBeenCalled();
});

test("outbox 조회 중 현재 등록이 바뀌면 이전 등록 결과를 반환하지 않는다", async () => {
  const review=await enter(); const delayed=deferred<WorkspaceOutboxPage>();
  fakes.outbox.mockImplementationOnce(() => delayed.promise);
  const pending=review.ports.list(current,null);
  await vi.waitFor(() => expect(fakes.outbox).toHaveBeenCalled());
  fakes.active={ ...current, device_id:"replacement", device_stream_id:"replacement-stream" }; fakes.status="connecting";
  fakes.listeners.get("workspace-sync-status-changed")?.();
  await vi.waitFor(() => expect(document.querySelector("#workspace-sync-status")?.textContent).toContain("서버 동기화 중"));
  delayed.resolve({ entries:[entries[0]], next_after_seq:null });
  await expect(pending).rejects.toThrow();
  expect(fakes.inspect).not.toHaveBeenCalled(); expect(fakes.queue).not.toHaveBeenCalled(); expect(fakes.abandon).not.toHaveBeenCalled();
});

test("상세 조회 중 등록이 바뀌면 이전 응답을 새 등록에 반환하지 않는다", async () => {
  const review=await enter(); await allPages(review.ports,current);
  const delayed=deferred<ReturnType<typeof inspection>>(); fakes.inspect.mockReturnValueOnce(delayed.promise);
  const pending=review.ports.load(current,oldA,op1);
  await vi.waitFor(() => expect(fakes.inspect).toHaveBeenCalledOnce());
  fakes.active={ ...current, device_id:"replacement", device_stream_id:"replacement-stream" }; fakes.status="connecting";
  fakes.listeners.get("workspace-sync-status-changed")?.();
  await vi.waitFor(() => expect(document.querySelector("#workspace-sync-status")?.textContent).toContain("서버 동기화 중"));
  delayed.resolve(inspection(oldA,op1,docA));
  await expect(pending).rejects.toThrow();
  expect(fakes.queue).not.toHaveBeenCalled(); expect(fakes.abandon).not.toHaveBeenCalled();
});
