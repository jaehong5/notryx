import { invoke } from "@tauri-apps/api/core";
import type { M0OpenedDocument, M0SavedCheckpoint, M0SaveRequest, M0CloseRequest, NodeState, WorkspaceDraft, WorkspacePairStatus, WorkspaceRegistration, WorkspaceOpenedDocument, WorkspaceSendResult, WorkspaceDocumentOutboxState, WorkspaceOutboxEntry, WorkspaceOutboxPage, WorkspaceQueuedOperation, WorkspaceSyncState } from "../../../packages/protocol-types";
import type { RestoreNodeRequest, TrashPage } from "../../../packages/protocol-types";
import type { RestoreDisplay } from "./trash-panel";
import type { CreateDisplay } from "./create-panel";
import type { DeleteDisplay } from "./delete-panel";
import type { MoveDisplay } from "./move-panel";
import type { MoveNodeRequest } from "../../../packages/protocol-types";
import type { FrozenDeleteReviewChoice, FrozenDeleteReviewItem } from "./frozen-delete-review";
import type { FrozenMoveReviewChoice, FrozenMoveReviewItem } from "./frozen-move-review";
import type { FrozenCreateReviewChoice, FrozenCreateReviewItem } from "./frozen-create-review";
import type { SearchPage, SearchPanelPorts, SearchResult } from "./search-panel";
import type { ConflictReviewSnapshot } from "./conflict-review";
import type { FrozenContentLocalReviewSnapshot, FrozenContentRecordedDecision, FrozenContentReviewSnapshot } from "./frozen-content-review";
import type { WorkspaceFrozenContentInspection } from "../../../packages/protocol-types";
import type { WorkspaceContentConflictMergeFollowup, WorkspaceContentConflictMergeFollowupApprovalRequest, WorkspaceContentConflictMergeFollowupStatusResponse, WorkspaceContentConflictMergeStatusRequest, WorkspaceContentConflictMergeStatusResponse } from "../../../packages/protocol-types";
import type { WorkspaceConflictResolutionResponse, WorkspaceContentConflictChooseRemoteRequest, WorkspaceContentConflictChooseRemoteResponse, WorkspaceContentConflictCopyProgressResponse, WorkspaceContentConflictCopyResponse, WorkspaceContentConflictCopyStatusResponse, WorkspaceContentConflictProgressResponse, WorkspaceContentConflictReopenResponse, WorkspaceDeletedDraftCopyConfirmResult, WorkspaceDeletedDraftCopyFollowupAbandonRequest, WorkspaceDeletedDraftCopyFollowupAbandonResult, WorkspaceDeletedDraftCopyProgressResult, WorkspaceDeletedDraftCopyReconfirmRequest, WorkspaceDeletedDraftCopyReopenResponse, WorkspaceDeletedDraftCopySnapshot, WorkspaceDeletedDraftCopyStatus } from "../../../packages/protocol-types";

type SearchPageWithNodes = SearchPage & { results: (SearchResult & { node: NodeState | null })[] };

function searchRequestId(value: string): string {
  if (typeof value !== "string" || value.length > 16 || !/^[1-9][0-9]*$/.test(value) || !Number.isSafeInteger(Number(value))) throw new Error("잘못된 검색 요청입니다.");
  return value;
}

function searchCount(value: unknown): number {
  if (typeof value !== "number" || !Number.isInteger(value) || value < 0 || value > 0xffffffff) throw new Error("잘못된 검색 범위입니다.");
  return value;
}

export async function workspaceSearch(binding: WorkspaceRegistration, request: Parameters<SearchPanelPorts["scan"]>[1]): Promise<SearchPageWithNodes> {
  const request_id = searchRequestId(request.request_id);
  if (!request.query || new TextEncoder().encode(request.query).length > 4096 || (request.scope !== "workspace" && request.scope !== "recovery")) throw new Error("잘못된 검색 요청입니다.");
  const after_id = request.after === null ? null : nodeId(request.after);
  const renderer_session_id = await getRendererSession();
  const value = object(await invoke("workspace_search", { request: { renderer_session_id, binding, request_id, query:request.query, scope:request.scope, after_id } }));
  if (!sameRegistration(registration(value.binding), binding) || value.request_id !== request_id || !Array.isArray(value.results) || value.results.length > 200) throw new Error("잘못된 검색 응답입니다.");
  const available_documents = searchCount(value.available_documents), total_documents = searchCount(value.total_documents);
  const recovery_available_documents = searchCount(value.recovery_available_documents), recovery_total_documents = searchCount(value.recovery_total_documents);
  if (available_documents > total_documents || recovery_available_documents > recovery_total_documents) throw new Error("잘못된 검색 범위입니다.");
  let previous = after_id;
  const results = value.results.map((raw: unknown): SearchResult & { node: NodeState | null } => {
    const item = object(raw), node_id = nodeId(item.node_id), source = item.source;
    if ((previous !== null && node_id <= previous) || item.scope !== request.scope || (source !== "draft" && source !== "cache") || (item.scope === "recovery" && source !== "draft")) throw new Error("잘못된 검색 결과입니다.");
    previous = node_id;
    const node = item.node === null ? null : nodeState(item.node);
    if ((request.scope === "workspace" && (!node || node.deleted)) || (node && (nodeId(node.node_id) !== node_id || node.kind !== "file" || (node.parent_id !== null && nodeId(node.parent_id) === node_id)))) throw new Error("잘못된 검색 문서입니다.");
    return { node_id, scope:request.scope, source, node };
  });
  const next_after_id = value.next_after_id === null ? null : nodeId(value.next_after_id);
  if (next_after_id !== null && ((after_id !== null && next_after_id <= after_id) || (previous !== null && next_after_id < previous))) throw new Error("잘못된 검색 구간입니다.");
  if (results.length > (request.scope === "workspace" ? available_documents : recovery_available_documents)) throw new Error("잘못된 검색 범위입니다.");
  return { results, available_documents, total_documents, recovery_available_documents, recovery_total_documents, next_after_id };
}

export async function cancelWorkspaceSearch(binding: WorkspaceRegistration, requestId: string): Promise<void> {
  const request_id = searchRequestId(requestId);
  const renderer_session_id = await getRendererSession();
  await invoke("workspace_cancel_search", { request: { renderer_session_id, binding, request_id } });
}

function nodeId(value: unknown): string {
  const id = string(value);
  if (!/^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(id)) throw new Error("잘못된 항목 식별자입니다.");
  return id;
}

function sameRegistration(a: WorkspaceRegistration, b: WorkspaceRegistration): boolean {
  return a.server_origin === b.server_origin && a.workspace_id === b.workspace_id && a.workspace_epoch === b.workspace_epoch && a.device_id === b.device_id && a.device_stream_id === b.device_stream_id;
}

function restoreRequest(value: unknown): RestoreNodeRequest {
  const request = object(value);
  const expected_meta_rev = string(request.expected_meta_rev);
  const name = string(request.name);
  if (!expected_meta_rev || new TextEncoder().encode(expected_meta_rev).length > 256 || !name || new TextEncoder().encode(name).length > 255) throw new Error("잘못된 복원 요청입니다.");
  return { expected_meta_rev, parent_id: nodeId(request.parent_id), name };
}

function restoreDisplay(value: unknown, binding: WorkspaceRegistration): RestoreDisplay {
  const item = object(value);
  const operation_id = nodeId(item.operation_id);
  const operation_seq = generation(item.operation_seq);
  const node_id = nodeId(item.node_id);
  const request = restoreRequest(item.request);
  const status = string(item.status);
  if (operation_seq === "0" || node_id === request.parent_id || (status !== "queued" && status !== "sent" && status !== "abandoning" && status !== "abandoned" && status !== "committed" && status !== "failed")) throw new Error("잘못된 복원 상태입니다.");
  let reason: string | null = null;
  if (status === "queued" || status === "sent" || status === "abandoning") {
    if (item.receipt !== null) throw new Error("잘못된 복원 결과입니다.");
  } else {
    const receipt = object(item.receipt);
    const receiptStatus = string(receipt.status);
    if (string(receipt.operation_id) !== operation_id || generation(receipt.operation_seq) !== operation_seq || string(receipt.workspace_epoch) !== binding.workspace_epoch) throw new Error("다른 복원 결과입니다.");
    if (status === "abandoned") {
      if (receiptStatus !== "abandoned") throw new Error("잘못된 복원 취소 결과입니다.");
    } else if (status === "committed") {
      if (receiptStatus !== "restored") throw new Error("잘못된 복원 결과입니다.");
      const restored = nodeState(receipt.node);
      if (restored.node_id !== node_id || restored.parent_id !== request.parent_id || restored.name !== request.name || restored.deleted || restored.last_change_seq !== generation(receipt.commit_seq)) throw new Error("잘못된 복원 결과입니다.");
    } else {
      if (!["recovery_conflict", "node_deleted", "not_deleted", "history_expired", "metadata_precondition_failed", "destination_exists", "parent_not_folder", "not_found", "not_applied"].includes(receiptStatus)) throw new Error("잘못된 복원 오류입니다.");
      reason = receiptStatus;
    }
  }
  return { operation_id, operation_seq, node_id, request, status, reason };
}

export async function workspaceTrash(binding: WorkspaceRegistration, after: string | null): Promise<TrashPage> {
  const renderer_session_id = await getRendererSession();
  if (after !== null) nodeId(after);
  const page = object(await invoke("workspace_trash", { request: { renderer_session_id, after_id: after } }));
  const workspace_epoch = string(page.workspace_epoch);
  if (workspace_epoch !== binding.workspace_epoch || !Array.isArray(page.nodes) || page.nodes.length > 100) throw new Error("잘못된 휴지통 응답입니다.");
  let previous = after;
  const nodes = page.nodes.map((value: unknown): TrashPage["nodes"][number] => {
    const item = object(value);
    const node_id = nodeId(item.node_id), parent_id = nodeId(item.parent_id), name = string(item.name), meta_rev = string(item.meta_rev);
    if (node_id === parent_id || (previous !== null && node_id <= previous) || !name || !meta_rev || new TextEncoder().encode(meta_rev).length > 256 || (item.kind !== "file" && item.kind !== "folder") || typeof item.restorable !== "boolean") throw new Error("잘못된 휴지통 항목입니다.");
    previous = node_id;
    return { node_id, parent_id, name, kind: item.kind, meta_rev, restorable: item.restorable };
  });
  const next_after_id = page.next_after_id === null ? null : nodeId(page.next_after_id);
  if (next_after_id !== null && (!nodes.length || next_after_id !== nodes[nodes.length - 1].node_id)) throw new Error("잘못된 휴지통 페이지입니다.");
  return { workspace_epoch, nodes, next_after_id };
}

export async function queueWorkspaceRestore(binding: WorkspaceRegistration, node: string, request: RestoreNodeRequest): Promise<RestoreDisplay> {
  const renderer_session_id = await getRendererSession();
  const expectedNode = nodeId(node), expectedRequest = restoreRequest(request);
  const item = restoreDisplay(await invoke("workspace_queue_restore", { request: { renderer_session_id, binding, node_id: expectedNode, request: expectedRequest } }), binding);
  if (item.status !== "queued" || item.node_id !== expectedNode || item.request.expected_meta_rev !== expectedRequest.expected_meta_rev || item.request.parent_id !== expectedRequest.parent_id || item.request.name !== expectedRequest.name) throw new Error("다른 복원 요청의 응답입니다.");
  return item;
}

export async function workspaceRestoreStatus(binding: WorkspaceRegistration, after: string): Promise<{ entries: RestoreDisplay[]; next_after_seq: string | null }> {
  const renderer_session_id = await getRendererSession();
  generation(after);
  const page = object(await invoke("workspace_restore_status", { request: { renderer_session_id, binding, after_seq: after, limit: 100 } }));
  if (!sameRegistration(registration(page.binding), binding) || !Array.isArray(page.entries) || page.entries.length > 100) throw new Error("잘못된 복원 상태입니다.");
  let previous = BigInt(after);
  const entries = page.entries.map((value: unknown) => {
    const item = restoreDisplay(value, binding), sequence = BigInt(item.operation_seq);
    if (sequence <= previous) throw new Error("잘못된 복원 순서입니다.");
    previous = sequence;
    return item;
  });
  const next_after_seq = page.next_after_seq === null ? null : generation(page.next_after_seq);
  if (next_after_seq !== null && (!entries.length || next_after_seq !== entries[entries.length - 1].operation_seq)) throw new Error("잘못된 복원 페이지입니다.");
  return { entries, next_after_seq };
}

export async function requestWorkspaceRestoreAbandon(binding: WorkspaceRegistration, operationId: string): Promise<void> {
  const operation_id = nodeId(operationId);
  const renderer_session_id = await getRendererSession();
  await invoke("workspace_request_restore_abandon", { request: { renderer_session_id, binding, operation_id } });
}

