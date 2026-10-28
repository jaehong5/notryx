import { invoke } from "@tauri-apps/api/core";
import type { M0OpenedDocument, M0SavedCheckpoint, M0SaveRequest, M0CloseRequest, NodeState, WorkspaceDraft, WorkspacePairStatus, WorkspaceRegistration, WorkspaceOpenedDocument, WorkspaceSendResult, WorkspaceDocumentOutboxState, WorkspaceOutboxEntry, WorkspaceQueuedOperation } from "../../../packages/protocol-types";

function object(value: unknown): Record<string, unknown> {
  if (value === null || typeof value !== "object" || Array.isArray(value)) throw new Error("잘못된 저장 응답입니다.");
  return value as Record<string, unknown>;
}

function string(value: unknown): string {
  if (typeof value !== "string") throw new Error("잘못된 저장 응답입니다.");
  return value;
}

function generation(value: unknown): string {
  const text = string(value);
  if (!/^(0|[1-9][0-9]*)$/.test(text) || BigInt(text) > 9223372036854775807n) throw new Error("잘못된 편집 세대입니다.");
  return text;
}

let rendererSession: Promise<string> | undefined;

function getRendererSession(): Promise<string> {
  rendererSession ??= invoke("m0_renderer_session").then(string).catch((cause) => {
    rendererSession = undefined;
    throw cause;
  });
  return rendererSession;
}

export async function openDocument(document_id: string): Promise<M0OpenedDocument> {
  const renderer_session_id = await getRendererSession();
  const value = object(await invoke("m0_open", { request: { document_id, renderer_session_id } }));
  const reply = {
    document_id: string(value.document_id), edit_session_id: string(value.edit_session_id),
    generation: generation(value.generation), bytes_base64: string(value.bytes_base64),
  };
  if (reply.document_id !== document_id) throw new Error("다른 문서의 응답입니다.");
  return reply;
}

export async function saveDocument(request: M0SaveRequest): Promise<M0SavedCheckpoint> {
  const value = object(await invoke("m0_save", { request }));
  return { edit_session_id: string(value.edit_session_id), generation: generation(value.generation) };
}

export const closeDocument = (request: M0CloseRequest) => invoke<void>("m0_close", { request });

export async function pairWorkspace(origin: string, workspace_id: string, pairing_secret: string): Promise<void> {
  const renderer_session_id = await getRendererSession();
  const value = object(await invoke("workspace_pair", { request: { renderer_session_id, origin, workspace_id, pairing_secret } }));
  if (string(value.workspace_id) !== workspace_id || string(value.protocol) !== "mdw-sync/1.2") throw new Error("작업 공간 응답이 일치하지 않습니다.");
}

function nullableString(value: unknown): string | null {
  return value === null ? null : string(value);
}

function nodeState(value: unknown): NodeState {
  const node = object(value);
  if (node.kind !== "file" && node.kind !== "folder") throw new Error("잘못된 문서 목록입니다.");
  if (typeof node.deleted !== "boolean") throw new Error("잘못된 문서 목록입니다.");
  return {
    node_id: string(node.node_id), parent_id: nullableString(node.parent_id), name: string(node.name),
    kind: node.kind, deleted: node.deleted, content_rev: nullableString(node.content_rev),
    content_etag: nullableString(node.content_etag), meta_rev: string(node.meta_rev),
    meta_etag: string(node.meta_etag), node_etag: string(node.node_etag),
    content_hash: nullableString(node.content_hash), size: nullableString(node.size),
    children_generation: generation(node.children_generation), last_change_seq: generation(node.last_change_seq),
  };
}

export async function workspaceInventory(after_id: string | null): Promise<{ nodes: NodeState[]; next_after_id: string | null }> {
  const renderer_session_id = await getRendererSession();
  const value = object(await invoke("workspace_inventory", { request: { renderer_session_id, after_id } }));
  if (!Array.isArray(value.nodes) || value.nodes.length > 100) throw new Error("잘못된 문서 목록입니다.");
  return { nodes: value.nodes.map(nodeState), next_after_id: nullableString(value.next_after_id) };
}

function registration(value: unknown): WorkspaceRegistration {
  const binding = object(value);
  return { server_origin: string(binding.server_origin), workspace_id: string(binding.workspace_id),
    workspace_epoch: string(binding.workspace_epoch), device_id: string(binding.device_id), device_stream_id: string(binding.device_stream_id) };
}

