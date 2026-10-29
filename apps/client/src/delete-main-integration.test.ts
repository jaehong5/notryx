// @vitest-environment jsdom
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { EditorView } from "@codemirror/view";
import type { WorkspaceRegistration } from "../../../packages/protocol-types";

const fakes = vi.hoisted(() => ({
  root: undefined as { unmount(): void } | undefined,
  listeners: new Map<string, () => void>(),
  active: undefined as WorkspaceRegistration | undefined,
  etag: '"original-node"',
  noNode: false,
  queue: vi.fn(), status: vi.fn(), cancel: vi.fn(), send: vi.fn(), open:vi.fn(), save:vi.fn(), close:vi.fn(), kick:vi.fn(),
}));
vi.mock("react-dom/client", async (original) => {
  const actual = await original<typeof import("react-dom/client")>();
  return { ...actual, createRoot: (...args: Parameters<typeof actual.createRoot>) => {
    const root = actual.createRoot(...args); fakes.root = root; return root;
  } };
});
vi.mock("@tauri-apps/api/event", () => ({ listen: async (name: string, callback: () => void) => { fakes.listeners.set(name, callback); return () => fakes.listeners.delete(name); } }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
vi.mock("./migration-recovery", () => ({ StartupGate: ({ ready }: { ready: () => unknown }) => ready() }));
vi.mock("./auto-transmit",async(original)=>{const actual=await original<typeof import("./auto-transmit")>();return {...actual,AutoTransmit:class extends actual.AutoTransmit { override kick(){fakes.kick();super.kick();} }};});
vi.mock("./ipc", async (original) => {
  const actual = await original<typeof import("./ipc")>();
  return {
    ...actual,
    localDrafts: async () => [], pendingPairStatus: async () => "none",
    workspaceSyncState: async () => ({ binding:fakes.active, status:"ready" }),
    workspaceInventory: async () => ({ nodes:fakes.noNode ? [rootNode] : [rootNode, { ...node, node_etag:fakes.etag }], next_after_id:null }),
    workspaceDeleteStatus: fakes.status, queueWorkspaceDelete: fakes.queue,
    requestWorkspaceDeleteAbandon: fakes.cancel,
    openWorkspaceDocument:fakes.open, saveWorkspaceDocument:fakes.save, closeWorkspaceDocument:fakes.close,
    workspaceDocumentOutbox:async()=>({binding:fakes.active,document_id:nodeId,first_content_conflict:null, has_conflict:false,has_failure:false,latest:null,confirmed_through:"0"}),
    prepareWorkspaceRemoteRefresh:async()=>null, inspectWorkspaceRemoteChange:async()=>"same",
    sendNextWorkspaceOperation: fakes.send,
  };
});

const binding: WorkspaceRegistration = { server_origin:"https://example.invalid", workspace_id:"workspace", workspace_epoch:"epoch", device_id:"device", device_stream_id:"stream" };
const rootId = "019a1000-0000-7000-8000-000000000001";
const nodeId = "019a1000-0000-7000-8000-000000000002";
const rootNode = { node_id:rootId, parent_id:null, name:"", kind:"folder" as const, deleted:false, content_rev:null, content_etag:null, meta_rev:"meta", meta_etag:'"meta"', node_etag:'"root"', content_hash:null, size:null, children_generation:"0", last_change_seq:"1" };
const node = { ...rootNode, node_id:nodeId, parent_id:rootId, name:"Synthetic.md", kind:"file" as const, content_rev:"content", content_etag:'"content"', content_hash:"sha256:" + "a".repeat(64), size:"1" };
const entry = { operation_id:"019a1000-0000-7000-8000-000000000003", operation_seq:"1", node_id:nodeId, expected_node_etag:'"original-node"', status:"queued" as const, reason:null };
const rangeRects = Object.getOwnPropertyDescriptor(Range.prototype, "getClientRects");
const rangeBounds = Object.getOwnPropertyDescriptor(Range.prototype, "getBoundingClientRect");

beforeEach(() => {
  Object.defineProperty(Range.prototype, "getClientRects", { configurable:true, value:()=>[] });
  Object.defineProperty(Range.prototype, "getBoundingClientRect", { configurable:true, value:()=>({left:0,right:0,top:0,bottom:0,width:0,height:0}) });
  vi.resetModules(); fakes.listeners.clear(); fakes.active = binding; fakes.etag = '"original-node"'; fakes.noNode = false;
  fakes.queue.mockReset().mockResolvedValue(entry);
  fakes.status.mockReset().mockResolvedValue({ entries:[], next_after_seq:null });
  fakes.cancel.mockReset().mockResolvedValue(undefined);
  fakes.kick.mockReset();
  fakes.open.mockReset().mockResolvedValue({binding,document_id:nodeId,edit_session_id:"session",generation:"0",bytes_base64:btoa("base"),readonly:false,local_draft:false,has_session:true,cache_warning:null,open_notice:null});
  fakes.save.mockReset().mockImplementation(async(request:{edit_session_id:string;generation:string})=>({edit_session_id:request.edit_session_id,generation:request.generation}));
  fakes.close.mockReset().mockResolvedValue(undefined);
  fakes.send.mockReset().mockResolvedValue({ binding, status:"idle", operation_id:null, document_id:null, generation:null });
});
afterEach(async () => {
  fakes.root?.unmount(); fakes.root = undefined;
  await new Promise((resolve) => setTimeout(resolve,0)); document.body.replaceChildren(); vi.restoreAllMocks();
  if (rangeRects) Object.defineProperty(Range.prototype,"getClientRects",rangeRects); else delete (Range.prototype as Partial<Range>).getClientRects;
  if (rangeBounds) Object.defineProperty(Range.prototype,"getBoundingClientRect",rangeBounds); else delete (Range.prototype as Partial<Range>).getBoundingClientRect;
});
async function showDelete() {
  document.body.innerHTML = '<div id="root"></div>';
  await import("./main");
  await vi.waitFor(() => expect(document.querySelector(`[data-delete-node="${nodeId}"]`)).not.toBeNull());
  document.querySelector<HTMLButtonElement>(`[data-delete-node="${nodeId}"]`)!.click();
  await vi.waitFor(() => expect(document.querySelector('[aria-label="문서 삭제"]')).not.toBeNull());
}

test("목록에서 삭제 확인을 열고 사용자가 본 ETag를 durable queue로 보낸다", async () => {
  await showDelete();
  expect(fakes.queue).not.toHaveBeenCalled();
  fakes.etag = '"new-node"';
  fakes.listeners.get("workspace-metadata-changed")?.();
  await new Promise((resolve) => setTimeout(resolve,0));
  document.querySelector<HTMLButtonElement>('[data-testid="delete-confirm"]')!.click();
  await vi.waitFor(() => expect(fakes.queue).toHaveBeenCalledExactlyOnceWith(binding,{ node_id:nodeId, expected_node_etag:'"original-node"' }));
});

test("목록의 삭제 확인을 취소하면 요청과 전송을 시작하지 않는다", async () => {
  await showDelete();
  document.querySelector<HTMLButtonElement>('[data-testid="delete-confirm-cancel"]')!.click();
  await vi.waitFor(() => expect(document.querySelector('[aria-label="문서 삭제"]')).toBeNull());
  expect(fakes.queue).not.toHaveBeenCalled();
});

test("live 문서가 없어도 삭제 요청을 열어 미확정 작업을 취소할 수 있다", async () => {
  fakes.noNode = true;
  fakes.status.mockResolvedValue({ entries:[{ ...entry, status:"sent" }], next_after_seq:null });
  document.body.innerHTML = '<div id="root"></div>';
  await import("./main");
  await vi.waitFor(() => expect(document.querySelector('[data-testid="delete-requests-toggle"]')).not.toBeNull());
  document.querySelector<HTMLButtonElement>('[data-testid="delete-requests-toggle"]')!.click();
  await vi.waitFor(() => expect(document.querySelector(`[data-delete-cancel="${entry.operation_id}"]`)).not.toBeNull());
  expect(document.querySelector('[data-testid="delete-confirm"]')).toBeNull();
  document.querySelector<HTMLButtonElement>(`[data-delete-cancel="${entry.operation_id}"]`)!.click();
  await vi.waitFor(() => expect(fakes.cancel).toHaveBeenCalledExactlyOnceWith(binding, entry.operation_id));
});

test("열린 문서를 명시적으로 닫으면 세션을 정리하고 문서 삭제를 다시 선택할 수 있다",async()=>{
  document.body.innerHTML='<div id="root"></div>';await import("./main");
  await vi.waitFor(()=>expect(document.querySelector(`[data-document="${nodeId}"]:not(:disabled)`)).not.toBeNull());
  document.querySelector<HTMLButtonElement>(`[data-document="${nodeId}"]`)!.click();
  await vi.waitFor(()=>expect(document.querySelector('.cm-content[contenteditable=true]')).not.toBeNull());
  await vi.waitFor(()=>expect(document.querySelector('[data-testid="close-document"]')).not.toBeNull());
  document.querySelector<HTMLButtonElement>('[data-testid="close-document"]')!.click();
  await vi.waitFor(()=>expect(fakes.close).toHaveBeenCalledExactlyOnceWith({document_id:nodeId,edit_session_id:"session"}));
  await vi.waitFor(()=>expect(document.querySelector('.cm-content')).toBeNull());
});

test("문서 닫기의 마지막 checkpoint가 실패하면 입력과 편집 세션을 유지한다",async()=>{
  document.body.innerHTML='<div id="root"></div>';await import("./main");
  await vi.waitFor(()=>expect(document.querySelector(`[data-document="${nodeId}"]:not(:disabled)`)).not.toBeNull());
  document.querySelector<HTMLButtonElement>(`[data-document="${nodeId}"]`)!.click();
  await vi.waitFor(()=>expect(document.querySelector('.cm-content[contenteditable=true]')).not.toBeNull());
  const view=EditorView.findFromDOM(document.querySelector('.cm-editor')!)!;
  view.dispatch({changes:{from:view.state.doc.length,insert:"local edit"},userEvent:"input.type"});
  fakes.save.mockRejectedValueOnce(new Error("synthetic save failure"));
  await vi.waitFor(()=>expect(document.querySelector('[data-testid="close-document"]')).not.toBeNull());
  document.querySelector<HTMLButtonElement>('[data-testid="close-document"]')!.click();
  await vi.waitFor(()=>expect(document.body.textContent).toContain("synthetic save failure"));
  expect(fakes.close).not.toHaveBeenCalled();expect(view.state.doc.toString()).toBe("baselocal edit");
  expect(document.querySelector('.cm-content[contenteditable=true]')).not.toBeNull();
});

test("삭제 요청 목록을 닫은 뒤 취소가 저장되면 scheduler를 깨운다",async()=>{
  let release!:()=>void;fakes.cancel.mockImplementation(()=>new Promise<void>(resolve=>{release=resolve;}));
  fakes.noNode=true;fakes.status.mockResolvedValue({entries:[{...entry,status:"sent"}],next_after_seq:null});
  document.body.innerHTML='<div id="root"></div>';await import("./main");
  await vi.waitFor(()=>expect(document.querySelector('[data-testid="delete-requests-toggle"]')).not.toBeNull());
  document.querySelector<HTMLButtonElement>('[data-testid="delete-requests-toggle"]')!.click();
  await vi.waitFor(()=>expect(document.querySelector(`[data-delete-cancel="${entry.operation_id}"]`)).not.toBeNull());
  document.querySelector<HTMLButtonElement>(`[data-delete-cancel="${entry.operation_id}"]`)!.click();
  await vi.waitFor(()=>expect(fakes.cancel).toHaveBeenCalledOnce());
  document.querySelector<HTMLButtonElement>('[data-testid="delete-requests-toggle"]')!.click();
  await vi.waitFor(()=>expect(document.querySelector('[aria-label="문서 삭제"]')).toBeNull());
  fakes.kick.mockClear();release();
  await vi.waitFor(()=>expect(fakes.kick).toHaveBeenCalledOnce(),{timeout:3000});
},12000);