function createDisplay(value: unknown, binding: WorkspaceRegistration): CreateDisplay {
  const item = object(value), request = object(item.request);
  const operation_id = nodeId(item.operation_id), operation_seq = generation(item.operation_seq);
  const node_id = nodeId(item.node_id), parent_id = nodeId(request.parent_id), name = string(request.name);
  const status = string(item.status);
  if (operation_seq === "0" || nodeId(request.node_id) !== node_id || parent_id === node_id || !name || new TextEncoder().encode(name).length > 255 || (request.kind !== "folder" && request.kind !== "file") || (request.upload_id !== undefined && request.upload_id !== null) || !["queued", "sent", "abandoning", "abandoned", "committed", "failed"].includes(status)) throw new Error("잘못된 생성 상태입니다.");
  let reason: string | null = null;
  if (status === "queued" || status === "sent" || status === "abandoning") {
    if (item.receipt !== null) throw new Error("잘못된 생성 결과입니다.");
  } else {
    const receipt = object(item.receipt), receiptStatus = string(receipt.status);
    if (string(receipt.operation_id) !== operation_id || generation(receipt.operation_seq) !== operation_seq || string(receipt.workspace_epoch) !== binding.workspace_epoch) throw new Error("다른 생성 결과입니다.");
    if (status === "abandoned") {
      if (receiptStatus !== "abandoned") throw new Error("잘못된 취소 결과입니다.");
    } else if (status === "committed") {
      if (receiptStatus !== "created") throw new Error("잘못된 생성 결과입니다.");
      const node = nodeState(receipt.node);
      if (node.node_id !== node_id || node.parent_id !== parent_id || node.name !== name || node.kind !== request.kind || node.deleted || node.last_change_seq !== generation(receipt.commit_seq) || (node.kind === "folder" ? node.content_rev !== null || node.content_etag !== null || node.content_hash !== null || node.size !== null : node.content_rev === null || node.content_etag !== `"${node.content_rev}"` || node.content_hash !== "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855" || (node.size === null || !/^0+$/.test(node.size)) || node.children_generation !== "0")) throw new Error("잘못된 생성 결과입니다.");
    } else {
      if (!["recovery_conflict", "node_deleted", "destination_exists", "node_id_conflict", "parent_not_folder", "not_found", "not_applied"].includes(receiptStatus)) throw new Error("잘못된 생성 오류입니다.");
      reason = receiptStatus;
    }
  }
  return { operation_id, operation_seq, node_id, parent_id, name, kind: request.kind as "file" | "folder", status: status as CreateDisplay["status"], reason };
}

export async function queueWorkspaceCreate(binding: WorkspaceRegistration, request: { parent_id: string; name: string; kind: "file" | "folder" }): Promise<CreateDisplay> {
  const parent_id = nodeId(request.parent_id), name = string(request.name), kind = request.kind;
  if (kind !== "file" && kind !== "folder") throw new Error("잘못된 생성 종류입니다.");
  if (!name || new TextEncoder().encode(name).length > 255) throw new Error("잘못된 폴더 이름입니다.");
  const renderer_session_id = await getRendererSession();
  const item = createDisplay(await invoke("workspace_queue_create", { request: { renderer_session_id, binding, parent_id, name, kind } }), binding);
  if (item.status !== "queued" || item.parent_id !== parent_id || item.name !== name || item.kind !== kind) throw new Error("다른 생성 요청의 응답입니다.");
  return item;
}

export async function workspaceCreateStatus(binding: WorkspaceRegistration, after: string): Promise<{ entries: CreateDisplay[]; next_after_seq: string | null }> {
  const renderer_session_id = await getRendererSession();
  generation(after);
  const page = object(await invoke("workspace_create_status", { request: { renderer_session_id, binding, after_seq: after, limit: 100 } }));
  if (!sameRegistration(registration(page.binding), binding) || !Array.isArray(page.entries) || page.entries.length > 100) throw new Error("잘못된 생성 상태입니다.");
  let previous = BigInt(after);
  const entries = page.entries.map((value: unknown) => { const item = createDisplay(value, binding), sequence = BigInt(item.operation_seq); if (sequence <= previous) throw new Error("잘못된 생성 순서입니다."); previous = sequence; return item; });
  const next_after_seq = page.next_after_seq === null ? null : generation(page.next_after_seq);
  if (next_after_seq !== null && (!entries.length || next_after_seq !== entries[entries.length - 1].operation_seq)) throw new Error("잘못된 생성 페이지입니다.");
  return { entries, next_after_seq };
}

export async function requestWorkspaceCreateAbandon(binding: WorkspaceRegistration, operationId: string): Promise<void> {
  const operation_id = nodeId(operationId), renderer_session_id = await getRendererSession();
  await invoke("workspace_request_create_abandon", { request: { renderer_session_id, binding, operation_id } });
}

function nodeEtag(value: unknown): string {
  const etag = string(value);
  if (new TextEncoder().encode(etag).length > 256 || !/^"[\x21\x23-\x7e\u0080-\u{10ffff}]*"$/u.test(etag)) throw new Error("잘못된 문서 상태입니다.");
  return etag;
}

function deleteDisplay(value: unknown, binding: WorkspaceRegistration): DeleteDisplay {
  const item = object(value);
  const operation_id = nodeId(item.operation_id), operation_seq = generation(item.operation_seq);
  const node_id = nodeId(item.node_id), expected_node_etag = nodeEtag(item.expected_node_etag);
  const status = string(item.status);
  if (operation_seq === "0" || !["queued", "sent", "abandoning", "abandoned", "committed", "failed"].includes(status)) throw new Error("잘못된 삭제 상태입니다.");
  let reason: string | null = null;
  if (status === "queued" || status === "sent" || status === "abandoning") {
    if (item.receipt !== null) throw new Error("잘못된 삭제 결과입니다.");
  } else {
    const receipt = object(item.receipt), receiptStatus = string(receipt.status);
    if (nodeId(receipt.operation_id) !== operation_id || generation(receipt.operation_seq) !== operation_seq || string(receipt.workspace_epoch) !== binding.workspace_epoch) throw new Error("다른 삭제 결과입니다.");
    if (status === "abandoned") {
      if (receiptStatus !== "abandoned") throw new Error("잘못된 취소 결과입니다.");
    } else if (status === "committed") {
      if (receiptStatus !== "deleted") throw new Error("잘못된 삭제 결과입니다.");
      const node = nodeState(receipt.node);
      if (node.node_id !== node_id || node.kind !== "file" || !node.deleted || node.last_change_seq !== generation(receipt.commit_seq)) throw new Error("잘못된 삭제 결과입니다.");
    } else {
      if (!["recovery_conflict", "node_deleted", "node_precondition_failed", "not_found", "not_applied"].includes(receiptStatus)) throw new Error("잘못된 삭제 오류입니다.");
      if (receiptStatus === "node_precondition_failed" && nodeEtag(receipt.current_node_etag) === expected_node_etag) throw new Error("잘못된 삭제 오류입니다.");
      reason = receiptStatus;
    }
  }
  return { operation_id, operation_seq, node_id, expected_node_etag, status: status as DeleteDisplay["status"], reason };
}

export async function queueWorkspaceDelete(binding: WorkspaceRegistration, request: { node_id: string; expected_node_etag: string }): Promise<DeleteDisplay> {
  const node_id = nodeId(request.node_id), expected_node_etag = nodeEtag(request.expected_node_etag);
  const renderer_session_id = await getRendererSession();
  const item = deleteDisplay(await invoke("workspace_queue_delete", { request: { renderer_session_id, binding, node_id, expected_node_etag } }), binding);
  if (item.status !== "queued" || item.node_id !== node_id || item.expected_node_etag !== expected_node_etag) throw new Error("다른 삭제 요청의 응답입니다.");
  return item;
}

export async function workspaceDeleteStatus(binding: WorkspaceRegistration, after: string): Promise<{ entries: DeleteDisplay[]; next_after_seq: string | null }> {
  const renderer_session_id = await getRendererSession();
  generation(after);
  const page = object(await invoke("workspace_delete_status", { request: { renderer_session_id, binding, after_seq: after, limit: 100 } }));
  if (!sameRegistration(registration(page.binding), binding) || !Array.isArray(page.entries) || page.entries.length > 100) throw new Error("잘못된 삭제 상태입니다.");
  let previous = BigInt(after);
  const entries = page.entries.map((value: unknown) => {
    const item = deleteDisplay(value, binding), sequence = BigInt(item.operation_seq);
    if (sequence <= previous) throw new Error("잘못된 삭제 순서입니다.");
    previous = sequence;
    return item;
  });
  const next_after_seq = page.next_after_seq === null ? null : generation(page.next_after_seq);
  if (next_after_seq !== null && (!entries.length || next_after_seq !== entries[entries.length - 1].operation_seq)) throw new Error("잘못된 삭제 페이지입니다.");
  return { entries, next_after_seq };
}

export async function requestWorkspaceDeleteAbandon(binding: WorkspaceRegistration, operationId: string): Promise<void> {
  const operation_id = nodeId(operationId), renderer_session_id = await getRendererSession();
  await invoke("workspace_request_delete_abandon", { request: { renderer_session_id, binding, operation_id } });
}

function markdownName(value: unknown): string {
  const name = string(value);
  if (!name || new TextEncoder().encode(name).length > 255 || name.normalize("NFC") !== name || [...name].some((character) => {
    const code = character.codePointAt(0)!;
    return character === "/" || character === "\\" || code < 32 || (code >= 127 && code <= 159);
  }) || !/\.md$/i.test(name)) throw new Error("잘못된 문서 이름입니다.");
  return name;
}

function moveRequest(value: unknown): MoveNodeRequest {
  const item = object(value);
  const expected_parent_id = nodeId(item.expected_parent_id), parent_id = nodeId(item.parent_id), name = markdownName(item.name);
  return { expected_parent_id, parent_id, name };
}

function strongEtag(value: unknown): string {
  const etag = string(value);
  if (!/^"[\x21\x23-\x7e\u0080-\u{10ffff}]*"$/u.test(etag)) throw new Error("잘못된 문서 상태입니다.");
  return etag;
}

function decimalU64(value: string | null): boolean {
  if (value === null || !/^[0-9]+$/.test(value)) return false;
  const digits = value.replace(/^0+/, "") || "0";
  return digits.length < 20 || (digits.length === 20 && digits <= "18446744073709551615");
}

function changeSequence(value: unknown): string {
  const sequence = string(value);
  if (!/^(0|[1-9][0-9]*)$/.test(sequence) || !decimalU64(sequence)) throw new Error("잘못된 변경 순번입니다.");
  return sequence;
}

function moveDisplay(value: unknown, binding: WorkspaceRegistration): MoveDisplay {
  const item = object(value);
  const operation_id = nodeId(item.operation_id), operation_seq = generation(item.operation_seq), node_id = nodeId(item.node_id);
  const expected_meta_etag = nodeEtag(item.expected_meta_etag), request = moveRequest(item.request), status = string(item.status);
  if (expected_meta_etag === '""' || operation_seq === "0" || node_id === request.expected_parent_id || node_id === request.parent_id || !["queued", "sent", "abandoning", "abandoned", "committed", "failed"].includes(status)) throw new Error("잘못된 이동 상태입니다.");
  let reason: string | null = null;
  if (status === "queued" || status === "sent" || status === "abandoning") {
    if (item.receipt !== null) throw new Error("잘못된 이동 결과입니다.");
  } else {
    const receipt = object(item.receipt), receiptStatus = string(receipt.status);
    if (nodeId(receipt.operation_id) !== operation_id || generation(receipt.operation_seq) !== operation_seq || string(receipt.workspace_epoch) !== binding.workspace_epoch) throw new Error("다른 이동 결과입니다.");
    if (status === "abandoned") {
      if (receiptStatus !== "abandoned") throw new Error("잘못된 이동 취소 결과입니다.");
    } else if (status === "committed") {
      if (receiptStatus !== "moved") throw new Error("잘못된 이동 결과입니다.");
      const node = nodeStateWithSequence(receipt.node, changeSequence), commit_seq = changeSequence(receipt.commit_seq);
      if (node.node_id !== node_id || node.parent_id !== request.parent_id || node.name !== request.name || node.kind !== "file" || node.deleted || commit_seq === "0" || node.last_change_seq !== commit_seq || !node.meta_rev || strongEtag(node.meta_etag) !== `"${node.meta_rev}"` || (request.parent_id !== request.expected_parent_id && node.meta_etag === expected_meta_etag) || node.content_rev === null || !node.content_rev || node.content_etag === null || strongEtag(node.content_etag) !== `"${node.content_rev}"` || !/^sha256:[0-9a-f]{64}$/.test(node.content_hash ?? "") || !decimalU64(node.size) || node.children_generation !== "0") throw new Error("잘못된 이동 결과입니다.");
      strongEtag(node.node_etag);
    } else {
      if (!["recovery_conflict", "node_deleted", "metadata_precondition_failed", "expected_parent_mismatch", "destination_exists", "parent_not_folder", "not_found", "not_applied"].includes(receiptStatus)) throw new Error("잘못된 이동 오류입니다.");
      if (receiptStatus === "metadata_precondition_failed") {
        const revision = string(receipt.current_meta_rev), etag = strongEtag(receipt.current_meta_etag);
        if (!revision || etag !== `"${revision}"` || etag === expected_meta_etag) throw new Error("잘못된 이동 오류입니다.");
      }
      reason = receiptStatus;
    }
  }
  return { operation_id, operation_seq, node_id, expected_meta_etag, request, status: status as MoveDisplay["status"], reason };
}

export async function queueWorkspaceMove(binding: WorkspaceRegistration, input: { node_id: string; expected_meta_etag: string; request: MoveNodeRequest }): Promise<MoveDisplay> {
  const node_id = nodeId(input.node_id), expected_meta_etag = nodeEtag(input.expected_meta_etag), request = moveRequest(input.request);
  if (expected_meta_etag === '""' || request.expected_parent_id === node_id || request.parent_id === node_id) throw new Error("잘못된 이동 요청입니다.");
  const renderer_session_id = await getRendererSession();
  const item = moveDisplay(await invoke("workspace_queue_move", { request: { renderer_session_id, binding, node_id, expected_meta_etag, request } }), binding);
  if (item.status !== "queued" || item.node_id !== node_id || item.expected_meta_etag !== expected_meta_etag || item.request.expected_parent_id !== request.expected_parent_id || item.request.parent_id !== request.parent_id || item.request.name !== request.name) throw new Error("다른 이동 요청의 응답입니다.");
  return item;
}