function openedWorkspace(value: unknown, documentId: string): WorkspaceOpenedDocument {
  const data = object(value);
  if (typeof data.readonly !== "boolean" || typeof data.local_draft !== "boolean") throw new Error("잘못된 문서 응답입니다.");
  const opened = {
    document_id: string(data.document_id), edit_session_id: string(data.edit_session_id),
    generation: generation(data.generation), bytes_base64: string(data.bytes_base64), readonly: data.readonly, local_draft: data.local_draft,
    binding: registration(data.binding),
  };
  if (opened.document_id !== documentId) throw new Error("다른 문서의 응답입니다.");
  return opened;
}

export async function openWorkspaceDocument(node: NodeState): Promise<WorkspaceOpenedDocument> {
  const renderer_session_id = await getRendererSession();
  return openedWorkspace(await invoke("workspace_open", { request: { renderer_session_id, node } }), node.node_id);
}

export async function localDrafts(): Promise<WorkspaceDraft[]> {
  const renderer_session_id = await getRendererSession();
  const value: unknown = await invoke("workspace_local_drafts", { request: { renderer_session_id } });
  if (!Array.isArray(value)) throw new Error("잘못된 초안 목록입니다.");
  return value.map((item) => {
    const draft = object(item);
    if (typeof draft.current_registration !== "boolean" || typeof draft.has_session !== "boolean") throw new Error("잘못된 초안 목록입니다.");
    return {
      binding: registration(draft.binding),
      document_id: string(draft.document_id), generation: generation(draft.generation),
      current_registration: draft.current_registration, has_session: draft.has_session,
    };
  });
}

export async function pendingPairStatus(): Promise<WorkspacePairStatus> {
  const renderer_session_id = await getRendererSession();
  const value: unknown = await invoke("workspace_pair_status", { request: { renderer_session_id } });
  if (value !== "none" && value !== "in_progress" && value !== "pending_adoption") throw new Error("잘못된 등록 상태입니다.");
  return value;
}

export async function openLocalDraft(draft: WorkspaceDraft): Promise<WorkspaceOpenedDocument> {
  const renderer_session_id = await getRendererSession();
  return openedWorkspace(await invoke("workspace_open_draft", { request: { renderer_session_id, binding: draft.binding, document_id: draft.document_id } }), draft.document_id);
}

export async function recoverWorkspaceSource(document_id: string, edit_session_id: string, bytes: Uint8Array, action: "copy" | "export"): Promise<boolean> {
  if (bytes.length > 64 * 1024 * 1024) throw new Error("recovery_too_large");
  const renderer_session_id = await getRendererSession();
  const value: unknown = await invoke(action === "copy" ? "workspace_copy_source" : "workspace_export_source", {
    request: { renderer_session_id, document_id, edit_session_id, bytes_base64: toBase64(bytes) },
  });
  if (action === "copy") return true;
  if (typeof value !== "boolean") throw new Error("잘못된 내보내기 응답입니다.");
  return value;
}

export async function adoptPendingPair(): Promise<void> {
  const renderer_session_id = await getRendererSession();
  object(await invoke("workspace_adopt_pending_pair", { request: { renderer_session_id } }));
}

export async function discardPendingPair(): Promise<void> {
  const renderer_session_id = await getRendererSession();
  await invoke("workspace_discard_pending_pair", { request: { renderer_session_id } });
}

export async function saveWorkspaceDocument(request: M0SaveRequest): Promise<M0SavedCheckpoint> {
  const value = object(await invoke("workspace_save", { request }));
  return { edit_session_id: string(value.edit_session_id), generation: generation(value.generation) };
}

export const closeWorkspaceDocument = (request: M0CloseRequest) => invoke<void>("workspace_close", { request });

export async function queueWorkspaceDocument(document_id: string, edit_session_id: string, expected_generation: string): Promise<WorkspaceQueuedOperation> {
  const renderer_session_id = await getRendererSession();
  const value = object(await invoke("workspace_queue", { request: { renderer_session_id, document_id, edit_session_id, expected_generation } }));
  if (typeof value.committed !== "boolean") throw new Error("잘못된 전송 상태입니다.");
  const queued = { operation_id: string(value.operation_id), operation_seq: generation(value.operation_seq), document_id: string(value.document_id), generation: generation(value.generation), committed: value.committed };
  if (queued.document_id !== document_id) throw new Error("다른 문서의 전송 요청입니다.");
  return queued;
}

export async function sendNextWorkspaceOperation(): Promise<WorkspaceSendResult> {
  const renderer_session_id = await getRendererSession();
  const value = object(await invoke("workspace_send_next", { request: { renderer_session_id } }));
  const status = string(value.status);
  if (status !== "idle" && status !== "pending" && status !== "committed" && status !== "conflict" && status !== "failed" && status !== "abandoned" && status !== "needs_review" && status !== "ack_pending" && status !== "auth_required" && status !== "gateway_auth_required") throw new Error("잘못된 전송 상태입니다.");
  return { binding: registration(value.binding), status, operation_id: nullableString(value.operation_id), document_id: nullableString(value.document_id), generation: value.generation === null ? null : generation(value.generation) };
}