export async function workspaceMoveStatus(binding: WorkspaceRegistration, after: string): Promise<{ entries: MoveDisplay[]; next_after_seq: string | null }> {
  generation(after);
  const renderer_session_id = await getRendererSession();
  const page = object(await invoke("workspace_move_status", { request: { renderer_session_id, binding, after_seq: after, limit: 100 } }));
  if (!sameRegistration(registration(page.binding), binding) || !Array.isArray(page.entries) || page.entries.length > 100) throw new Error("잘못된 이동 상태입니다.");
  let previous = BigInt(after);
  const entries = page.entries.map((raw: unknown) => {
    const item = moveDisplay(raw, binding), sequence = BigInt(item.operation_seq);
    if (sequence <= previous) throw new Error("잘못된 이동 순서입니다.");
    previous = sequence;
    return item;
  });
  const next_after_seq = page.next_after_seq === null ? null : generation(page.next_after_seq);
  if (next_after_seq !== null && (!entries.length || next_after_seq !== entries[entries.length - 1].operation_seq)) throw new Error("잘못된 이동 페이지입니다.");
  return { entries, next_after_seq };
}

export async function requestWorkspaceMoveAbandon(binding: WorkspaceRegistration, operationId: string): Promise<void> {
  const operation_id = nodeId(operationId), renderer_session_id = await getRendererSession();
  await invoke("workspace_request_move_abandon", { request: { renderer_session_id, binding, operation_id } });
}

export async function workspaceFrozenMoves(binding: WorkspaceRegistration, after_id: string | null): Promise<{ binding: WorkspaceRegistration; entries: FrozenMoveReviewItem[]; next_after_id: string | null }> {
  if (after_id !== null) nodeId(after_id);
  const renderer_session_id = await getRendererSession();
  const page = object(await invoke("workspace_frozen_moves", { request: { renderer_session_id, binding, after_id, limit: 100 } }));
  if (!sameRegistration(registration(page.binding), binding) || !Array.isArray(page.entries) || page.entries.length > 100) throw new Error("잘못된 이전 이동 목록입니다.");
  let previous = after_id;
  const entries = page.entries.map((raw: unknown): FrozenMoveReviewItem => {
    const value = object(raw), old_binding = registration(value.old_binding);
    const item = moveDisplay(value, old_binding);
    if ((previous !== null && item.operation_id <= previous) || old_binding.workspace_id !== binding.workspace_id || sameRegistration(old_binding, binding) || !["queued", "sent", "abandoning", "failed"].includes(item.status)) throw new Error("잘못된 이전 이동 요청입니다.");
    previous = item.operation_id;
    const current_node = value.current_node === null ? null : nodeState(value.current_node);
    const current_destination = value.current_destination === null ? null : nodeState(value.current_destination);
    if (current_node) {
      if (nodeId(current_node.node_id) !== item.node_id) throw new Error("잘못된 현재 문서입니다.");
      if (current_node.parent_id !== null) nodeId(current_node.parent_id);
      nodeEtag(current_node.meta_etag);
    }
    if (current_destination && nodeId(current_destination.node_id) !== item.request.parent_id) throw new Error("잘못된 현재 목적지입니다.");
    return { old_binding, ...item, status: item.status as FrozenMoveReviewItem["status"], current_node, current_destination };
  });
  const next_after_id = page.next_after_id === null ? null : nodeId(page.next_after_id);
  if (next_after_id !== null && (!entries.length || entries[entries.length - 1].operation_id !== next_after_id)) throw new Error("잘못된 이전 이동 페이지입니다.");
  return { binding, entries, next_after_id };
}

export async function resolveWorkspaceFrozenMove(binding: WorkspaceRegistration, old_binding: WorkspaceRegistration, operation_id: string, choice: FrozenMoveReviewChoice): Promise<void> {
  nodeId(operation_id);
  if (old_binding.workspace_id !== binding.workspace_id || sameRegistration(old_binding, binding)) throw new Error("다른 작업 공간의 요청입니다.");
  if (choice.kind === "apply") {
    if (nodeEtag(choice.expected_meta_etag) === '""') throw new Error("잘못된 문서 상태입니다.");
    moveRequest(choice.request);
  } else if (choice.kind !== "dismiss") throw new Error("잘못된 선택입니다.");
  const renderer_session_id = await getRendererSession();
  await invoke("workspace_resolve_frozen_move", { request: { renderer_session_id, binding, old_binding, operation_id, choice } });
}

export async function workspaceFrozenDeletes(binding: WorkspaceRegistration, after_id: string | null): Promise<{ binding: WorkspaceRegistration; entries: FrozenDeleteReviewItem[]; next_after_id: string | null }> {
  if (after_id !== null) nodeId(after_id);
  const renderer_session_id = await getRendererSession();
  const page = object(await invoke("workspace_frozen_deletes", { request: { renderer_session_id, binding, after_id, limit: 100 } }));
  if (!sameRegistration(registration(page.binding), binding) || !Array.isArray(page.entries) || page.entries.length > 100) throw new Error("잘못된 이전 삭제 목록입니다.");
  let previous = after_id;
  const entries = page.entries.map((raw: unknown): FrozenDeleteReviewItem => {
    const item = object(raw), old_binding = registration(item.old_binding);
    const operation_id = nodeId(item.operation_id), operation_seq = generation(item.operation_seq);
    const node_id = nodeId(item.node_id), expected_node_etag = nodeEtag(item.expected_node_etag);
    const status = string(item.status);
    if ((previous !== null && operation_id <= previous) || operation_seq === "0" || old_binding.workspace_id !== binding.workspace_id || sameRegistration(old_binding, binding) || !["queued", "sent", "abandoning", "failed"].includes(status)) throw new Error("잘못된 이전 삭제 요청입니다.");
    previous = operation_id;
    let reason: string | null = null;
    if (status === "failed") {
      const receipt = object(item.receipt), receiptStatus = string(receipt.status);
      if (nodeId(receipt.operation_id) !== operation_id || generation(receipt.operation_seq) !== operation_seq || string(receipt.workspace_epoch) !== old_binding.workspace_epoch) throw new Error("다른 이전 삭제 결과입니다.");
      if (!["recovery_conflict", "node_deleted", "node_precondition_failed", "not_found", "not_applied"].includes(receiptStatus)) throw new Error("잘못된 이전 삭제 결과입니다.");
      if (receiptStatus === "node_precondition_failed" && nodeEtag(receipt.current_node_etag) === expected_node_etag) throw new Error("잘못된 이전 삭제 결과입니다.");
      reason = receiptStatus;
    } else if (item.receipt !== null) throw new Error("잘못된 이전 삭제 결과입니다.");
    const current_node = item.current_node === null ? null : nodeState(item.current_node);
    if (current_node && (nodeId(current_node.node_id) !== node_id || current_node.kind !== "file")) throw new Error("잘못된 현재 문서입니다.");
    if (current_node) nodeEtag(current_node.node_etag);
    return { old_binding, operation_id, operation_seq, node_id, expected_node_etag, status: status as FrozenDeleteReviewItem["status"], reason, current_node };
  });
  const next_after_id = page.next_after_id === null ? null : nodeId(page.next_after_id);
  if (next_after_id !== null && (!entries.length || entries[entries.length - 1].operation_id !== next_after_id)) throw new Error("잘못된 이전 삭제 페이지입니다.");
  return { binding, entries, next_after_id };
}

export async function resolveWorkspaceFrozenDelete(binding: WorkspaceRegistration, old_binding: WorkspaceRegistration, operation_id: string, choice: FrozenDeleteReviewChoice): Promise<void> {
  nodeId(operation_id);
  if (old_binding.workspace_id !== binding.workspace_id || sameRegistration(old_binding, binding)) throw new Error("다른 작업 공간의 요청입니다.");
  if (choice.kind === "apply") nodeEtag(choice.expected_node_etag);
  else if (choice.kind !== "dismiss") throw new Error("잘못된 선택입니다.");
  const renderer_session_id = await getRendererSession();
  await invoke("workspace_resolve_frozen_delete", { request: { renderer_session_id, binding, old_binding, operation_id, choice } });
}

export async function workspaceFrozenCreates(binding: WorkspaceRegistration, after_id: string | null): Promise<{ binding: WorkspaceRegistration; entries: FrozenCreateReviewItem[]; next_after_id: string | null }> {
  if (after_id !== null) nodeId(after_id);
  const renderer_session_id = await getRendererSession();
  const page = object(await invoke("workspace_frozen_creates", { request: { renderer_session_id, binding, after_id, limit: 100 } }));
  if (!sameRegistration(registration(page.binding), binding) || !Array.isArray(page.entries) || page.entries.length > 100) throw new Error("잘못된 이전 생성 목록입니다.");
  let previous = after_id;
  const entries = page.entries.map((raw: unknown): FrozenCreateReviewItem => {
    const item = object(raw), old_binding = registration(item.old_binding), request = object(item.request);
    const operation_id = nodeId(item.operation_id), operation_seq = generation(item.operation_seq);
    const node_id = nodeId(request.node_id), parent_id = nodeId(request.parent_id), name = string(request.name);
    const status = item.status;
    if ((previous !== null && operation_id <= previous) || operation_seq === "0" || old_binding.workspace_id !== binding.workspace_id || sameRegistration(old_binding, binding) || node_id === parent_id || (request.kind !== "folder" && request.kind !== "file") || (request.upload_id !== undefined && request.upload_id !== null) || !name || new TextEncoder().encode(name).length > 255 || (status !== "queued" && status !== "sent" && status !== "abandoning" && status !== "failed") || !Array.isArray(item.current_same_name) || item.current_same_name.length > 100) throw new Error("잘못된 이전 생성 요청입니다.");
    previous = operation_id;
    let reason: string | null = null;
    if (status === "failed") {
      const receipt = object(item.receipt);
      const keys = Object.keys(receipt).sort();
      if (keys.join(",") !== "operation_id,operation_seq,status,workspace_epoch" || string(receipt.operation_id) !== operation_id || generation(receipt.operation_seq) !== operation_seq || string(receipt.workspace_epoch) !== old_binding.workspace_epoch) throw new Error("잘못된 이전 생성 결과입니다.");
      reason = string(receipt.status);
      if (!["recovery_conflict", "node_deleted", "destination_exists", "node_id_conflict", "parent_not_folder", "not_found", "not_applied"].includes(reason)) throw new Error("잘못된 이전 생성 결과입니다.");
    } else if (item.receipt !== null) throw new Error("잘못된 이전 생성 결과입니다.");
    function reviewNode(rawNode: unknown): NodeState {
      const node = nodeState(rawNode);
      if (node.kind === "folder" && (node.content_rev !== null || node.content_etag !== null || node.content_hash !== null || node.size !== null)) throw new Error("잘못된 현재 항목입니다.");
      if (node.kind === "file" && (node.content_rev === null || node.content_etag === null || node.content_hash === null || node.size === null || node.children_generation !== "0")) throw new Error("잘못된 현재 항목입니다.");
      return node;
    }
    const current_same_id = item.current_same_id === null ? null : reviewNode(item.current_same_id);
    if (current_same_id && nodeId(current_same_id.node_id) !== node_id) throw new Error("잘못된 현재 항목입니다.");
    const current_same_name = item.current_same_name.map((rawNode: unknown) => {
      const node = reviewNode(rawNode);
      if (nodeId(node.node_id) === node_id || node.parent_id !== parent_id || node.deleted) throw new Error("잘못된 현재 항목입니다.");
      return node;
    });
    return { old_binding, operation_id, operation_seq, node_id, parent_id, name, kind: request.kind as "file" | "folder", status, reason, current_same_id, current_same_name };
  });
  const next_after_id = page.next_after_id === null ? null : nodeId(page.next_after_id);
  if (next_after_id !== null && (!entries.length || entries[entries.length - 1].operation_id !== next_after_id)) throw new Error("잘못된 이전 생성 페이지입니다.");
  return { binding, entries, next_after_id };
}

export async function resolveWorkspaceFrozenCreate(binding: WorkspaceRegistration, old_binding: WorkspaceRegistration, operation_id: string, choice: FrozenCreateReviewChoice): Promise<void> {
  nodeId(operation_id);
  if (old_binding.workspace_id !== binding.workspace_id || sameRegistration(old_binding, binding)) throw new Error("다른 작업 공간의 요청입니다.");
  if (choice.kind === "apply") {
    nodeId(choice.parent_id);
    if (!choice.name || new TextEncoder().encode(choice.name).length > 255) throw new Error("잘못된 폴더 이름입니다.");
  } else if (choice.kind !== "dismiss") throw new Error("잘못된 선택입니다.");
  const renderer_session_id = await getRendererSession();
  await invoke("workspace_resolve_frozen_create", { request: { renderer_session_id, binding, old_binding, operation_id, choice } });
}

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

function conflictRevision(value: unknown, precondition: boolean): string {
  const revision = string(value);
  if (!revision || (precondition && new TextEncoder().encode(revision).length > 256) || !/^"[\x21\x23-\x7e\u0080-\u{10ffff}]*"$/u.test(`"${revision}"`)) throw new Error("잘못된 충돌 revision입니다.");
  return revision;
}

function conflictBytes(value: unknown): Uint8Array {
  const encoded = string(value);
  if (encoded.length > 4 * Math.ceil(5 * 1024 * 1024 / 3)) throw new Error("비교 원문이 너무 큽니다.");
  const bytes = fromBase64(encoded);
  if (bytes.length > 5 * 1024 * 1024) throw new Error("비교 원문이 너무 큽니다.");
  new TextDecoder("utf-8", { fatal:true }).decode(bytes);
  return bytes;
}

function conflictHash(value: unknown): string {
  const hash = string(value);
  if (!/^sha256:[0-9a-f]{64}$/.test(hash)) throw new Error("잘못된 충돌 원문 hash입니다.");
  return hash;
}

type FrozenContentInspection = {
  current_binding: WorkspaceRegistration; old_binding: WorkspaceRegistration; document_id: string; operation_id: string;
  operation_seq: string; operation_status: "queued" | "sent" | "abandoning"; abandon_state: "none" | "requested" | "sent"; operation_generation: string; local_generation: string;
  expected_revision: string | null; outcome_unknown: boolean; base_revision: string; remote_revision: string;
  operation_bytes: Uint8Array; base_bytes: Uint8Array; local_bytes: Uint8Array; remote_bytes: Uint8Array;
  inspected: WorkspaceFrozenContentInspection;
};

async function frozenContentBytes(encodedValue: unknown, hashValue: unknown, maximum: number): Promise<Uint8Array> {
  const encoded = string(encodedValue), hash = conflictHash(hashValue);
  if (encoded.length > 4 * Math.ceil(maximum / 3) || encoded.length % 4 !== 0 || !/^[A-Za-z0-9+/]*={0,2}$/.test(encoded)) throw new Error("잘못된 비교 원문입니다.");
  const bytes = fromBase64(encoded);
  if (bytes.byteLength > maximum) throw new Error("비교 원문이 너무 큽니다.");
  const digest = new Uint8Array(await crypto.subtle.digest("SHA-256", bytes as Uint8Array<ArrayBuffer>));
  const actual = `sha256:${Array.from(digest, (byte) => byte.toString(16).padStart(2, "0")).join("")}`;
  if (actual !== hash) throw new Error("비교 원문의 hash가 일치하지 않습니다.");
  return bytes;
}

export async function workspaceInspectFrozenContent(current: WorkspaceRegistration, old: WorkspaceRegistration, operationId: string, documentId: string): Promise<FrozenContentInspection> {
  const current_binding = registration(current), old_binding = registration(old), operation_id = nodeId(operationId), document_id = nodeId(documentId);
  if (Object.values(current_binding).some((value) => !value) || Object.values(old_binding).some((value) => !value) || sameRegistration(current_binding, old_binding) || current_binding.workspace_id !== old_binding.workspace_id) throw new Error("잘못된 등록입니다.");
  const renderer_session_id = await getRendererSession();
  const item = object(await invoke("workspace_inspect_frozen_content", { request: { renderer_session_id, current_binding, old_binding, operation_id } }));
  return parseFrozenContentInspection(item, current_binding, old_binding, operation_id, document_id);
}

async function parseFrozenContentInspection(item: Record<string, unknown>, current_binding: WorkspaceRegistration, old_binding: WorkspaceRegistration, operation_id: string, document_id: string): Promise<FrozenContentInspection> {
  const keys = ["current_binding", "old_binding", "document_id", "operation_id", "operation_seq", "operation_status", "abandon_state", "operation_generation", "local_generation", "expected_revision", "outcome_unknown", "base_revision", "base_bytes_base64", "base_hash", "operation_bytes_base64", "operation_hash", "local_bytes_base64", "local_hash", "remote_revision", "remote_bytes_base64", "remote_hash"];
  if (Object.keys(item).some((key) => !keys.includes(key)) || !sameRegistration(registration(item.current_binding), current_binding) || !sameRegistration(registration(item.old_binding), old_binding) || nodeId(item.document_id) !== document_id || nodeId(item.operation_id) !== operation_id) throw new Error("다른 본문 비교 결과입니다.");
  const operation_seq = generation(item.operation_seq), operation_generation = generation(item.operation_generation), local_generation = generation(item.local_generation);
  const base_revision = conflictRevision(item.base_revision, true), remote_revision = conflictRevision(item.remote_revision, false);
  const expected_revision = item.expected_revision === null ? null : conflictRevision(item.expected_revision, true);
  const operation_status = item.operation_status, abandon_state = item.abandon_state;
  if (abandon_state !== "none" && abandon_state !== "requested" && abandon_state !== "sent") throw new Error("잘못된 취소 상태입니다.");
  if (operation_seq === "0" || operation_generation === "0" || local_generation === "0" || BigInt(local_generation) < BigInt(operation_generation) || (operation_status !== "queued" && operation_status !== "sent" && operation_status !== "abandoning") || (operation_status === "abandoning" ? abandon_state !== "requested" && abandon_state !== "sent" : abandon_state !== "none") || item.outcome_unknown !== (operation_status === "sent") || (operation_status === "sent" && expected_revision !== base_revision)) throw new Error("잘못된 본문 비교 상태입니다.");
  const localLimit = 5 * 1024 * 1024;
  const operation_bytes = await frozenContentBytes(item.operation_bytes_base64, item.operation_hash, localLimit);
  const base_bytes = await frozenContentBytes(item.base_bytes_base64, item.base_hash, localLimit);
  const local_bytes = await frozenContentBytes(item.local_bytes_base64, item.local_hash, localLimit);
  const remote_bytes = await frozenContentBytes(item.remote_bytes_base64, item.remote_hash, 25 * 1024 * 1024);
  if (operation_generation === local_generation && (operation_bytes.length !== local_bytes.length || !operation_bytes.every((byte, index) => byte === local_bytes[index]))) throw new Error("비교 초안이 일치하지 않습니다.");
  const inspected: WorkspaceFrozenContentInspection = { current_binding, old_binding, document_id, operation_id, operation_seq, operation_status, abandon_state, operation_generation, local_generation, expected_revision, outcome_unknown: operation_status === "sent", base_revision,
    operation_bytes_base64: string(item.operation_bytes_base64), operation_hash: conflictHash(item.operation_hash),
    base_bytes_base64: string(item.base_bytes_base64), base_hash: conflictHash(item.base_hash),
    local_bytes_base64: string(item.local_bytes_base64), local_hash: conflictHash(item.local_hash),
    remote_revision, remote_bytes_base64: string(item.remote_bytes_base64), remote_hash: conflictHash(item.remote_hash) };
  return { current_binding, old_binding, document_id, operation_id, operation_seq, operation_status, abandon_state, operation_generation, local_generation, expected_revision, outcome_unknown: operation_status === "sent", base_revision, remote_revision, operation_bytes, base_bytes, local_bytes, remote_bytes, inspected };
}

function checkedFrozenContentSnapshot(snapshot: FrozenContentReviewSnapshot): WorkspaceFrozenContentInspection {
  const inspected = snapshot.inspected;
  if (!inspected || !sameRegistration(registration(inspected.current_binding), registration(snapshot.binding)) ||
    !sameRegistration(registration(inspected.old_binding), registration(snapshot.old_binding)) ||
    inspected.document_id !== snapshot.document_id || inspected.operation_id !== snapshot.operation_id ||
    inspected.operation_seq !== snapshot.operation_seq || inspected.operation_generation !== snapshot.generation ||
    inspected.local_generation !== snapshot.local_generation || inspected.operation_status !== snapshot.status ||
    inspected.abandon_state !== snapshot.abandon_state || inspected.expected_revision !== snapshot.expected_revision ||
    inspected.outcome_unknown !== snapshot.outcome_unknown || inspected.base_revision !== snapshot.base_revision ||
    inspected.remote_revision !== snapshot.remote_revision ||
    inspected.operation_bytes_base64 !== toBase64(snapshot.operation_bytes) || inspected.base_bytes_base64 !== toBase64(snapshot.base_bytes) ||
    inspected.local_bytes_base64 !== toBase64(snapshot.local_bytes) || inspected.remote_bytes_base64 !== toBase64(snapshot.remote_bytes)) {
    throw new Error("비교한 원문이 변경됐습니다. 다시 대조해 주세요.");
  }
  return inspected;
}

export async function workspaceApplyFrozenContent(snapshot: FrozenContentReviewSnapshot): Promise<{ binding: WorkspaceRegistration; operation_id: string }> {
  const inspected = checkedFrozenContentSnapshot(snapshot);
  const renderer_session_id = await getRendererSession();
  const value = object(await invoke("workspace_apply_frozen_content", { request: { renderer_session_id, inspected, choice: "latest_draft" } }));
  if (Object.keys(value).some((key) => !["operation_id", "operation_seq"].includes(key))) throw new Error("잘못된 전송 작업입니다.");
  const operation_id = nodeId(value.operation_id), operation_seq = generation(value.operation_seq);
  if (operation_seq === "0") throw new Error("잘못된 전송 순서입니다.");
  return { binding: snapshot.binding, operation_id };
}

function frozenContentCurrentTarget(value: unknown, documentId: string) {
  const target = object(value);
  if (Object.keys(target).some((key) => !["node_id", "parent_id", "name", "path"].includes(key))) throw new Error("잘못된 적용 대상입니다.");
  const node_id = nodeId(target.node_id), parent_id = nodeId(target.parent_id), name = markdownName(target.name), path = string(target.path);
  const parts = path.slice(1).split("/");
  if (node_id !== documentId || node_id === parent_id || !path.startsWith("/") || path.includes("\\") || parts.some((part) => !part || part === "." || part === ".." || [...part].some((character) => { const code = character.codePointAt(0)!; return code < 32 || (code >= 127 && code <= 159); })) || parts.at(-1) !== name) throw new Error("잘못된 적용 대상입니다.");
  return { node_id, parent_id, name, path };
}

export async function workspaceInspectFrozenContentCrossEpoch(current: WorkspaceRegistration, old: WorkspaceRegistration, operationId: string, documentId: string): Promise<FrozenContentReviewSnapshot> {
  const current_binding = registration(current), old_binding = registration(old), operation_id = nodeId(operationId), document_id = nodeId(documentId);
  if (Object.values(current_binding).some((value) => !value) || Object.values(old_binding).some((value) => !value) || current_binding.workspace_id !== old_binding.workspace_id || current_binding.workspace_epoch === old_binding.workspace_epoch) throw new Error("잘못된 등록입니다.");
  const renderer_session_id = await getRendererSession();
  const value = object(await invoke("workspace_inspect_frozen_content_cross_epoch", { request: { renderer_session_id, current_binding, old_binding, operation_id } }));
  if (Object.keys(value).some((key) => !["inspected", "current_target"].includes(key))) throw new Error("잘못된 본문 비교 결과입니다.");
  const result = await parseFrozenContentInspection(object(value.inspected), current_binding, old_binding, operation_id, document_id);
  const current_target = frozenContentCurrentTarget(value.current_target, document_id);
  return { binding: current_binding, old_binding, document_id, operation_id, operation_seq: result.operation_seq, generation: result.operation_generation,
    local_generation: result.local_generation, status: result.operation_status, abandon_state: result.abandon_state, expected_revision: result.expected_revision,
    outcome_unknown: result.outcome_unknown, base_revision: result.base_revision, remote_revision: result.remote_revision,
    operation_bytes: result.operation_bytes, base_bytes: result.base_bytes, local_bytes: result.local_bytes, remote_bytes: result.remote_bytes,
    inspected: result.inspected, current_target, target_key: JSON.stringify(current_target), cross_epoch_inspected: { inspected: result.inspected, current_target } };
}

export async function workspaceApplyFrozenContentCrossEpoch(snapshot: FrozenContentReviewSnapshot, confirmed: true): Promise<{ binding: WorkspaceRegistration; operation_id: string }> {
  if (confirmed !== true || snapshot.binding.workspace_id !== snapshot.old_binding.workspace_id || snapshot.binding.workspace_epoch === snapshot.old_binding.workspace_epoch) throw new Error("현재 적용 대상을 다시 확인해 주세요.");
  const inspected = checkedFrozenContentSnapshot(snapshot), wire = object(snapshot.cross_epoch_inspected);
  if (Object.keys(wire).some((key) => !["inspected", "current_target"].includes(key))) throw new Error("잘못된 본문 비교 결과입니다.");
  const target = frozenContentCurrentTarget(snapshot.current_target, snapshot.document_id), originalTarget = frozenContentCurrentTarget(wire.current_target, snapshot.document_id);
  if (snapshot.target_key !== JSON.stringify(target) || JSON.stringify(originalTarget) !== JSON.stringify(target) || JSON.stringify(wire.inspected) !== JSON.stringify(inspected)) throw new Error("비교한 적용 대상이 변경됐습니다. 다시 확인해 주세요.");
  const renderer_session_id = await getRendererSession();
  const value = object(await invoke("workspace_apply_frozen_content_cross_epoch", { request: { renderer_session_id, inspected: { inspected, current_target: target }, same_node_is_not_identity: true } }));
  if (Object.keys(value).some((key) => !["operation_id", "operation_seq"].includes(key))) throw new Error("잘못된 전송 작업입니다.");
  const operation_id = nodeId(value.operation_id), operation_seq = generation(value.operation_seq);
  if (operation_seq === "0") throw new Error("잘못된 전송 순서입니다.");
  return { binding: snapshot.binding, operation_id };
}

export async function workspaceDismissFrozenContent(snapshot: FrozenContentReviewSnapshot): Promise<{ binding: WorkspaceRegistration; old_operation_id: string }> {
  const inspected = checkedFrozenContentSnapshot(snapshot);
  const renderer_session_id = await getRendererSession();
  const value = object(await invoke("workspace_dismiss_frozen_content", { request: { renderer_session_id, inspected } }));
  if (Object.keys(value).some((key) => !["operation_id", "operation_seq", "operation_status", "abandon_state"].includes(key)) ||
    nodeId(value.operation_id) !== snapshot.operation_id || generation(value.operation_seq) !== snapshot.operation_seq ||
    value.operation_status !== snapshot.status || value.abandon_state !== snapshot.abandon_state) throw new Error("잘못된 재발급 포기 결과입니다.");
  return { binding: snapshot.binding, old_operation_id: snapshot.operation_id };
}