export async function previewWorkspaceImage(document_id: string, edit_session_id: string, node: NodeState): Promise<Uint8Array> {
  const renderer_session_id = await getRendererSession();
  const value = object(await invoke("workspace_preview_image", { request: { renderer_session_id, document_id, edit_session_id, node } }));
  if (typeof value.width !== "number" || typeof value.height !== "number" || !Number.isInteger(value.width) || !Number.isInteger(value.height) || value.width <= 0 || value.height <= 0 || value.width > 8192 || value.height > 8192 || value.width * value.height > 16_000_000) throw new Error("잘못된 이미지 응답입니다.");
  return fromBase64(string(value.png_base64));
}

export async function openWorkspaceExternal(document_id: string, edit_session_id: string, url: string): Promise<void> {
  const renderer_session_id = await getRendererSession();
  await invoke("workspace_open_external", { request: { renderer_session_id, document_id, edit_session_id, url } });
}

function outboxEntry(value: unknown): WorkspaceOutboxEntry {
  const entry = object(value);
  const status = string(entry.status);
  const abandon = string(entry.abandon_state);
  const failure = nullableString(entry.failure);
  if (status !== "queued" && status !== "sent" && status !== "committed" && status !== "conflict" && status !== "abandoning" && status !== "abandoned" && status !== "failed") throw new Error("잘못된 전송 상태입니다.");
  if (abandon !== "none" && abandon !== "requested" && abandon !== "sent") throw new Error("잘못된 취소 상태입니다.");
  if (failure !== null && failure !== "node_deleted" && failure !== "recovery_conflict" && failure !== "not_found" && failure !== "not_applied") throw new Error("잘못된 전송 오류입니다.");
  if (typeof entry.frozen !== "boolean" || typeof entry.payload_retired !== "boolean") throw new Error("잘못된 등록 상태입니다.");
  return { operation_id: string(entry.operation_id), operation_seq: generation(entry.operation_seq), document_id: string(entry.document_id),
    generation: generation(entry.generation), status, frozen: entry.frozen, abandon_state: abandon, failure, payload_retired: entry.payload_retired };
}

export async function workspaceDocumentOutbox(binding: WorkspaceRegistration, document_id: string): Promise<WorkspaceDocumentOutboxState> {
  const renderer_session_id = await getRendererSession();
  const value = object(await invoke("workspace_document_outbox", { request: { renderer_session_id, binding, document_id } }));
  if (typeof value.has_conflict !== "boolean" || typeof value.has_failure !== "boolean") throw new Error("잘못된 전송 상태입니다.");
  const result = { binding: registration(value.binding), document_id: string(value.document_id), latest: value.latest === null ? null : outboxEntry(value.latest),
    has_conflict: value.has_conflict, has_failure: value.has_failure, confirmed_through: generation(value.confirmed_through) };
  if (result.document_id !== document_id || JSON.stringify(result.binding) !== JSON.stringify(binding)) throw new Error("다른 등록의 전송 상태입니다.");
  return result;
}

export function fromBase64(base64: string): Uint8Array {
  const maxBytes = 25 * 1024 * 1024;
  if (base64.length > Math.ceil(maxBytes / 3) * 4) throw new Error("문서가 너무 큽니다.");
  const native = (Uint8Array as typeof Uint8Array & { fromBase64?: (value: string) => Uint8Array }).fromBase64;
  if (typeof native === "function") {
    let decoded: Uint8Array | undefined;
    try { decoded = native.call(Uint8Array, base64); } catch { /* Preserve the atob error and compatibility behavior. */ }
    if (decoded) {
      if (decoded.length > maxBytes) throw new Error("문서가 너무 큽니다.");
      return decoded;
    }
  }
  const binary = atob(base64);
  if (binary.length > maxBytes) throw new Error("문서가 너무 큽니다.");
  const bytes = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i);
  return bytes;
}

export function toBase64(bytes: Uint8Array): string {
  if (bytes.length === 0) return "";
  const native = (bytes as Uint8Array & { toBase64?: () => string }).toBase64;
  if (typeof native === "function") return native.call(bytes);
  let binary = "";
  for (let i = 0; i < bytes.length; i += 8192) binary += String.fromCharCode(...bytes.subarray(i, i + 8192));
  return btoa(binary);
}