export async function workspaceReviewFrozenContentLocal(current: WorkspaceRegistration, old: WorkspaceRegistration, operationId: string, documentId: string): Promise<FrozenContentLocalReviewSnapshot> {
  const current_binding = registration(current), old_binding = registration(old), operation_id = nodeId(operationId), document_id = nodeId(documentId);
  if (Object.values(current_binding).some((value) => !value) || Object.values(old_binding).some((value) => !value) || sameRegistration(current_binding, old_binding) || current_binding.workspace_id !== old_binding.workspace_id) throw new Error("잘못된 등록입니다.");
  const renderer_session_id = await getRendererSession();
  const item = object(await invoke("workspace_review_frozen_content_local", { request: { renderer_session_id, current_binding, old_binding, document_id, operation_id } }));
  const keys = ["current_binding", "old_binding", "document_id", "operation_id", "operation_seq", "operation_status", "abandon_state", "dependency", "operation_generation", "local_generation", "expected_revision", "outcome_unknown", "base_revision", "base_bytes_base64", "base_hash", "operation_bytes_base64", "operation_hash", "local_bytes_base64", "local_hash", "remote_evidence", "remote_index_error"];
  if (Object.keys(item).some((key) => !keys.includes(key)) || !sameRegistration(registration(item.current_binding), current_binding) || !sameRegistration(registration(item.old_binding), old_binding) || nodeId(item.document_id) !== document_id || nodeId(item.operation_id) !== operation_id) throw new Error("다른 본문 비교 결과입니다.");
  const operation_seq = generation(item.operation_seq), operation_generation = generation(item.operation_generation), local_generation = generation(item.local_generation);
  const base_revision = conflictRevision(item.base_revision, true), expected_revision = item.expected_revision === null ? null : conflictRevision(item.expected_revision, true);
  const dependency = item.dependency === null ? null : nodeId(item.dependency);
  const operation_status = item.operation_status, abandon_state = item.abandon_state;
  if (abandon_state !== "none" && abandon_state !== "requested" && abandon_state !== "sent") throw new Error("잘못된 취소 상태입니다.");
  if (operation_seq === "0" || operation_generation === "0" || local_generation === "0" || BigInt(local_generation) < BigInt(operation_generation) || (operation_status !== "queued" && operation_status !== "sent" && operation_status !== "abandoning") || (operation_status === "abandoning" ? abandon_state !== "requested" && abandon_state !== "sent" : abandon_state !== "none") || item.outcome_unknown !== (operation_status === "sent") || (operation_status === "sent" && expected_revision !== base_revision)) throw new Error("잘못된 본문 비교 상태입니다.");
  const evidence = object(item.remote_evidence), remote_status = evidence.kind;
  if (remote_status !== "deleted" && remote_status !== "too_large" && remote_status !== "unknown") throw new Error("잘못된 원격 상태입니다.");
  if (typeof item.remote_index_error !== "boolean" || (item.remote_index_error && remote_status !== "unknown")) throw new Error("잘못된 동기화 상태입니다.");
  const evidenceKeys = remote_status === "unknown" ? ["kind"] : remote_status === "deleted" ? ["kind", "change_seq"] : ["kind", "change_seq", "size"];
  if (Object.keys(evidence).some((key) => !evidenceKeys.includes(key))) throw new Error("잘못된 원격 상태입니다.");
  if (remote_status !== "unknown" && generation(evidence.change_seq) === "0") throw new Error("잘못된 원격 변경 순서입니다.");
  if (remote_status === "too_large" && BigInt(generation(evidence.size)) <= BigInt(25 * 1024 * 1024)) throw new Error("잘못된 원격 크기입니다.");
  const operation_bytes = await frozenContentBytes(item.operation_bytes_base64, item.operation_hash, 5 * 1024 * 1024);
  const base_bytes = await frozenContentBytes(item.base_bytes_base64, item.base_hash, 5 * 1024 * 1024);
  const local_bytes = await frozenContentBytes(item.local_bytes_base64, item.local_hash, 5 * 1024 * 1024);
  if (operation_generation === local_generation && (operation_bytes.length !== local_bytes.length || !operation_bytes.every((byte, index) => byte === local_bytes[index]))) throw new Error("비교 초안이 일치하지 않습니다.");
  return { binding: current_binding, old_binding, document_id, operation_id, operation_seq, generation: operation_generation, local_generation, status: operation_status, abandon_state, dependency, expected_revision, outcome_unknown: operation_status === "sent", base_revision, operation_bytes, base_bytes, local_bytes, remote_status, remote_index_error: item.remote_index_error, evidence_key: JSON.stringify(evidence), inspected: item };
}

export async function workspaceDismissFrozenContentLocal(snapshot: FrozenContentLocalReviewSnapshot): Promise<{ binding: WorkspaceRegistration; old_operation_id: string }> {
  const inspected = object(snapshot.inspected), evidence = object(inspected.remote_evidence);
  if (snapshot.remote_index_error || inspected.remote_index_error !== snapshot.remote_index_error || !sameRegistration(registration(inspected.current_binding), registration(snapshot.binding)) || !sameRegistration(registration(inspected.old_binding), registration(snapshot.old_binding)) || inspected.document_id !== snapshot.document_id || inspected.operation_id !== snapshot.operation_id || inspected.operation_seq !== snapshot.operation_seq || inspected.operation_generation !== snapshot.generation || inspected.local_generation !== snapshot.local_generation || inspected.operation_status !== snapshot.status || inspected.abandon_state !== snapshot.abandon_state || inspected.dependency !== snapshot.dependency || inspected.expected_revision !== snapshot.expected_revision || inspected.outcome_unknown !== snapshot.outcome_unknown || inspected.base_revision !== snapshot.base_revision || evidence.kind !== snapshot.remote_status || snapshot.evidence_key !== JSON.stringify(evidence) || inspected.operation_bytes_base64 !== toBase64(snapshot.operation_bytes) || inspected.base_bytes_base64 !== toBase64(snapshot.base_bytes) || inspected.local_bytes_base64 !== toBase64(snapshot.local_bytes)) throw new Error("비교한 원문이 변경됐습니다. 다시 확인해 주세요.");
  const renderer_session_id = await getRendererSession();
  const value = object(await invoke("workspace_dismiss_frozen_content_local", { request: { renderer_session_id, inspected } }));
  if (Object.keys(value).some((key) => !["operation_id", "operation_seq", "operation_status", "abandon_state"].includes(key)) || nodeId(value.operation_id) !== snapshot.operation_id || generation(value.operation_seq) !== snapshot.operation_seq || value.operation_status !== snapshot.status || value.abandon_state !== snapshot.abandon_state) throw new Error("잘못된 재발급 포기 결과입니다.");
  return { binding: snapshot.binding, old_operation_id: snapshot.operation_id };
}

export async function workspaceFrozenContentDecisionStatus(current: WorkspaceRegistration, old: WorkspaceRegistration, documentId: string, oldOperationId: string): Promise<FrozenContentRecordedDecision | null> {
  const binding = registration(current), old_binding = registration(old), document_id = nodeId(documentId), old_operation_id = nodeId(oldOperationId);
  if (Object.values(binding).some((value) => !value) || Object.values(old_binding).some((value) => !value) ||
    sameRegistration(binding, old_binding) || binding.workspace_id !== old_binding.workspace_id) throw new Error("잘못된 등록입니다.");
  const renderer_session_id = await getRendererSession();
  const response = object(await invoke("workspace_frozen_content_decision_status", { request: { renderer_session_id, binding, old_binding, document_id, old_operation_id } }));
  if (Object.keys(response).some((key) => !["binding", "old_binding", "document_id", "old_operation_id", "decision", "dismissed"].includes(key)) ||
    !sameRegistration(registration(response.binding), binding) || !sameRegistration(registration(response.old_binding), old_binding) ||
    nodeId(response.document_id) !== document_id || nodeId(response.old_operation_id) !== old_operation_id) throw new Error("다른 복구 결정입니다.");
  if (response.decision === null && response.dismissed === null) return null;
  if (response.dismissed !== null) {
    if (response.decision !== null) throw new Error("중복된 복구 결정입니다.");
    const dismissed = object(response.dismissed);
    if (Object.keys(dismissed).some((key) => !["operation_id", "operation_seq", "operation_status", "abandon_state"].includes(key))) throw new Error("잘못된 재발급 포기 결과입니다.");
    const operation_seq = generation(dismissed.operation_seq), old_status = dismissed.operation_status, abandon_state = dismissed.abandon_state;
    if (abandon_state !== "none" && abandon_state !== "requested" && abandon_state !== "sent") throw new Error("잘못된 취소 상태입니다.");
    if (nodeId(dismissed.operation_id) !== old_operation_id || operation_seq === "0" || (old_status !== "queued" && old_status !== "sent" && old_status !== "abandoning") ||
      (old_status === "abandoning" ? abandon_state !== "requested" && abandon_state !== "sent" : abandon_state !== "none")) throw new Error("잘못된 재발급 포기 상태입니다.");
    return { kind: "dismissed", binding, old_binding, document_id, old_operation_id, operation_seq, old_status, abandon_state };
  }
  const decision = object(response.decision);
  if (Object.keys(decision).some((key) => !["operation_id", "operation_seq", "status", "approved_generation", "approved_hash"].includes(key))) throw new Error("잘못된 복구 결정입니다.");
  const operation_id = nodeId(decision.operation_id), operation_seq = generation(decision.operation_seq), approved_generation = generation(decision.approved_generation), approved_hash = conflictHash(decision.approved_hash);
  const status = decision.status;
  if (operation_id === old_operation_id || operation_seq === "0" || approved_generation === "0" ||
    (status !== "queued" && status !== "sent" && status !== "committed" && status !== "conflict" && status !== "abandoning" && status !== "abandoned" && status !== "failed")) throw new Error("잘못된 복구 결정입니다.");
  return { binding, old_binding, document_id, old_operation_id, operation_id, operation_seq, status, approved_generation, approved_hash };
}

export async function workspaceContentConflictCopyStatus(binding: WorkspaceRegistration, documentId: string, operationId: string): Promise<WorkspaceContentConflictCopyStatusResponse> {
  const expectedBinding = registration(binding), document_id = nodeId(documentId), operation_id = nodeId(operationId);
  const renderer_session_id = await getRendererSession();
  const result = object(await invoke("workspace_content_conflict_copy_status", { request: { renderer_session_id, binding: expectedBinding, document_id, operation_id } }));
  const responseBinding = registration(result.binding);
  if (Object.keys(result).some((key) => !["binding", "document_id", "operation_id", "copy"].includes(key)) ||
    !sameRegistration(responseBinding, expectedBinding) || nodeId(result.document_id) !== document_id || nodeId(result.operation_id) !== operation_id) throw new Error("다른 충돌 사본 상태입니다.");
  if (result.copy === null) return { binding: responseBinding, document_id, operation_id, copy: null };
  const raw = object(result.copy);
  if (Object.keys(raw).some((key) => !["version", "parent_id", "name", "node_id", "create_operation_id", "create_status", "create_failure", "content_status", "followup_operation_id"].includes(key))) throw new Error("잘못된 충돌 사본 상태입니다.");
  const version = generation(raw.version), parent_id = nodeId(raw.parent_id), name = markdownName(raw.name);
  const node_id = nodeId(raw.node_id), create_operation_id = nodeId(raw.create_operation_id);
  const followup_operation_id = raw.followup_operation_id === null ? null : nodeId(raw.followup_operation_id);
  const create_status = raw.create_status, create_failure = raw.create_failure, content_status = raw.content_status;
  if (node_id === document_id || node_id === create_operation_id || create_operation_id === operation_id || followup_operation_id === operation_id ||
    (create_status !== "not_queued" && create_status !== "pending" && create_status !== "created" && create_status !== "needs_review") ||
    (create_failure !== null && create_failure !== "destination_exists" && create_failure !== "other") ||
    (create_status === "needs_review" ? create_failure === null : create_failure !== null) ||
    (content_status !== "not_queued" && content_status !== "pending" && content_status !== "saved" && content_status !== "needs_review")) throw new Error("잘못된 충돌 사본 상태입니다.");
  return { binding: responseBinding, document_id, operation_id, copy: { version, parent_id, name, node_id, create_operation_id, create_status, create_failure, content_status, followup_operation_id } };
}

export async function progressWorkspaceContentConflictCopy(binding: WorkspaceRegistration, documentId: string, sessionId: string, operationId: string, version: string, approvedFollowupOperationId: string | null): Promise<WorkspaceContentConflictCopyProgressResponse> {
  const expectedBinding = registration(binding), document_id = nodeId(documentId), operation_id = nodeId(operationId);
  const edit_session_id = string(sessionId), expected_intent_version = generation(version);
  const approved_followup_operation_id = approvedFollowupOperationId === null ? null : nodeId(approvedFollowupOperationId);
  if (!edit_session_id || approved_followup_operation_id === operation_id) throw new Error("잘못된 충돌 사본 요청입니다.");
  const renderer_session_id = await getRendererSession();
  const result = object(await invoke("workspace_progress_content_conflict_copy", { request: { progress: { renderer_session_id, binding: expectedBinding, document_id, edit_session_id, operation_id, expected_intent_version }, approved_followup_operation_id } }));
  if (result.status === "approval_required" && Object.keys(result).every((key) => ["status", "followup_operation_id"].includes(key))) {
    const followup_operation_id = nodeId(result.followup_operation_id);
    if (followup_operation_id === operation_id) throw new Error("잘못된 충돌 사본 진행 결과입니다.");
    return { status: "approval_required", followup_operation_id };
  }
  if (result.status === "pending" && Object.keys(result).every((key) => ["status", "requested_operation_id"].includes(key))) {
    const requested_operation_id = result.requested_operation_id === null ? null : nodeId(result.requested_operation_id);
    if (requested_operation_id !== null && (requested_operation_id === operation_id || requested_operation_id !== approved_followup_operation_id)) throw new Error("잘못된 충돌 사본 진행 결과입니다.");
    return { status: "pending", requested_operation_id };
  }
  if (result.status === "saved" && Object.keys(result).length === 1) return { status: "saved" };
  if (result.status === "needs_review" && Object.keys(result).length === 1) return { status: "needs_review" };
  throw new Error("잘못된 충돌 사본 진행 결과입니다.");
}

export type DeletedDraftCopySnapshot = WorkspaceDeletedDraftCopySnapshot;
export async function reopenWorkspaceDeletedDraftCopy(binding: WorkspaceRegistration, documentId: string, previousSession: string): Promise<WorkspaceDeletedDraftCopyReopenResponse> {
  const expectedBinding = registration(binding), document_id = nodeId(documentId), previous_edit_session_id = string(previousSession);
  if (!previous_edit_session_id) throw new Error("잘못된 편집 세션입니다.");
  const renderer_session_id = await getRendererSession();
  const result = object(await invoke("workspace_reopen_deleted_draft_copy", { request: { renderer_session_id, binding: expectedBinding, document_id, previous_edit_session_id } }));
  if (Object.keys(result).some((key) => !["state", "document"].includes(key))) throw new Error("잘못된 삭제 사본 복구 결과입니다.");
  const state = deletedDraftCopyStatus(result.state, expectedBinding, document_id);
  const document = openedWorkspace(result.document, document_id);
  if (!sameRegistration(document.binding, expectedBinding) || !document.edit_session_id || document.edit_session_id === previous_edit_session_id || document.readonly || !document.local_draft) throw new Error("다른 삭제 초안 복구 문서입니다.");
  return { state, document };
}

export async function workspaceDeletedDraftCopyStatus(binding: WorkspaceRegistration, documentId: string): Promise<WorkspaceDeletedDraftCopyStatus | null> {
  const expectedBinding = registration(binding), document_id = nodeId(documentId);
  const renderer_session_id = await getRendererSession();
  const raw = await invoke("workspace_deleted_draft_copy_status", { request: { renderer_session_id, binding: expectedBinding, document_id } });
  return deletedDraftCopyStatus(raw, expectedBinding, document_id);
}

function deletedDraftCopyStatus(raw: unknown, expectedBinding: WorkspaceRegistration, document_id: string): WorkspaceDeletedDraftCopyStatus | null {
  if (raw === null) return null;
  const value = object(raw);
  if (Object.keys(value).some((key) => !["binding", "document_id", "generation", "local_hash", "tombstone_seq", "terminal_operation_id", "parent_id", "name", "node_id", "create_operation_id", "create_status", "create_failure", "content_status", "content_operation_id", "blocking_followup"].includes(key))) throw new Error("잘못된 삭제 사본 상태입니다.");
  const responseBinding = registration(value.binding), responseDocument = nodeId(value.document_id);
  const durableGeneration = generation(value.generation), local_hash = conflictHash(value.local_hash);
  const tombstone_seq = value.tombstone_seq === null ? null : changeSequence(value.tombstone_seq);
  const terminal_operation_id = value.terminal_operation_id === null ? null : nodeId(value.terminal_operation_id);
  const parent_id = nodeId(value.parent_id), name = markdownName(value.name), node_id = nodeId(value.node_id), create_operation_id = nodeId(value.create_operation_id);
  const create_status = value.create_status, create_failure = value.create_failure, content_status = value.content_status;
  const content_operation_id = value.content_operation_id === null ? null : nodeId(value.content_operation_id);
  let blocking_followup: WorkspaceDeletedDraftCopyStatus["blocking_followup"] = null;
  if (value.blocking_followup !== null) {
    const block = object(value.blocking_followup);
    if (Object.keys(block).some((key) => !["blocker_operation_id", "followup_operation_id", "followup_operation_seq", "followup_generation", "followup_hash"].includes(key))) throw new Error("잘못된 삭제 사본 차단 상태입니다.");
    const blocker_operation_id = nodeId(block.blocker_operation_id), followup_operation_id = nodeId(block.followup_operation_id), followup_operation_seq = generation(block.followup_operation_seq);
    const followup_generation = generation(block.followup_generation), followup_hash = conflictHash(block.followup_hash);
    if (blocker_operation_id === followup_operation_id || followup_operation_seq === "0" || followup_generation === "0") throw new Error("잘못된 삭제 사본 차단 상태입니다.");
    blocking_followup = { blocker_operation_id, followup_operation_id, followup_operation_seq, followup_generation, followup_hash };
  }
  if (!sameRegistration(responseBinding, expectedBinding) || responseDocument !== document_id || durableGeneration === "0" ||
    (tombstone_seq === null) === (terminal_operation_id === null) || tombstone_seq === "0" || node_id === document_id || node_id === parent_id ||
    (create_status !== "not_queued" && create_status !== "pending" && create_status !== "created" && create_status !== "needs_review") ||
    (create_failure !== null && create_failure !== "destination_exists" && create_failure !== "other") ||
    (create_status === "needs_review" ? create_failure === null : create_failure !== null) ||
    (content_status !== "not_queued" && content_status !== "pending" && content_status !== "saved" && content_status !== "needs_review") ||
    (content_status === "not_queued" ? content_operation_id !== null : content_operation_id === null)) throw new Error("잘못된 삭제 사본 상태입니다.");
  return { binding: responseBinding, document_id, generation: durableGeneration, local_hash, tombstone_seq, terminal_operation_id, parent_id, name, node_id, create_operation_id, create_status, create_failure, content_status, content_operation_id, blocking_followup };
}

export async function requestWorkspaceDeletedDraftCopyFollowupAbandon(binding: WorkspaceRegistration, documentId: string, targetNodeId: string, createOperationId: string, blockerOperationId: string, followupOperationId: string, followupOperationSeq: string, followupGeneration: string, followupHash: string): Promise<WorkspaceDeletedDraftCopyFollowupAbandonResult> {
  const expectedBinding = registration(binding), document_id = nodeId(documentId), node_id = nodeId(targetNodeId), create_operation_id = nodeId(createOperationId);
  const blocker_operation_id = nodeId(blockerOperationId), followup_operation_id = nodeId(followupOperationId);
  const followup_operation_seq = generation(followupOperationSeq), followup_generation = generation(followupGeneration), followup_hash = conflictHash(followupHash);
  if (node_id === document_id || blocker_operation_id === followup_operation_id || followup_operation_seq === "0" || followup_generation === "0") throw new Error("잘못된 삭제 사본 후속 요청입니다.");
  const renderer_session_id = await getRendererSession();
  const request: WorkspaceDeletedDraftCopyFollowupAbandonRequest = { renderer_session_id, binding: expectedBinding, document_id, node_id, create_operation_id, blocker_operation_id, followup_operation_id, followup_operation_seq, followup_generation, followup_hash };
  const result = object(await invoke("workspace_request_deleted_draft_copy_followup_abandon", { request }));
  if (Object.keys(result).some((key) => key !== "operation_id") || nodeId(result.operation_id) !== followup_operation_id) throw new Error("다른 삭제 사본 후속 요청의 결과입니다.");
  return { operation_id: followup_operation_id };
}

export async function progressWorkspaceDeletedDraftCopy(binding: WorkspaceRegistration, documentId: string, targetNodeId: string, createOperationId: string): Promise<WorkspaceDeletedDraftCopyProgressResult> {
  const expectedBinding = registration(binding), document_id = nodeId(documentId), node_id = nodeId(targetNodeId), create_operation_id = nodeId(createOperationId);
  if (node_id === document_id) throw new Error("잘못된 삭제 사본 요청입니다.");
  const renderer_session_id = await getRendererSession();
  const result = object(await invoke("workspace_progress_deleted_draft_copy", { request: { renderer_session_id, binding: expectedBinding, document_id, node_id, create_operation_id } }));
  if (Object.keys(result).some((key) => key !== "operation_id")) throw new Error("잘못된 삭제 사본 진행 결과입니다.");
  return { operation_id: nodeId(result.operation_id) };
}

function deletedDraftCopySnapshot(input: unknown): DeletedDraftCopySnapshot {
  const value = object(input);
  if (Object.keys(value).some((key) => !["binding", "document_id", "edit_session_id", "generation", "local_hash", "tombstone_seq"].includes(key))) throw new Error("잘못된 삭제 초안 확인 자료입니다.");
  const binding = registration(value.binding), document_id = nodeId(value.document_id), edit_session_id = string(value.edit_session_id);
  const durableGeneration = generation(value.generation), local_hash = conflictHash(value.local_hash), tombstone_seq = changeSequence(value.tombstone_seq);
  if (!edit_session_id || durableGeneration === "0" || tombstone_seq === "0") throw new Error("잘못된 삭제 초안 확인 자료입니다.");
  return { binding, document_id, edit_session_id, generation: durableGeneration, local_hash, tombstone_seq };
}

export async function reconfirmWorkspaceDeletedDraftCopy(binding: WorkspaceRegistration, documentId: string, expectedCreateOperationId: string, parentId: string, name: string): Promise<WorkspaceDeletedDraftCopyConfirmResult> {
  const expectedBinding = registration(binding), document_id = nodeId(documentId), expected_create_operation_id = nodeId(expectedCreateOperationId);
  const parent_id = nodeId(parentId), copyName = markdownName(name);
  const renderer_session_id = await getRendererSession();
  const request: WorkspaceDeletedDraftCopyReconfirmRequest = { renderer_session_id, binding: expectedBinding, document_id, expected_create_operation_id, parent_id, name: copyName };
  const result = object(await invoke("workspace_reconfirm_deleted_draft_copy", { request }));
  if (result.status !== "pending" || Object.keys(result).some((key) => !["status", "node_id", "create_operation_id"].includes(key))) throw new Error("잘못된 삭제 사본 재승인 결과입니다.");
  const node_id = nodeId(result.node_id), create_operation_id = nodeId(result.create_operation_id);
  if (node_id === document_id || node_id === create_operation_id || create_operation_id === expected_create_operation_id) throw new Error("다른 삭제 사본 재승인 결과입니다.");
  return { status: "pending", node_id, create_operation_id };
}

export async function prepareWorkspaceDeletedDraftCopy(binding: WorkspaceRegistration, documentId: string, sessionId: string, generationValue: string): Promise<DeletedDraftCopySnapshot> {
  const expectedBinding = registration(binding), document_id = nodeId(documentId), edit_session_id = string(sessionId), durableGeneration = generation(generationValue);
  if (!edit_session_id || durableGeneration === "0") throw new Error("잘못된 삭제 초안 요청입니다.");
  const renderer_session_id = await getRendererSession();
  const snapshot = deletedDraftCopySnapshot(await invoke("workspace_prepare_deleted_draft_copy", { request: { renderer_session_id, binding: expectedBinding, document_id, edit_session_id, generation: durableGeneration } }));
  if (!sameRegistration(snapshot.binding, expectedBinding) || snapshot.document_id !== document_id || snapshot.edit_session_id !== edit_session_id || snapshot.generation !== durableGeneration) throw new Error("다른 삭제 초안 확인 자료입니다.");
  return snapshot;
}

export async function confirmWorkspaceDeletedDraftCopy(snapshot: DeletedDraftCopySnapshot, parentId: string, name: string): Promise<WorkspaceDeletedDraftCopyConfirmResult> {
  const selected = deletedDraftCopySnapshot(snapshot), parent_id = nodeId(parentId), copyName = markdownName(name);
  const renderer_session_id = await getRendererSession();
  const result = object(await invoke("workspace_confirm_deleted_draft_copy", { request: { renderer_session_id, ...selected, parent_id, name: copyName } }));
  if (result.status !== "pending" || Object.keys(result).some((key) => !["status", "node_id", "create_operation_id"].includes(key))) throw new Error("잘못된 삭제 초안 사본 결과입니다.");
  const node_id = nodeId(result.node_id), create_operation_id = nodeId(result.create_operation_id);
  if (node_id === selected.document_id || node_id === create_operation_id) throw new Error("다른 삭제 초안 사본 결과입니다.");
  return { status: "pending", node_id, create_operation_id };
}

export async function retryWorkspaceContentConflictCopy(binding: WorkspaceRegistration, documentId: string, sessionId: string, operationId: string, version: string, expectedCreateOperationId: string, parentId: string, name: string): Promise<WorkspaceContentConflictCopyResponse> {
  const expectedBinding = registration(binding), document_id = nodeId(documentId), operation_id = nodeId(operationId);
  const edit_session_id = string(sessionId), expected_intent_version = generation(version);
  const expected_create_operation_id = nodeId(expectedCreateOperationId), parent_id = nodeId(parentId), copyName = markdownName(name);
  if (!edit_session_id || expected_create_operation_id === operation_id) throw new Error("잘못된 충돌 사본 재승인 요청입니다.");
  const renderer_session_id = await getRendererSession();
  const response = object(await invoke("workspace_retry_content_conflict_copy", { request: {
    progress: { renderer_session_id, binding: expectedBinding, document_id, edit_session_id, operation_id, expected_intent_version },
    expected_create_operation_id, parent_id, name: copyName,
  } }));
  if (response.status !== "pending" || Object.keys(response).some((key) => !["status", "version", "node_id", "create_operation_id"].includes(key))) throw new Error("잘못된 충돌 사본 재승인 결과입니다.");
  const resultVersion = generation(response.version), node_id = nodeId(response.node_id), create_operation_id = nodeId(response.create_operation_id);
  if (resultVersion !== expected_intent_version || node_id === document_id || node_id === create_operation_id || create_operation_id === operation_id || create_operation_id === expected_create_operation_id) throw new Error("다른 충돌 사본 재승인 결과입니다.");
  return { status: "pending", version: resultVersion, node_id, create_operation_id };
}

export async function confirmWorkspaceContentConflictCopy(snapshot: ConflictReviewSnapshot, session: string, expectedVersion: string | null, parentId: string, name: string): Promise<WorkspaceContentConflictCopyResponse> {
  const selection = conflictSelection(snapshot, session, expectedVersion), parent_id = nodeId(parentId), copyName = markdownName(name);
  const renderer_session_id = await getRendererSession();
  const response = object(await invoke("workspace_confirm_content_conflict_copy", { request: {
    selection: { renderer_session_id, ...selection }, parent_id, name: copyName,
  } }));
  if (response.status !== "pending" || Object.keys(response).some((key) => !["status", "version", "node_id", "create_operation_id"].includes(key))) throw new Error("잘못된 충돌 사본 결과입니다.");
  const version = generation(response.version), node_id = nodeId(response.node_id), create_operation_id = nodeId(response.create_operation_id);
  if (node_id === selection.document_id || node_id === create_operation_id || create_operation_id === selection.operation_id) throw new Error("다른 충돌 사본 결과입니다.");
  return { status: "pending", version, node_id, create_operation_id };
}

function conflictSelection(snapshot: ConflictReviewSnapshot, session: string, expectedVersion: string | null): Omit<WorkspaceContentConflictChooseRemoteRequest, "renderer_session_id"> {
  const binding = registration(snapshot.binding), document_id = nodeId(snapshot.node_id), operation_id = nodeId(snapshot.operation_id);
  const edit_session_id = string(session), selectedGeneration = generation(snapshot.generation);
  if (!edit_session_id) throw new Error("잘못된 편집 세션입니다.");
  const expected_intent_version = expectedVersion === null ? null : generation(expectedVersion);
  const base_revision = conflictRevision(snapshot.base_revision, true), conflict_revision = conflictRevision(snapshot.conflict_revision, true), remote_revision = conflictRevision(snapshot.remote_revision, false);
  const base_hash = conflictHash(snapshot.base_hash), local_hash = conflictHash(snapshot.local_hash), remote_hash = conflictHash(snapshot.remote_hash);
  return { binding, document_id, edit_session_id, operation_id, generation: selectedGeneration, expected_intent_version, base_revision, conflict_revision, remote_revision, base_hash, local_hash, remote_hash };
}

export async function confirmWorkspaceContentConflictMerge(snapshot: ConflictReviewSnapshot, session: string, expectedVersion: string | null): Promise<WorkspaceContentConflictChooseRemoteResponse> {
  const selection = conflictSelection(snapshot, session, expectedVersion);
  const renderer_session_id = await getRendererSession();
  const response = object(await invoke("workspace_confirm_content_conflict_merge", { request: {
    renderer_session_id, ...selection,
  } }));
  if (response.status === "pending" && Object.keys(response).every((key) => ["status", "version"].includes(key))) {
    const version = generation(response.version);
    if (selection.expected_intent_version === null ? version !== "0" : BigInt(version) !== BigInt(selection.expected_intent_version) && BigInt(version) !== BigInt(selection.expected_intent_version) + 1n) throw new Error("다른 합본 선택 결과입니다.");
    return { status: "pending", version };
  }
  if (response.status === "completed" && Object.keys(response).every((key) => ["status", "generation"].includes(key))) {
    const completed = generation(response.generation);
    if (BigInt(completed) !== BigInt(selection.generation) + 1n) throw new Error("다른 합본 선택 결과입니다.");
    return { status: "completed", generation: completed };
  }
  throw new Error("잘못된 합본 선택 결과입니다.");
}

export async function chooseWorkspaceRemoteContentConflict(snapshot: ConflictReviewSnapshot, session: string, expectedVersion: string | null): Promise<WorkspaceContentConflictChooseRemoteResponse> {
  const selection = conflictSelection(snapshot, session, expectedVersion);
  const renderer_session_id = await getRendererSession();
  const response = object(await invoke("workspace_choose_remote_content_conflict", { request: {
    renderer_session_id, ...selection,
  } }));
  if (response.status === "pending" && Object.keys(response).every((key) => ["status", "version"].includes(key))) {
    return { status: "pending", version: generation(response.version) };
  }
  if (response.status === "completed" && Object.keys(response).every((key) => ["status", "generation"].includes(key))) {
    return { status: "completed", generation: generation(response.generation) };
  }
  throw new Error("잘못된 충돌 해결 결과입니다.");
}

export async function continueWorkspaceRemoteContentConflict(binding: WorkspaceRegistration, documentId: string, sessionId: string, operationId: string, version: string): Promise<WorkspaceContentConflictProgressResponse> {
  const result = await progressRemoteConflict("workspace_continue_remote_content_conflict", binding, documentId, sessionId, operationId, version);
  if (result.status !== "pending" || Object.keys(result).some((key) => !["status", "version", "requested_operation_id"].includes(key))) throw new Error("잘못된 충돌 처리 결과입니다.");
  const resultVersion = generation(result.version);
  const requested = result.requested_operation_id === null ? null : nodeId(result.requested_operation_id);
  if (resultVersion !== version || requested === operationId) throw new Error("잘못된 충돌 처리 결과입니다.");
  return { status: "pending", version: resultVersion, requested_operation_id: requested };
}

export async function queueWorkspaceContentConflictMerge(binding: WorkspaceRegistration, documentId: string, sessionId: string, operationId: string, version: string): Promise<WorkspaceContentConflictProgressResponse> {
  const result = await progressRemoteConflict("workspace_queue_content_conflict_merge", binding, documentId, sessionId, operationId, version);
  if (result.status !== "pending" || Object.keys(result).some((key) => !["status", "version", "requested_operation_id"].includes(key))) throw new Error("잘못된 합본 처리 결과입니다.");
  const resultVersion = generation(result.version), requested = nodeId(result.requested_operation_id);
  if (resultVersion !== version || requested === operationId) throw new Error("다른 합본 처리 결과입니다.");
  return { status: "pending", version: resultVersion, requested_operation_id: requested };
}

export async function completeWorkspacePendingRemoteContentConflict(binding: WorkspaceRegistration, documentId: string, sessionId: string, operationId: string, version: string): Promise<WorkspaceContentConflictChooseRemoteResponse> {
  const result = await progressRemoteConflict("workspace_complete_pending_remote_content_conflict", binding, documentId, sessionId, operationId, version);
  if (result.status !== "completed" || Object.keys(result).some((key) => !["status", "generation"].includes(key))) throw new Error("잘못된 충돌 처리 결과입니다.");
  const completedGeneration = generation(result.generation);
  if (completedGeneration === "0") throw new Error("잘못된 충돌 처리 결과입니다.");
  return { status: "completed", generation: completedGeneration };
}

async function progressRemoteConflict(command: "workspace_continue_remote_content_conflict" | "workspace_complete_pending_remote_content_conflict" | "workspace_queue_content_conflict_merge", binding: WorkspaceRegistration, documentId: string, sessionId: string, operationId: string, version: string): Promise<Record<string, unknown>> {
  const expectedBinding = registration(binding), document_id = nodeId(documentId), operation_id = nodeId(operationId);
  const edit_session_id = string(sessionId), expected_intent_version = generation(version);
  if (!edit_session_id) throw new Error("잘못된 편집 세션입니다.");
  const renderer_session_id = await getRendererSession();
  return object(await invoke(command, { request: { renderer_session_id, binding: expectedBinding, document_id, edit_session_id, operation_id, expected_intent_version } }));
}

export async function reopenWorkspaceContentConflictCheckpoint(binding: WorkspaceRegistration, documentId: string, operationId: string, previousSessionId: string): Promise<WorkspaceContentConflictReopenResponse> {
  const expectedBinding = registration(binding), document_id = nodeId(documentId), operation_id = nodeId(operationId);
  const previous_edit_session_id = string(previousSessionId);
  if (!previous_edit_session_id) throw new Error("잘못된 편집 세션입니다.");
  const renderer_session_id = await getRendererSession();
  const result = object(await invoke("workspace_reopen_content_conflict_checkpoint", { request: { renderer_session_id, binding: expectedBinding, document_id, operation_id, previous_edit_session_id } }));
  if (Object.keys(result).some((key) => !["state", "document"].includes(key))) throw new Error("잘못된 충돌 복구 결과입니다.");
  const state = conflictResolutionState(result.state);
  const document = openedWorkspace(result.document, document_id);
  if (!sameRegistration(document.binding, expectedBinding) || !document.edit_session_id || document.edit_session_id === previous_edit_session_id || document.readonly || !document.local_draft) throw new Error("다른 충돌 복구 문서입니다.");
  return { state, document };
}

export async function workspaceContentConflictResolutionStatus(binding: WorkspaceRegistration, documentId: string, operationId: string): Promise<WorkspaceConflictResolutionResponse> {
  const document_id = nodeId(documentId), operation_id = nodeId(operationId);
  const expectedBinding = registration(binding);
  const renderer_session_id = await getRendererSession();
  const raw = object(await invoke("workspace_content_conflict_resolution_status", { request: { renderer_session_id, binding: expectedBinding, document_id, operation_id } }));
  const responseBinding = registration(raw.binding);
  if (!sameRegistration(responseBinding, expectedBinding) || nodeId(raw.document_id) !== document_id || nodeId(raw.operation_id) !== operation_id ||
    Object.keys(raw).some((key) => !["binding", "document_id", "operation_id", "state"].includes(key))) throw new Error("다른 충돌 해결 상태입니다.");
  return { binding: responseBinding, document_id, operation_id, state: conflictResolutionState(raw.state) };
}

export async function workspaceContentConflictMergeStatus(binding: WorkspaceRegistration, documentId: string, operationId: string, version: string): Promise<WorkspaceContentConflictMergeStatusResponse> {
  const expectedBinding = registration(binding), document_id = nodeId(documentId), operation_id = nodeId(operationId), expected_intent_version = generation(version);
  const renderer_session_id = await getRendererSession();
  const raw = object(await invoke("workspace_content_conflict_merge_status", { request: { renderer_session_id, binding: expectedBinding, document_id, operation_id, expected_intent_version } }));
  if (Object.keys(raw).some((key) => !["binding", "document_id", "operation_id", "intent_version", "merge_operation"].includes(key))) throw new Error("잘못된 합본 상태입니다.");
  const responseBinding = registration(raw.binding), intent_version = generation(raw.intent_version);
  if (!sameRegistration(responseBinding, expectedBinding) || nodeId(raw.document_id) !== document_id || nodeId(raw.operation_id) !== operation_id || intent_version !== expected_intent_version) throw new Error("다른 합본 상태입니다.");
  let merge_operation: WorkspaceContentConflictMergeStatusResponse["merge_operation"] = null;
  if (raw.merge_operation !== null) {
    const operation = object(raw.merge_operation);
    if (Object.keys(operation).some((key) => !["operation_id", "operation_seq"].includes(key))) throw new Error("잘못된 합본 작업입니다.");
    const mergeId = nodeId(operation.operation_id), operation_seq = generation(operation.operation_seq);
    if (mergeId === operation_id || operation_seq === "0") throw new Error("다른 합본 작업입니다.");
    merge_operation = { operation_id: mergeId, operation_seq };
  }
  return { binding: responseBinding, document_id, operation_id, intent_version, merge_operation };
}

function mergeFollowup(value: unknown, conflictId: string): WorkspaceContentConflictMergeFollowup {
  const followup = object(value);
  if (Object.keys(followup).some((key) => !["operation_id", "operation_seq", "generation", "payload_hash"].includes(key))) throw new Error("잘못된 합본 후속 작업입니다.");
  const operation_id = nodeId(followup.operation_id), operation_seq = generation(followup.operation_seq), selectedGeneration = generation(followup.generation), payload_hash = conflictHash(followup.payload_hash);
  if (operation_id === conflictId || operation_seq === "0" || selectedGeneration === "0") throw new Error("잘못된 합본 후속 작업입니다.");
  return { operation_id, operation_seq, generation:selectedGeneration, payload_hash };
}

export async function workspaceContentConflictMergeFollowupStatus(binding: WorkspaceRegistration, documentId: string, operationId: string, version: string): Promise<WorkspaceContentConflictMergeFollowupStatusResponse> {
  const expectedBinding = registration(binding), document_id = nodeId(documentId), operation_id = nodeId(operationId), expected_intent_version = generation(version);
  const renderer_session_id = await getRendererSession();
  const request: WorkspaceContentConflictMergeStatusRequest = { renderer_session_id, binding:expectedBinding, document_id, operation_id, expected_intent_version };
  const raw = object(await invoke("workspace_content_conflict_merge_followup_status", { request }));
  if (Object.keys(raw).some((key) => !["binding", "document_id", "operation_id", "intent_version", "followup"].includes(key))) throw new Error("잘못된 합본 후속 상태입니다.");
  const responseBinding = registration(raw.binding), intent_version = generation(raw.intent_version);
  if (!sameRegistration(responseBinding, expectedBinding) || nodeId(raw.document_id) !== document_id || nodeId(raw.operation_id) !== operation_id || intent_version !== expected_intent_version) throw new Error("다른 합본 후속 상태입니다.");
  const followup = raw.followup === null ? null : mergeFollowup(raw.followup, operation_id);
  return { binding:responseBinding, document_id, operation_id, intent_version, followup };
}

export async function requestWorkspaceContentConflictMergeFollowupAbandon(binding: WorkspaceRegistration, documentId: string, sessionId: string, operationId: string, version: string, selectedFollowup: WorkspaceContentConflictMergeFollowup): Promise<WorkspaceContentConflictProgressResponse> {
  const expectedBinding = registration(binding), document_id = nodeId(documentId), operation_id = nodeId(operationId), expected_intent_version = generation(version), edit_session_id = string(sessionId);
  if (!edit_session_id) throw new Error("잘못된 편집 세션입니다.");
  const followup = mergeFollowup(selectedFollowup, operation_id);
  const renderer_session_id = await getRendererSession();
  const request: WorkspaceContentConflictMergeFollowupApprovalRequest = { progress:{ renderer_session_id, binding:expectedBinding, document_id, edit_session_id, operation_id, expected_intent_version }, followup };
  const result = object(await invoke("workspace_request_content_conflict_merge_followup_abandon", { request }));
  if (result.status !== "pending" || Object.keys(result).some((key) => !["status", "version", "requested_operation_id"].includes(key))) throw new Error("잘못된 합본 후속 처리 결과입니다.");
  const resultVersion = generation(result.version), requested_operation_id = nodeId(result.requested_operation_id);
  if (resultVersion !== expected_intent_version || requested_operation_id !== followup.operation_id) throw new Error("다른 합본 후속 처리 결과입니다.");
  return { status:"pending", version:resultVersion, requested_operation_id };
}

function conflictResolutionState(input: unknown): WorkspaceConflictResolutionResponse["state"] {
  const value = object(input);
  let state: WorkspaceConflictResolutionResponse["state"];
  if (value.status === "not_found") {
    if (Object.keys(value).length !== 1) throw new Error("잘못된 충돌 해결 상태입니다.");
    state = { status: "not_found" };
  } else if (value.status === "pending") {
    if (Object.keys(value).some((key) => !["status", "version", "generation", "choice", "needs_review", "frozen"].includes(key)) ||
      (value.choice !== "keep_local_copy" && value.choice !== "choose_remote" && value.choice !== "save_merged") ||
      typeof value.needs_review !== "boolean" || typeof value.frozen !== "boolean") throw new Error("잘못된 충돌 해결 상태입니다.");
    state = { status: "pending", version: generation(value.version), generation: generation(value.generation), choice: value.choice, needs_review: value.needs_review, frozen: value.frozen };
  } else if (value.status === "completed") {
    if (Object.keys(value).some((key) => !["status", "generation", "frozen"].includes(key)) || typeof value.frozen !== "boolean") throw new Error("잘못된 충돌 해결 상태입니다.");
    state = { status: "completed", generation: generation(value.generation), frozen: value.frozen };
  } else throw new Error("잘못된 충돌 해결 상태입니다.");
  return state;
}

export async function prepareWorkspaceContentConflict(binding: WorkspaceRegistration, documentId: string, sessionId: string, generationValue: string): Promise<ConflictReviewSnapshot | null> {
  const document_id = nodeId(documentId), edit_session_id = string(sessionId), savedGeneration = generation(generationValue);
  if (!edit_session_id) throw new Error("잘못된 편집 세션입니다.");
  const renderer_session_id = await getRendererSession();
  const raw: unknown = await invoke("workspace_prepare_content_conflict", { request: { renderer_session_id, binding, document_id, edit_session_id, generation:savedGeneration } });
  if (raw === null) return null;
  const item = object(raw), responseBinding = registration(item.binding);
  const operation_id = nodeId(item.operation_id), operation_seq = generation(item.operation_seq);
  const resultGeneration = generation(item.generation), responseDocument = nodeId(item.document_id);
  const base_revision = conflictRevision(item.base_revision, true), conflict_revision = conflictRevision(item.conflict_revision, true), remote_revision = conflictRevision(item.remote_revision, false);
  if (!sameRegistration(responseBinding, binding) || responseDocument !== document_id || resultGeneration !== savedGeneration || resultGeneration === "0" || operation_seq === "0" || base_revision === conflict_revision) throw new Error("다른 충돌 비교 결과입니다.");
  return { binding:responseBinding, node_id:responseDocument, operation_id, operation_seq, generation:resultGeneration,
    base_revision, base_bytes:conflictBytes(item.base_bytes_base64), base_hash:conflictHash(item.base_hash),
    local_bytes:conflictBytes(item.local_bytes_base64), local_hash:conflictHash(item.local_hash),
    conflict_revision, remote_revision, remote_bytes:conflictBytes(item.remote_bytes_base64), remote_hash:conflictHash(item.remote_hash) };
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
  return nodeStateWithSequence(value, generation);
}

function nodeStateWithSequence(value: unknown, parseChangeSequence: (value: unknown) => string): NodeState {
  const node = object(value);
  if (node.kind !== "file" && node.kind !== "folder") throw new Error("잘못된 문서 목록입니다.");
  if (typeof node.deleted !== "boolean") throw new Error("잘못된 문서 목록입니다.");
  return {
    node_id: string(node.node_id), parent_id: nullableString(node.parent_id), name: string(node.name),
    kind: node.kind, deleted: node.deleted, content_rev: nullableString(node.content_rev),
    content_etag: nullableString(node.content_etag), meta_rev: string(node.meta_rev),
    meta_etag: string(node.meta_etag), node_etag: string(node.node_etag),
    content_hash: nullableString(node.content_hash), size: nullableString(node.size),
    children_generation: generation(node.children_generation), last_change_seq: parseChangeSequence(node.last_change_seq),
  };
}

export async function workspaceInventory(after_id: string | null): Promise<{ nodes: NodeState[]; next_after_id: string | null }> {
  const renderer_session_id = await getRendererSession();
  const value = object(await invoke("workspace_inventory", { request: { renderer_session_id, after_id } }));
  if (!Array.isArray(value.nodes) || value.nodes.length > 100) throw new Error("잘못된 문서 목록입니다.");
  return { nodes: value.nodes.map(nodeState), next_after_id: nullableString(value.next_after_id) };
}

export async function workspaceCreateParents(binding: WorkspaceRegistration, after_id: string | null): Promise<{ nodes: NodeState[]; next_after_id: string | null }> {
  if (after_id !== null) nodeId(after_id);
  const renderer_session_id = await getRendererSession();
  const page = object(await invoke("workspace_create_parents", { request: { renderer_session_id, binding, after_id } }));
  if (!sameRegistration(registration(page.binding), binding) || string(page.workspace_epoch) !== binding.workspace_epoch || !Array.isArray(page.nodes) || page.nodes.length > 100) throw new Error("잘못된 부모 폴더 목록입니다.");
  let previous = after_id;
  const nodes = page.nodes.map((raw: unknown) => {
    const node = nodeState(raw), id = nodeId(node.node_id);
    if ((previous !== null && id <= previous) || node.kind !== "folder" || node.deleted || (node.parent_id !== null && nodeId(node.parent_id) === id)) throw new Error("잘못된 부모 폴더 목록입니다.");
    previous = id;
    return node;
  });
  const next_after_id = page.next_after_id === null ? null : nodeId(page.next_after_id);
  if (next_after_id !== null && (!nodes.length || next_after_id !== nodes[nodes.length - 1].node_id)) throw new Error("잘못된 부모 폴더 페이지입니다.");
  return { nodes, next_after_id };
}

function registration(value: unknown): WorkspaceRegistration {
  const binding = object(value);
  return { server_origin: string(binding.server_origin), workspace_id: string(binding.workspace_id),
    workspace_epoch: string(binding.workspace_epoch), device_id: string(binding.device_id), device_stream_id: string(binding.device_stream_id) };
}

export async function workspaceSyncState(): Promise<WorkspaceSyncState> {
  const renderer_session_id = await getRendererSession();
  const value = object(await invoke("workspace_sync_status", { request: { renderer_session_id } }));
  const status = value.status;
  if (status !== "connecting" && status !== "ready" && status !== "offline" && status !== "auth_required" && status !== "gateway_auth_required" && status !== "needs_review") {
    throw new Error("잘못된 동기화 상태입니다.");
  }
  return { binding: registration(value.binding), status };
}

function openedWorkspace(value: unknown, documentId: string): WorkspaceOpenedDocument {
  const data = object(value);
  if (typeof data.readonly !== "boolean" || typeof data.local_draft !== "boolean" || typeof data.has_session !== "boolean") throw new Error("잘못된 문서 응답입니다.");
  if (data.cache_warning !== null && data.cache_warning !== "storage" && data.cache_warning !== "damaged" && data.cache_warning !== "unexpected") throw new Error("잘못된 캐시 상태입니다.");
  if ((data.open_notice !== null && data.open_notice !== "prior_draft_preserved") || (data.open_notice === "prior_draft_preserved" && (!data.readonly || data.local_draft))) throw new Error("잘못된 문서 안내입니다.");
  const opened: WorkspaceOpenedDocument & { open_notice: "prior_draft_preserved" | null } = {
    document_id: string(data.document_id), edit_session_id: string(data.edit_session_id),
    generation: generation(data.generation), bytes_base64: string(data.bytes_base64), readonly: data.readonly, local_draft: data.local_draft, has_session: data.has_session,
    binding: registration(data.binding),
    cache_warning: data.cache_warning,
    open_notice: data.open_notice,
  };
  if (opened.document_id !== documentId) throw new Error("다른 문서의 응답입니다.");
  return opened;
}

export async function openWorkspaceDocument(node: NodeState): Promise<WorkspaceOpenedDocument> {
  const renderer_session_id = await getRendererSession();
  return openedWorkspace(await invoke("workspace_open", { request: { renderer_session_id, node } }), node.node_id);
}

export async function prepareWorkspaceRemoteRefresh(document_id: string, edit_session_id: string): Promise<{ token: string; bytes_base64: string } | null> {
  const renderer_session_id = await getRendererSession();
  const value: unknown = await invoke("workspace_prepare_remote_refresh", { request: { renderer_session_id, document_id, edit_session_id } });
  if (value === null) return null;
  const candidate = object(value);
  return { token: string(candidate.token), bytes_base64: string(candidate.bytes_base64) };
}

export async function commitWorkspaceRemoteRefresh(document_id: string, edit_session_id: string, token: string): Promise<boolean> {
  const renderer_session_id = await getRendererSession();
  const value: unknown = await invoke("workspace_commit_remote_refresh", { request: { renderer_session_id, document_id, edit_session_id, token } });
  if (typeof value !== "boolean") throw new Error("잘못된 문서 갱신 응답입니다.");
  return value;
}

export async function inspectWorkspaceRemoteChange(document_id: string, edit_session_id: string, generation: string): Promise<"same" | "changed" | "deleted" | "pending"> {
  const renderer_session_id = await getRendererSession();
  const value: unknown = await invoke("workspace_inspect_dirty_remote", { request: { renderer_session_id, document_id, edit_session_id, generation } });
  if (value !== "same" && value !== "changed" && value !== "deleted" && value !== "pending") throw new Error("잘못된 원격 변경 상태입니다.");
  return value;
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
  if (status !== "idle" && status !== "pending" && status !== "committed" && status !== "conflict" && status !== "failed" && status !== "abandoned" && status !== "permission_denied" && status !== "needs_review" && status !== "sync_pending" && status !== "ack_pending" && status !== "auth_required" && status !== "gateway_auth_required") throw new Error("잘못된 전송 상태입니다.");
  const operation_id = nullableString(value.operation_id), document_id = nullableString(value.document_id), current_generation = value.generation === null ? null : generation(value.generation);
  if (status === "permission_denied" && (operation_id === null || document_id === null || current_generation !== null)) throw new Error("잘못된 권한 거절 응답입니다.");
  if (status === "permission_denied") { nodeId(operation_id); nodeId(document_id); }
  let content_block: WorkspaceSendResult["content_block"] = null;
  if (value.content_block !== null) {
    const block = object(value.content_block);
    if (Object.keys(block).some((key) => !["conflict_operation_id", "blocked_operation_id", "blocked_seq"].includes(key))) throw new Error("잘못된 충돌 차단 상태입니다.");
    const conflict_operation_id = nodeId(block.conflict_operation_id), blocked_operation_id = nodeId(block.blocked_operation_id), blocked_seq = generation(block.blocked_seq);
    if (status !== "needs_review" || operation_id === null || document_id === null || current_generation === null || current_generation === "0" ||
      blocked_operation_id !== nodeId(operation_id) || conflict_operation_id === blocked_operation_id || blocked_seq === "0") throw new Error("잘못된 충돌 차단 상태입니다.");
    nodeId(document_id);
    content_block = { conflict_operation_id, blocked_operation_id, blocked_seq };
  }
  return { binding: registration(value.binding), status, operation_id, document_id, generation: current_generation, content_block };
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

export async function workspaceOutbox(binding: WorkspaceRegistration, afterSeq: string): Promise<WorkspaceOutboxPage> {
  const expected = registration(binding), after_seq = generation(afterSeq);
  if (Object.values(expected).some((value) => !value)) throw new Error("잘못된 등록입니다.");
  const renderer_session_id = await getRendererSession();
  const page = object(await invoke("workspace_outbox", { request: { renderer_session_id, binding: expected, after_seq, limit: 100 } }));
  if (Object.keys(page).some((key) => !["entries", "next_after_seq"].includes(key)) || !Array.isArray(page.entries) || page.entries.length > 100) throw new Error("잘못된 전송 목록입니다.");
  let previous = BigInt(after_seq);
  const ids = new Set<string>();
  const entries = page.entries.map((raw: unknown): WorkspaceOutboxEntry => {
    const item = object(raw);
    if (Object.keys(item).some((key) => !["operation_id", "operation_seq", "document_id", "generation", "status", "frozen", "abandon_state", "failure", "payload_retired"].includes(key))) throw new Error("잘못된 전송 항목입니다.");
    const entry = outboxEntry(item);
    const id = nodeId(entry.operation_id), document_id = nodeId(entry.document_id), seq = BigInt(entry.operation_seq);
    if (entry.generation === "0" || seq <= previous || ids.has(id)) throw new Error("잘못된 전송 순서입니다.");
    previous = seq; ids.add(id);
    return { ...entry, operation_id: id, document_id };
  });
  const next_after_seq = page.next_after_seq === null ? null : generation(page.next_after_seq);
  if (next_after_seq !== null && (entries.length !== 100 || BigInt(next_after_seq) !== previous)) throw new Error("잘못된 전송 페이지입니다.");
  return { entries, next_after_seq };
}

export async function workspaceDocumentOutbox(binding: WorkspaceRegistration, document_id: string): Promise<WorkspaceDocumentOutboxState> {
  const renderer_session_id = await getRendererSession();
  const value = object(await invoke("workspace_document_outbox", { request: { renderer_session_id, binding, document_id } }));
  if (typeof value.has_conflict !== "boolean" || typeof value.has_failure !== "boolean" || Object.keys(value).some((key) => !["binding", "document_id", "latest", "first_content_conflict", "has_conflict", "has_failure", "confirmed_through"].includes(key))) throw new Error("잘못된 전송 상태입니다.");
  let first_content_conflict: WorkspaceDocumentOutboxState["first_content_conflict"] = null;
  if (value.first_content_conflict !== null) {
    const conflict = object(value.first_content_conflict);
    if (Object.keys(conflict).some((key) => !["operation_id", "operation_seq"].includes(key))) throw new Error("잘못된 충돌 상태입니다.");
    const operation_id = nodeId(conflict.operation_id), operation_seq = generation(conflict.operation_seq);
    if (operation_seq === "0" || !value.has_conflict) throw new Error("잘못된 충돌 상태입니다.");
    first_content_conflict = { operation_id, operation_seq };
  }
  const result = { binding: registration(value.binding), document_id: string(value.document_id), latest: value.latest === null ? null : outboxEntry(value.latest),
    first_content_conflict, has_conflict: value.has_conflict, has_failure: value.has_failure, confirmed_through: generation(value.confirmed_through) };
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
