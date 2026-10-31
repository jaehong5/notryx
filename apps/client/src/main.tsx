import { createRoot } from "react-dom/client";
import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState, type FormEvent } from "react";
import { Compartment, EditorState } from "@codemirror/state";
import { EditorView, keymap, lineNumbers, drawSelection } from "@codemirror/view";
import { defaultKeymap, history, historyKeymap } from "@codemirror/commands";
import { markdown } from "@codemirror/lang-markdown";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { EditorController, initialEditorState } from "./editor-controller";
import { openDocument, saveDocument, closeDocument, pairWorkspace, workspaceInventory, workspaceCreateParents, workspaceFrozenCreates, resolveWorkspaceFrozenCreate, workspaceFrozenDeletes, resolveWorkspaceFrozenDelete, workspaceFrozenMoves, resolveWorkspaceFrozenMove, workspaceSyncState, openWorkspaceDocument, prepareWorkspaceRemoteRefresh, commitWorkspaceRemoteRefresh, inspectWorkspaceRemoteChange, saveWorkspaceDocument, closeWorkspaceDocument, localDrafts, openLocalDraft, adoptPendingPair, discardPendingPair, pendingPairStatus, queueWorkspaceDocument, sendNextWorkspaceOperation, previewWorkspaceImage, workspaceDocumentOutbox, openWorkspaceExternal, recoverWorkspaceSource, workspaceTrash, queueWorkspaceRestore, workspaceRestoreStatus, requestWorkspaceRestoreAbandon, queueWorkspaceCreate, workspaceCreateStatus, requestWorkspaceCreateAbandon, queueWorkspaceDelete, workspaceDeleteStatus, requestWorkspaceDeleteAbandon, queueWorkspaceMove, workspaceMoveStatus, requestWorkspaceMoveAbandon, workspaceSearch, cancelWorkspaceSearch } from "./ipc";
import { MarkdownPreview } from "./preview";
import { PreviewImageQueue } from "./preview-image-queue";
import { workspacePath, draftKey, draftSelectionId } from "./workspace-list";
import { WorkspaceController } from "./workspace-controller";
import { resolveWorkspaceReference } from "./workspace-reference";
import { collectPreviewReferences } from "./preview-references";
import { compareMoveReferences } from "./move-reference-impact";
import { AutoTransmit, QueueStorageError, type TransmitScope } from "./auto-transmit";
import { clearQueueError, putQueueError, QueueAlerts, type QueueError } from "./queue-errors";
import { receiptWaitResult, staleTransmissionResult } from "./send-result";
import { documentSizeGuard } from "./document-size-guard";
import { StartupGate } from "./migration-recovery";
import { workspaceOpenError } from "./workspace-open-error";
import { credentialStateMessage, workspaceCredentialStatus } from "./credential-status";
import { TrashPanel, type TrashPanelPorts } from "./trash-panel";
import { CreatePanel, type CreatePanelPorts } from "./create-panel";
import { DeletePanel, type DeletePanelPorts } from "./delete-panel";
import { MovePanel, type MovePanelPorts } from "./move-panel";
import { FrozenCreateReview, type FrozenCreateReviewPorts } from "./frozen-create-review";
import { FrozenDeleteReview, type FrozenDeleteReviewPorts } from "./frozen-delete-review";
import { FrozenMoveReview, type FrozenMoveReviewPorts } from "./frozen-move-review";
import { FrozenContentReview, type FrozenContentReviewItem, type FrozenContentReviewPorts } from "./frozen-content-review";
import { workspaceOutbox, workspaceInspectFrozenContent, workspaceInspectFrozenContentCrossEpoch, workspaceReviewFrozenContentLocal, workspaceApplyFrozenContent, workspaceApplyFrozenContentCrossEpoch, workspaceDismissFrozenContent, workspaceDismissFrozenContentLocal, workspaceFrozenContentDecisionStatus } from "./ipc";
import { SearchPanel, type SearchPanelPorts, type SearchResult } from "./search-panel";
import { ConflictReview } from "./conflict-review";
import { ConflictCopyRetry } from "./conflict-copy-retry";
import { DeletedDraftCopyReview } from "./deleted-draft-copy-review";
import { useDeletedDraftCopy } from "./use-deleted-draft-copy";
import { useContentConflict } from "./use-content-conflict";
import type { NodeState, WorkspaceDraft, WorkspaceRegistration } from "../../../packages/protocol-types";
import "./style.css";

const documents = [
  ["welcome", "시작하기"], ["unicode", "한글과 원문 보존"], ["100k", "100 KiB 문서"],
  ["100k-b", "100 KiB 문서 B"], ["5m", "5 MiB 문서"], ["5m-b", "5 MiB 문서 B"],
  ["mixed", "혼합 줄바꿈"], ["non-utf8", "비UTF-8 원문"],
];

function App() {
  const parent = useRef<HTMLDivElement>(null);
  const controller = useRef<EditorController | null>(null);
  const transmitter = useRef<AutoTransmit | null>(null);
  const source = useRef("");
  const previewEnabled = useRef(false);
  const previewTimer = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);
  const modeRef = useRef<"fixture" | "workspace">("fixture");
  const nodesRef = useRef<NodeState[]>([]);
  const searchNodesRef = useRef(new Map<string, NodeState>());
  const workspace = useRef(new WorkspaceController());
  const metadataVersion = useRef(0);
  const inventoryVersion = useRef<number | null>(null);
  const inventoryBinding = useRef<string | null>(null);
  const sessionModes = useRef(new Map<string, { mode: "fixture" | "workspace"; documentId: string; uiId: string; scope?: TransmitScope; binding?: WorkspaceRegistration; orphanDraft?: boolean }>());
  const unqueuedEdits = useRef(new Map<string, string>());
  const syncRevision = useRef(0);
  const credentialRevision = useRef(0);
  const credentialMounted = useRef(false);
  const workspacePageStartRef = useRef<string | null>(null);
  const selectedRef = useRef("");
  const imageQueue = useRef(new PreviewImageQueue());
  const draftsRef = useRef<WorkspaceDraft[]>([]);
  const connecting = useRef(false);
  const registrationVersion = useRef(0);
  const workspaceBindingRef = useRef<WorkspaceRegistration | null>(null);
  const [state, setState] = useState(initialEditorState);
  const [preview, setPreview] = useState(false);
  const [previewSource, setPreviewSource] = useState("");
  const [previewAnchor, setPreviewAnchor] = useState<{ documentId: string; fragment: string } | null>(null);
  const [probeError, setProbeError] = useState("");
  const [isolation, setIsolation] = useState(false);
  const [mode, setMode] = useState<"fixture" | "workspace">("fixture");
  const [nodes, setNodes] = useState<NodeState[]>([]);
  const [workspacePageStart, setWorkspacePageStart] = useState<string | null>(null);
  const [workspaceNextPage, setWorkspaceNextPage] = useState<string | null>(null);
  const [drafts, setDrafts] = useState<WorkspaceDraft[]>([]);
  const [connectionError, setConnectionError] = useState("");
  const [inventoryError, setInventoryError] = useState("");
  const [queueErrors, setQueueErrors] = useState<QueueError[]>([]);
  const [pendingPair, setPendingPair] = useState(false);
  const [busy, setBusy] = useState(true);
  const [workspaceConnecting, setWorkspaceConnecting] = useState(false);
  const [syncStatus, setSyncStatus] = useState("");
  const [workspaceSyncMessage, setWorkspaceSyncMessage] = useState("");
  const [credentialMessage, setCredentialMessage] = useState("");
  const [recoveryMessage, setRecoveryMessage] = useState("");
  const [memoryRecovery, setMemoryRecovery] = useState<{ instance: EditorController; id: string; session: string; generation: string; revision: number; registration: number; binding: string; sessionRef: ReturnType<typeof sessionModes.current.get>; sessionBinding?: WorkspaceRegistration; text: string } | null>(null);
  const [hasContentConflict, setHasContentConflict] = useState(false);
  const [copyRevision, setCopyRevision] = useState(0);
  const {
    conflictPreparing, conflictMessage, conflictSnapshot, conflictRecovery, conflictRecovering,
    pendingConflict, pendingMerge, pendingCopy, copyRetryOpen, conflictReviewPorts, conflictActionRevision,
    setConflictSnapshot, setConflictMessage, setPendingConflict, setPendingMerge, setPendingCopy, setCopyRetryOpen,
    inspectConflict, conflictContextCurrent, chooseRemote, saveMerged, refreshMerge, queueMerge,
    refreshMergeFollowup, approveMergeFollowup, keepConflictCopy, refreshCopy, retryCopy, progressCopy,
    recoverConflictResult, continueConflict, completeConflict, observeConflictStatus,
  } = useContentConflict({ controller, transmitter, registrationVersion, workspaceBindingRef, sessionModes, syncRevision, copyRevision, setCopyRevision, refreshSyncStatus, setConnectionError });
  const [recovering, setRecovering] = useState(false);
  const [workspaceBinding, setWorkspaceBinding] = useState<WorkspaceRegistration | null>(null);
  const [trashOpen, setTrashOpen] = useState(false);
  const [createOpen, setCreateOpen] = useState(false);
  const [deleteTarget, setDeleteTarget] = useState<{ node: NodeState; path: string } | null>(null);
  const [deleteRequestsOpen, setDeleteRequestsOpen] = useState(false);
  const [moveTarget, setMoveTarget] = useState<{ node: NodeState; path: string } | null>(null);
  const [moveRequestsOpen, setMoveRequestsOpen] = useState(false);
  const [frozenCreateOpen, setFrozenCreateOpen] = useState(false);
  const [frozenDeleteOpen, setFrozenDeleteOpen] = useState(false);
  const [frozenMoveOpen, setFrozenMoveOpen] = useState(false);
  const [frozenContentOpen, setFrozenContentOpen] = useState(false);
  const frozenContentItems = useRef(new Map<string, FrozenContentReviewItem>());
  const frozenContentListRevision = useRef(0);
  const [searchOpen, setSearchOpen] = useState(false);
  const [searchRevision, setSearchRevision] = useState(0);
  const [restoreRevision, setRestoreRevision] = useState(0);
  const [restorePermission, setRestorePermission] = useState<{ binding: string; operationId: string } | null>(null);
  const recoveringRef = useRef(false);
  const trashPorts = useMemo<TrashPanelPorts>(() => ({ trash: workspaceTrash, folders: workspaceInventory, queue: queueWorkspaceRestore, status: workspaceRestoreStatus, cancel: requestWorkspaceRestoreAbandon }), []);
  const createPorts = useMemo<CreatePanelPorts>(() => ({
    folders: workspaceCreateParents,
    queue: queueWorkspaceCreate,
    status: workspaceCreateStatus,
    retry: async (binding, operationId) => {
      const key = JSON.stringify(binding);
      if (!workspaceBindingRef.current || JSON.stringify(workspaceBindingRef.current) !== key || !transmitter.current?.retryRestoreAfterPermissionDenied(key, operationId)) throw new Error("stale_operation");
      setRestorePermission((current) => current?.binding === key && current.operationId === operationId ? null : current);
    },
    cancel: async (binding, operationId) => {
      await requestWorkspaceCreateAbandon(binding, operationId);
      const key = JSON.stringify(binding);
      if (!workspaceBindingRef.current || JSON.stringify(workspaceBindingRef.current) !== key) return;
      if (!transmitter.current?.resumeRestoreAfterCancel(key, operationId)) transmitter.current?.kick();
      setRestoreRevision((current) => current + 1);
    },
  }), []);
  const deletePorts = useMemo<DeletePanelPorts>(() => ({
    queue: queueWorkspaceDelete,
    status: workspaceDeleteStatus,
    retry: async (binding, operationId) => {
      const key = JSON.stringify(binding);
      if (!workspaceBindingRef.current || JSON.stringify(workspaceBindingRef.current) !== key || !transmitter.current?.retryRestoreAfterPermissionDenied(key, operationId)) throw new Error("stale_operation");
      setRestorePermission((current) => current?.binding === key && current.operationId === operationId ? null : current);
    },
    cancel: async (binding, operationId) => {
      await requestWorkspaceDeleteAbandon(binding, operationId);
      const key = JSON.stringify(binding);
      if (!workspaceBindingRef.current || JSON.stringify(workspaceBindingRef.current) !== key) return;
      if (!transmitter.current?.resumeRestoreAfterCancel(key, operationId)) transmitter.current?.kick();
      setRestoreRevision((current) => current + 1);
    },
  }), []);
  const movePorts = useMemo<MovePanelPorts>(() => ({
    folders: workspaceCreateParents,
    status: workspaceMoveStatus,
    inspect: (binding, nodeId, request) => {
      const key = JSON.stringify(binding);
      const controllerInstance = controller.current;
      if (!controllerInstance || !workspaceBindingRef.current || JSON.stringify(workspaceBindingRef.current) !== key
        || inventoryBinding.current !== key || inventoryVersion.current !== metadataVersion.current) return { status: "unavailable" };
      try {
        const snapshot = controllerInstance.recoverySnapshot();
        const session = sessionModes.current.get(snapshot.session);
        if (snapshot.id !== nodeId || !session || session.mode !== "workspace" || session.documentId !== nodeId || !session.binding || JSON.stringify(session.binding) !== key) return { status: "unavailable" };
        const tree = nodesRef.current;
        const version = registrationVersion.current;
        const metadata = metadataVersion.current;
        const references = collectPreviewReferences(new TextDecoder("utf-8", { fatal: true }).decode(snapshot.bytes));
        if (references.status !== "checked") return { status: "unavailable" };
        const impact = compareMoveReferences(tree, { sourceId: nodeId, parentId: request.parent_id, name: request.name }, references.references);
        if (impact.status !== "checked") return { status: "unavailable" };
        return {
          status: "checked",
          affected: impact.results.filter((item) => item.outcome === "target_changed" || item.outcome === "unresolved_after").length,
          unresolved: impact.results.filter((item) => item.outcome === "unresolved_before").length,
          isCurrent: () => controller.current === controllerInstance && controllerInstance.isCurrentSnapshot(snapshot.session, snapshot.generation, snapshot.revision)
            && nodesRef.current === tree && registrationVersion.current === version && metadataVersion.current === metadata
            && inventoryVersion.current === metadata && inventoryBinding.current === key
            && workspaceBindingRef.current !== null && JSON.stringify(workspaceBindingRef.current) === key,
        };
      } catch { return { status: "unavailable" }; }
    },
    queue: async (binding, request) => {
      const version = registrationVersion.current;
      const key = JSON.stringify(binding);
      const operation = await queueWorkspaceMove(binding, request);
      if (version === registrationVersion.current && workspaceBindingRef.current && JSON.stringify(workspaceBindingRef.current) === key) {
        setRestoreRevision((current) => current + 1);
        transmitter.current?.kick();
      }
      return operation;
    },
    retry: async (binding, operationId) => {
      const key = JSON.stringify(binding);
      if (!workspaceBindingRef.current || JSON.stringify(workspaceBindingRef.current) !== key || !transmitter.current?.retryRestoreAfterPermissionDenied(key, operationId)) throw new Error("stale_operation");
      setRestorePermission((current) => current?.binding === key && current.operationId === operationId ? null : current);
      setRestoreRevision((current) => current + 1);
    },
    cancel: async (binding, operationId) => {
      const version = registrationVersion.current;
      await requestWorkspaceMoveAbandon(binding, operationId);
      const key = JSON.stringify(binding);
      if (version !== registrationVersion.current || !workspaceBindingRef.current || JSON.stringify(workspaceBindingRef.current) !== key) return;
      if (!transmitter.current?.resumeRestoreAfterCancel(key, operationId)) transmitter.current?.kick();
      setRestoreRevision((current) => current + 1);
    },
  }), []);
  const frozenCreatePorts = useMemo<FrozenCreateReviewPorts>(() => ({
    list: workspaceFrozenCreates,
    folders: workspaceCreateParents,
    decide: async (binding, oldBinding, operationId, choice) => {
      const version = registrationVersion.current;
      const key = JSON.stringify(binding);
      await resolveWorkspaceFrozenCreate(binding, oldBinding, operationId, choice);
      if (choice.kind !== "apply" || version !== registrationVersion.current || !workspaceBindingRef.current || JSON.stringify(workspaceBindingRef.current) !== key) return;
      setRestoreRevision((current) => current + 1);
      transmitter.current?.kick();
    },
  }), []);
  const frozenDeletePorts = useMemo<FrozenDeleteReviewPorts>(() => ({
    list: workspaceFrozenDeletes,
    decide: async (binding, oldBinding, operationId, choice) => {
      const version = registrationVersion.current;
      const key = JSON.stringify(binding);
      await resolveWorkspaceFrozenDelete(binding, oldBinding, operationId, choice);
      if (choice.kind !== "apply" || version !== registrationVersion.current || !workspaceBindingRef.current || JSON.stringify(workspaceBindingRef.current) !== key) return;
      setRestoreRevision((current) => current + 1);
      transmitter.current?.kick();
    },
  }), []);
  const frozenMovePorts = useMemo<FrozenMoveReviewPorts>(() => ({
    list: workspaceFrozenMoves,
    decide: async (binding, oldBinding, operationId, choice) => {
      const version = registrationVersion.current;
      const key = JSON.stringify(binding);
      await resolveWorkspaceFrozenMove(binding, oldBinding, operationId, choice);
      if (choice.kind !== "apply" || version !== registrationVersion.current || !workspaceBindingRef.current || JSON.stringify(workspaceBindingRef.current) !== key) return;
      setRestoreRevision((current) => current + 1);
      transmitter.current?.kick();
    },
  }), []);
  const frozenContentPorts = useMemo<FrozenContentReviewPorts>(() => ({
    list: async (binding, cursor) => {
      const version = registrationVersion.current, key = JSON.stringify(binding), attempt = ++frozenContentListRevision.current;
      const current = () => registrationVersion.current === version && !!workspaceBindingRef.current && JSON.stringify(workspaceBindingRef.current) === key && frozenContentListRevision.current === attempt;
      if (!current()) throw new Error("stale_registration");
      if (cursor === null) frozenContentItems.current.clear();
      const drafts = await localDrafts();
      if (!current()) throw new Error("stale_registration");
      const groups = new Map<string, { binding: WorkspaceRegistration; documents: Set<string> }>();
      for (const draft of drafts) {
        const oldKey = JSON.stringify(draft.binding);
        if (draft.current_registration || draft.binding.workspace_id !== binding.workspace_id || oldKey === key) continue;
        let group = groups.get(oldKey);
        if (!group) { group = { binding: draft.binding, documents: new Set() }; groups.set(oldKey, group); }
        group.documents.add(draft.document_id);
      }
      const keys = [...groups.keys()].sort();
      let oldKey = keys[0], after = "0";
      if (cursor !== null) {
        const parsed: unknown = JSON.parse(cursor);
        if (!Array.isArray(parsed) || parsed.length !== 2 || typeof parsed[0] !== "string" || typeof parsed[1] !== "string" || !groups.has(parsed[0]) || !/^(0|[1-9][0-9]*)$/.test(parsed[1])) throw new Error("stale_cursor");
        [oldKey, after] = parsed;
      }
      if (!oldKey) return { binding, entries: [], next_after_id: null };
      const group = groups.get(oldKey)!;
      const page = await workspaceOutbox(group.binding, after);
      if (!current()) throw new Error("stale_registration");
      const entries: FrozenContentReviewItem[] = [];
      for (const operation of page.entries) {
        if (!operation.frozen || operation.payload_retired || !group.documents.has(operation.document_id) || (operation.status !== "queued" && operation.status !== "sent" && operation.status !== "abandoning")) continue;
        const item = { old_binding:group.binding, operation_id:operation.operation_id, operation_seq:operation.operation_seq, document_id:operation.document_id, generation:operation.generation, status:operation.status, abandon_state:operation.abandon_state };
        entries.push(item); frozenContentItems.current.set(JSON.stringify([oldKey, operation.operation_id]), item);
      }
      const nextKey = keys[keys.indexOf(oldKey) + 1];
      const next_after_id = page.next_after_seq !== null ? JSON.stringify([oldKey, page.next_after_seq]) : nextKey ? JSON.stringify([nextKey, "0"]) : null;
      return { binding, entries, next_after_id };
    },
    load: async (binding, oldBinding, operationId) => {
      const version = registrationVersion.current, key = JSON.stringify(binding), oldKey = JSON.stringify(oldBinding);
      const item = frozenContentItems.current.get(JSON.stringify([oldKey, operationId]));
      const current = () => registrationVersion.current === version && !!workspaceBindingRef.current && JSON.stringify(workspaceBindingRef.current) === key && frozenContentItems.current.get(JSON.stringify([oldKey, operationId])) === item;
      if (!item || !current() || oldBinding.workspace_id !== binding.workspace_id || oldKey === key) throw new Error("stale_inspection");
      const result = await workspaceInspectFrozenContent(binding, oldBinding, operationId, item.document_id);
      if (!current() || result.operation_seq !== item.operation_seq || result.operation_generation !== item.generation || result.operation_status !== item.status || result.abandon_state !== item.abandon_state) throw new Error("stale_inspection");
      return { binding:result.current_binding, old_binding:result.old_binding, operation_id:result.operation_id, operation_seq:result.operation_seq, document_id:result.document_id,
        generation:result.operation_generation, local_generation:result.local_generation, status:result.operation_status, abandon_state:result.abandon_state, expected_revision:result.expected_revision,
        outcome_unknown:result.outcome_unknown, base_revision:result.base_revision, remote_revision:result.remote_revision, inspected:result.inspected,
        operation_bytes:result.operation_bytes, base_bytes:result.base_bytes, local_bytes:result.local_bytes, remote_bytes:result.remote_bytes };
    },
    loadLocal: async (binding, oldBinding, operationId) => {
      const version = registrationVersion.current, key = JSON.stringify(binding), oldKey = JSON.stringify(oldBinding);
      const item = frozenContentItems.current.get(JSON.stringify([oldKey, operationId]));
      const current = () => registrationVersion.current === version && !!workspaceBindingRef.current && JSON.stringify(workspaceBindingRef.current) === key && frozenContentItems.current.get(JSON.stringify([oldKey, operationId])) === item;
      if (!item || !current() || oldBinding.workspace_id !== binding.workspace_id || oldKey === key) throw new Error("stale_inspection");
      const result = await workspaceReviewFrozenContentLocal(binding, oldBinding, operationId, item.document_id);
      if (!current() || result.operation_seq !== item.operation_seq || result.generation !== item.generation || result.status !== item.status || result.abandon_state !== item.abandon_state) throw new Error("stale_inspection");
      return result;
    },
    loadCrossEpoch: async (binding, oldBinding, operationId) => {
      const version = registrationVersion.current, key = JSON.stringify(binding), oldKey = JSON.stringify(oldBinding);
      const item = frozenContentItems.current.get(JSON.stringify([oldKey, operationId]));
      const current = () => registrationVersion.current === version && !!workspaceBindingRef.current && JSON.stringify(workspaceBindingRef.current) === key && frozenContentItems.current.get(JSON.stringify([oldKey, operationId])) === item;
      if (!item || !current() || oldBinding.workspace_id !== binding.workspace_id || oldBinding.workspace_epoch === binding.workspace_epoch) throw new Error("stale_inspection");
      const result = await workspaceInspectFrozenContentCrossEpoch(binding, oldBinding, operationId, item.document_id);
      if (!current() || result.operation_seq !== item.operation_seq || result.generation !== item.generation || result.status !== item.status || result.abandon_state !== item.abandon_state) throw new Error("stale_inspection");
      return result;
    },
    status: async (binding, oldBinding, documentId, operationId) => {
      const version = registrationVersion.current, key = JSON.stringify(binding), oldKey = JSON.stringify(oldBinding);
      const item = frozenContentItems.current.get(JSON.stringify([oldKey, operationId]));
      const current = () => registrationVersion.current === version && !!workspaceBindingRef.current && JSON.stringify(workspaceBindingRef.current) === key && frozenContentItems.current.get(JSON.stringify([oldKey, operationId])) === item;
      if (!item || !current() || item.document_id !== documentId) throw new Error("stale_inspection");
      const result = await workspaceFrozenContentDecisionStatus(binding, oldBinding, documentId, operationId);
      if (!current()) throw new Error("stale_registration");
      return result;
    },
    apply: async (snapshot) => {
      const version = registrationVersion.current, key = JSON.stringify(snapshot.binding), oldKey = JSON.stringify(snapshot.old_binding);
      const item = frozenContentItems.current.get(JSON.stringify([oldKey, snapshot.operation_id]));
      const current = () => registrationVersion.current === version && !!workspaceBindingRef.current && JSON.stringify(workspaceBindingRef.current) === key && frozenContentItems.current.get(JSON.stringify([oldKey, snapshot.operation_id])) === item;
      if (!item || !current() || item.document_id !== snapshot.document_id || item.operation_seq !== snapshot.operation_seq || item.generation !== snapshot.generation || item.status !== snapshot.status) throw new Error("stale_inspection");
      const result = await workspaceApplyFrozenContent(snapshot);
      if (!current()) throw new Error("stale_registration");
      transmitter.current?.kick();
      return result;
    },
    applyCrossEpoch: async (snapshot, confirmed) => {
      const version = registrationVersion.current, key = JSON.stringify(snapshot.binding), oldKey = JSON.stringify(snapshot.old_binding);
      const item = frozenContentItems.current.get(JSON.stringify([oldKey, snapshot.operation_id]));
      const current = () => registrationVersion.current === version && !!workspaceBindingRef.current && JSON.stringify(workspaceBindingRef.current) === key && frozenContentItems.current.get(JSON.stringify([oldKey, snapshot.operation_id])) === item;
      if (!item || !current() || item.document_id !== snapshot.document_id || item.operation_seq !== snapshot.operation_seq || item.generation !== snapshot.generation || item.status !== snapshot.status || item.abandon_state !== snapshot.abandon_state) throw new Error("stale_inspection");
      const result = await workspaceApplyFrozenContentCrossEpoch(snapshot, confirmed);
      if (!current()) throw new Error("stale_registration");
      transmitter.current?.kick();
      return result;
    },
    dismiss: async (snapshot) => {
      const version = registrationVersion.current, key = JSON.stringify(snapshot.binding), oldKey = JSON.stringify(snapshot.old_binding);
      const item = frozenContentItems.current.get(JSON.stringify([oldKey, snapshot.operation_id]));
      const current = () => registrationVersion.current === version && !!workspaceBindingRef.current && JSON.stringify(workspaceBindingRef.current) === key && frozenContentItems.current.get(JSON.stringify([oldKey, snapshot.operation_id])) === item;
      if (!item || !current() || item.document_id !== snapshot.document_id || item.operation_seq !== snapshot.operation_seq || item.generation !== snapshot.generation || item.status !== snapshot.status || item.abandon_state !== snapshot.abandon_state) throw new Error("stale_inspection");
      const result = await workspaceDismissFrozenContent(snapshot);
      if (!current()) throw new Error("stale_registration");
      return result;
    },
    dismissLocal: async (snapshot) => {
      const version = registrationVersion.current, key = JSON.stringify(snapshot.binding), oldKey = JSON.stringify(snapshot.old_binding);
      const item = frozenContentItems.current.get(JSON.stringify([oldKey, snapshot.operation_id]));
      const current = () => registrationVersion.current === version && !!workspaceBindingRef.current && JSON.stringify(workspaceBindingRef.current) === key && frozenContentItems.current.get(JSON.stringify([oldKey, snapshot.operation_id])) === item;
      if (!item || !current() || item.document_id !== snapshot.document_id || item.operation_seq !== snapshot.operation_seq || item.generation !== snapshot.generation || item.status !== snapshot.status || item.abandon_state !== snapshot.abandon_state) throw new Error("stale_inspection");
      const result = await workspaceDismissFrozenContentLocal(snapshot);
      if (!current()) throw new Error("stale_registration");
      return result;
    },
  }), []);
  const searchPorts = useMemo<SearchPanelPorts>(() => ({ scan: workspaceSearch, cancel: cancelWorkspaceSearch }), []);

  function reportQueueError(scope: TransmitScope, message: string): void {
    setQueueErrors((errors) => putQueueError(errors, scope, message));
  }

  const {
    deletedCopyReview, deletedCopyRecovery, deletedCopyBusy, deletedCopyRecorded, deletedCopyStatus,
    deletedFollowupApproval, deletedCopyMessage, deletedCopyRetryOpen,
    setDeletedCopyReview, setDeletedCopyRetryOpen, setDeletedFollowupApproval,
    deletedCopyCurrent, deletedStatusCurrent, refreshDeletedCopyStatus, reconfirmDeletedCopy,
    progressDeletedCopy, approveDeletedFollowup, inspectDeletedCopy, confirmDeletedCopy, recoverDeletedCopy,
    resetDeletedCopy, editedDeletedCopy, restoreDeletedCopy,
  } = useDeletedDraftCopy({ controller, transmitter, registrationVersion, workspaceBindingRef, sessionModes, copyRevision, setCopyRevision });

  async function recoverSource(action: "copy" | "export") {
    if (recoveringRef.current) return;
    recoveringRef.current = true;
    setRecovering(true);
    setRecoveryMessage("");
    setMemoryRecovery(null);
    const instance = controller.current;
    const registration = registrationVersion.current;
    const binding = JSON.stringify(workspaceBindingRef.current);
    let snapshot: ReturnType<EditorController["recoverySnapshot"]> | undefined;
    let recoverySession: ReturnType<typeof sessionModes.current.get> = undefined;
    const sameContext = () => controller.current === instance && registrationVersion.current === registration && JSON.stringify(workspaceBindingRef.current) === binding && !!snapshot && !!instance?.isCurrentSession(snapshot.session) && sessionModes.current.get(snapshot.session) === recoverySession;
    try {
      snapshot = instance?.recoverySnapshot();
      const session = snapshot && sessionModes.current.get(snapshot.session);
      recoverySession = session;
      if (!snapshot || !session || session.mode !== "workspace" || session.uiId !== snapshot.id) throw new Error("열린 문서를 다시 확인해 주세요.");
      const completed = await recoverWorkspaceSource(session.documentId, snapshot.session, snapshot.bytes, action);
      if (!sameContext()) return;
      if (completed) {
        const current = instance?.isCurrentSnapshot(snapshot.session, snapshot.generation, snapshot.revision);
        const sameDocument = instance?.isCurrentSession(snapshot.session);
        const previous = action === "copy" ? "시작 당시 원문을 복사했습니다." : "시작 당시 원문을 파일에 내보냈습니다.";
        setRecoveryMessage(current ? (action === "copy" ? "원문을 복사했습니다." : "선택한 파일에 원문을 내보냈습니다.")
          : sameDocument ? `${previous} 현재 문서 상태를 다시 확인하세요.` : `이전에 열었던 문서의 ${previous}`);
      }
    } catch (cause) {
      if (snapshot && !sameContext()) return;
      const error = String(cause);
      if (/recovery_copy_not_utf8|recovery_copy_contains_nul/.test(error)) setRecoveryMessage("이 원문은 텍스트로 복사할 수 없습니다. 파일로 내보내세요.");
      else if (error.includes("recovery_too_large")) setRecoveryMessage("복구 원문이 64 MiB를 초과해 내보내거나 복사할 수 없습니다.");
      else if (error.includes("recovery_export_exists")) setRecoveryMessage("이미 있는 파일은 변경하지 않았습니다. 다른 이름으로 내보내세요.");
      else setRecoveryMessage(`원문 복구 작업에 실패했습니다: ${error}`);
      if (snapshot && instance?.isBlockedRecoverySnapshot(snapshot.session, snapshot.generation, snapshot.revision)) {
        try {
          const text = new TextDecoder("utf-8", { fatal: true }).decode(snapshot.bytes);
          setMemoryRecovery({ instance, id: snapshot.id, session: snapshot.session, generation: snapshot.generation, revision: snapshot.revision, registration, binding, sessionRef: recoverySession, sessionBinding: recoverySession?.binding, text });
        } catch {
          setRecoveryMessage("원문 복구 작업과 텍스트 표시가 실패했습니다. 현재 화면을 닫지 말고 저장을 다시 시도해 주세요.");
        }
      }
    } finally {
      recoveringRef.current = false;
      setRecovering(false);
    }
  }

  async function refreshWorkspace(startAfter: string | null = null): Promise<void> {
    const metadata = metadataVersion.current;
    const binding = JSON.stringify(workspaceBindingRef.current);
    const isCurrent = () => metadataVersion.current === metadata && JSON.stringify(workspaceBindingRef.current) === binding;
    try {
      const result = await workspace.current.loadInventory(workspaceInventory, startAfter);
      if (!workspace.current.isCurrentInventory(result.revision) || !isCurrent()) return;
      nodesRef.current = result.nodes;
      inventoryVersion.current = metadata;
      inventoryBinding.current = binding;
      setNodes(result.nodes);
      setWorkspacePageStart(startAfter);
      workspacePageStartRef.current = startAfter;
      setWorkspaceNextPage(result.nextAfterId);
      setInventoryError("");
    } catch (cause) {
      if (isCurrent() && !String(cause).includes("stale_workspace_list")) setInventoryError(String(cause));
    }
  }

  async function refreshWorkspaceSync(): Promise<void> {
    void refreshCredentialStatus();
    const registration = registrationVersion.current;
    const update = await workspace.current.readSync(workspaceSyncState, () => registration === registrationVersion.current && modeRef.current === "workspace");
    if (!update) return;
    const current = update.state;
    setWorkspaceConnecting(workspace.current.connecting);
    const key = JSON.stringify(current.binding);
    if (workspaceBindingRef.current && JSON.stringify(workspaceBindingRef.current) !== key) {
      setRestorePermission(null);
      setConflictSnapshot(null);
      resetDeletedCopy();
      setConflictMessage("");
      setHasContentConflict(false);
      setPendingConflict(null);
      setPendingMerge(null);
      setPendingCopy(null);
      setCopyRetryOpen(false);
      syncRevision.current++;
      const activeSession = [...sessionModes.current.keys()].find((session) => controller.current?.isCurrentSession(session));
      setSyncStatus(activeSession && unqueuedEdits.current.has(activeSession) ? "이전 등록의 미전송 편집" : "");
    }
    workspaceBindingRef.current = current.binding;
    setWorkspaceBinding(current.binding);
    transmitter.current?.activateBinding(key);
    switch (current.status) {
      case "ready":
        setWorkspaceSyncMessage("");
        transmitter.current?.resumeSync();
        void controller.current?.inspectRemoteChange();
        break;
      case "connecting": setWorkspaceSyncMessage("서버 동기화 중"); break;
      case "offline": setWorkspaceSyncMessage("서버에 연결되지 않음"); break;
      case "auth_required": setWorkspaceSyncMessage("기기 인증 필요"); break;
      case "gateway_auth_required": setWorkspaceSyncMessage("gateway 인증 필요"); break;
      case "needs_review": setWorkspaceSyncMessage("서버 상태 대조 필요"); break;
    }
    if (update.refreshInventory) await refreshWorkspace(workspacePageStartRef.current);
  }

  async function refreshCredentialStatus(): Promise<void> {
    if (!credentialMounted.current) return;
    const revision = ++credentialRevision.current;
    const registration = registrationVersion.current;
    try {
      const status = await workspaceCredentialStatus();
      if (credentialMounted.current && revision === credentialRevision.current && registration === registrationVersion.current) setCredentialMessage(credentialStateMessage(status));
    } catch {
      if (credentialMounted.current && revision === credentialRevision.current && registration === registrationVersion.current) setCredentialMessage("인증정보 저장 상태를 확인하지 못했습니다.");
    }
  }

  async function refreshDrafts(): Promise<void> {
    const result = await localDrafts();
    draftsRef.current = result;
    setDrafts(result);
  }

  async function openSearchResult(result: SearchResult): Promise<void> {
    const active = workspaceBindingRef.current;
    if (!active || modeRef.current !== "workspace") throw new Error("작업 공간을 다시 확인해 주세요.");
    const bindingKey = JSON.stringify(active);
    if (result.scope === "recovery") {
      const available = await localDrafts();
      if (!workspaceBindingRef.current || JSON.stringify(workspaceBindingRef.current) !== bindingKey) throw new Error("작업 공간이 변경됐습니다.");
      const draft = available.find((item) => item.document_id === result.node_id && JSON.stringify(item.binding) === bindingKey);
      if (!draft) throw new Error("초안이 변경됐습니다. 검색을 다시 시작하세요.");
      draftsRef.current = available;
      setDrafts(available);
      await controller.current?.open(draftSelectionId(draft, "workspace"));
      return;
    }
    const node = (result as SearchResult & { node: NodeState | null }).node;
    if (!node || node.node_id !== result.node_id || node.deleted || node.kind !== "file") throw new Error("문서가 변경됐습니다. 검색을 다시 시작하세요.");
    searchNodesRef.current.set(node.node_id, node);
    try { await controller.current?.open(node.node_id); }
    finally { if (searchNodesRef.current.get(node.node_id) === node) searchNodesRef.current.delete(node.node_id); }
  }

  async function refreshSyncStatus(): Promise<void> {
    const revision = ++syncRevision.current;
    const actionVersion = conflictActionRevision.current;
    const current = [...sessionModes.current.entries()].find(([session]) => controller.current?.isCurrentSession(session));
    if (!current || !current[1].binding) return;
    const [sessionId, session] = current;
    const binding = session.binding;
    if (!binding) return;
    const bindingKey = JSON.stringify(binding), version = registrationVersion.current;
    const result = await workspaceDocumentOutbox(binding, session.documentId);
    if (revision !== syncRevision.current || actionVersion !== conflictActionRevision.current || version !== registrationVersion.current || !workspaceBindingRef.current || JSON.stringify(workspaceBindingRef.current) !== bindingKey || !controller.current?.isCurrentSession(sessionId)) return;
    setHasContentConflict(result.has_conflict);
    if (result.has_conflict) setSyncStatus("충돌 있음");
    else if (result.has_failure || result.latest?.frozen) setSyncStatus("서버 결과 대조 필요");
    else {
      const last = result.latest;
      const unqueued = unqueuedEdits.current.has(sessionId);
      if (!last) setSyncStatus(unqueued ? "서버 반영 대기" : "");
      else if (last.status === "conflict") setSyncStatus(unqueued ? "서버 반영 대기" : "");
      else if (last.status === "abandoned") {
        setSyncStatus(BigInt(last.operation_seq) <= BigInt(result.confirmed_through) ? (unqueued ? "서버 반영 대기" : "") : (unqueued ? "서버 반영 대기, 취소 영수증 확인 중" : "취소됨, 영수증 확인 중"));
      }
      else if (last.status === "committed" && controller.current?.isCurrentSaved(sessionId, last.generation)) {
        setSyncStatus(BigInt(last.operation_seq) <= BigInt(result.confirmed_through) ? "서버 반영됨" : "서버 저장됨, 영수증 확인 중");
      }
      else setSyncStatus("서버 반영 대기");
    }
    await observeConflictStatus({ result, binding, session, sessionId, version, revision, actionVersion });
  }

  async function connect(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (connecting.current) return;
    const form = event.currentTarget;
    const fields = new FormData(form);
    const origin = String(fields.get("origin") ?? "").trim();
    const workspaceId = String(fields.get("workspace_id") ?? "").trim().toLowerCase();
    const secret = String(fields.get("pairing_secret") ?? "");
    const secretInput = form.elements.namedItem("pairing_secret");
    if (secretInput instanceof HTMLInputElement) secretInput.value = "";
    connecting.current = true;
    registrationVersion.current++;
    workspace.current.invalidate();
    setInventoryError("");
    setBusy(true);
    setConnectionError("");
    try {
      if (!await controller.current?.prepareForWorkspaceChange()) return;
      await pairWorkspace(origin, workspaceId, secret);
      registrationVersion.current++;
      workspaceBindingRef.current = null;
      setWorkspaceBinding(null);
      setRestorePermission(null);
      modeRef.current = "workspace";
      nodesRef.current = [];
      setNodes([]);
      setWorkspacePageStart(null);
      workspacePageStartRef.current = null;
      setWorkspaceNextPage(null);
      setMode("workspace");
      await refreshWorkspaceSync();
      await refreshDrafts();
      await refreshWorkspace();
    } catch (cause) {
      if (String(cause).includes("pairing_pending_adoption") || String(cause).includes("pairing_in_progress")) setPendingPair(true);
      setConnectionError(String(cause));
    } finally {
      connecting.current = false;
      setBusy(false);
      void refreshCredentialStatus();
    }
  }

  async function resumePair(discard: boolean): Promise<void> {
    if (connecting.current) return;
    connecting.current = true;
    registrationVersion.current++;
    workspace.current.invalidate();
    setInventoryError("");
    setBusy(true);
    setConnectionError("");
    try {
      if (!await controller.current?.prepareForWorkspaceChange()) return;
      if (discard) {
        await discardPendingPair();
      } else {
        await adoptPendingPair();
        registrationVersion.current++;
        workspaceBindingRef.current = null;
        setWorkspaceBinding(null);
        setRestorePermission(null);
        setPendingPair(false);
        modeRef.current = "workspace";
        nodesRef.current = [];
        setNodes([]);
        setWorkspacePageStart(null);
        workspacePageStartRef.current = null;
        setWorkspaceNextPage(null);
        setMode("workspace");
        await refreshWorkspaceSync();
        await refreshDrafts();
        await refreshWorkspace();
      }
      setPendingPair(false);
    } catch (cause) {
      setConnectionError(String(cause));
    } finally {
      connecting.current = false;
      setBusy(false);
      void refreshCredentialStatus();
    }
  }

  useEffect(() => {
    const operations = new Map<string, { scope: TransmitScope; documentId: string; generation: string }>();
    let operationsBinding: string | undefined;
    const scheduler = new AutoTransmit({
      queue: async (scope, generation) => {
        const version = registrationVersion.current;
        const queued = await queueWorkspaceDocument(scope.documentId, scope.editSessionId, generation);
        const currentScope = sessionModes.current.get(scope.editSessionId)?.scope;
        if (version !== registrationVersion.current || !workspaceBindingRef.current || JSON.stringify(workspaceBindingRef.current) !== scope.binding
          || !currentScope || currentScope.binding !== scope.binding || currentScope.documentId !== scope.documentId || currentScope.editSessionId !== scope.editSessionId) throw new Error("stale_registration");
        if (!queued.operation_id || queued.document_id !== scope.documentId || queued.generation !== generation || typeof queued.committed !== "boolean") throw new Error("invalid_queued_operation");
        if (unqueuedEdits.current.get(scope.editSessionId) === queued.generation) unqueuedEdits.current.delete(scope.editSessionId);
        if (queued.committed) setTimeout(() => scheduler.kick(), 0);
        if (!queued.committed && operationsBinding === scope.binding) operations.set(queued.operation_id, { scope, documentId: queued.document_id, generation: queued.generation });
        if (controller.current?.isCurrentSession(scope.editSessionId)) { syncRevision.current++; setSyncStatus(unqueuedEdits.current.has(scope.editSessionId) ? "서버 반영 대기" : "동기화 대기"); }
        return { operationId: queued.operation_id, documentId: queued.document_id, generation: queued.generation, committed: queued.committed };
      },
      transmit: async () => {
        const version = registrationVersion.current;
        const bindingAtStart = workspaceBindingRef.current && JSON.stringify(workspaceBindingRef.current);
        const controllerAtStart = controller.current;
        const sessionAtStart = [...sessionModes.current.keys()].find((session) => controllerAtStart?.isCurrentSession(session));
        let result;
        try { result = await sendNextWorkspaceOperation(); }
        catch (cause) {
          if (/OutboxBlocked|ReconciliationRequired/.test(String(cause))) {
            const currentSession = [...sessionModes.current.keys()].find((session) => controller.current?.isCurrentSession(session));
            if (version === registrationVersion.current && bindingAtStart === (workspaceBindingRef.current && JSON.stringify(workspaceBindingRef.current))
              && controller.current === controllerAtStart && sessionAtStart === currentSession) setSyncStatus("서버 결과 대조 필요");
            return { status: "needs_review" as const };
          }
          throw cause;
        }
        const binding = JSON.stringify(result.binding);
        const activeBindings = [...sessionModes.current.values()].flatMap((session) => session.scope ? [session.scope.binding] : []);
        const stale = staleTransmissionResult(version, registrationVersion.current, binding, activeBindings);
        if (stale) return stale;
        if (workspaceBindingRef.current && JSON.stringify(workspaceBindingRef.current) !== binding) return { status: "pending" as const };
        if (result.generation === null && result.operation_id) {
          setRestoreRevision((current) => current + 1);
          if (result.status === "committed" || result.status === "failed" || result.status === "abandoned") {
            setRestorePermission((current) => current?.binding === binding && current.operationId === result.operation_id ? null : current);
          }
        }
        const operation = result.operation_id ? operations.get(result.operation_id) : undefined;
        const matchingOperation = operation && operation.scope.binding === binding && result.document_id === operation.documentId && result.generation === operation.generation;
        if (matchingOperation) {
          if (result.status === "committed") scheduler.acknowledge(operation.scope, { operationId: result.operation_id!, documentId: operation.documentId, generation: operation.generation }, true);
          if (controller.current?.isCurrentSession(operation.scope.editSessionId)) {
            if (result.status === "conflict") { setHasContentConflict(true); setSyncStatus("충돌 있음"); }
            else if (result.status === "failed" || result.status === "needs_review") setSyncStatus("서버 결과 대조 필요");
            else if (result.status === "ack_pending" || result.status === "pending") setSyncStatus("서버 결과 확인 중");
          }
          if (result.status === "committed" || result.status === "conflict" || result.status === "failed" || result.status === "abandoned") operations.delete(result.operation_id!);
        }
        if (result.status !== "pending") {
          void refreshSyncStatus().catch((cause) => setConnectionError(String(cause)));
          requestRemoteInspection();
        }
        if (result.status === "committed" || result.status === "abandoned" || result.status === "conflict" || result.status === "failed" || result.status === "idle") setCopyRevision((revision) => revision + 1);
        if (result.status === "ack_pending" || result.status === "auth_required" || result.status === "gateway_auth_required") return receiptWaitResult(result.status, result, binding, operation);
        if (result.status === "needs_review" && result.content_block && result.document_id) {
          return { status: "content_blocked" as const, binding, operationId: result.content_block.blocked_operation_id, documentId: result.document_id };
        }
        if (result.status === "permission_denied") {
          if (!result.operation_id) throw new Error("잘못된 권한 거절 응답입니다.");
          return { status: "permission_denied" as const, operationId: result.operation_id };
        }
        if (result.status === "idle" || result.status === "pending" || result.status === "needs_review" || result.status === "sync_pending") return { status: result.status };
        if (!result.operation_id) throw new Error("잘못된 서버 저장 응답입니다.");
        return { status: result.status, operationId: result.operation_id };
      },
      acknowledged: (scope, generation, current) => {
        if (current && controller.current?.isCurrentSaved(scope.editSessionId, generation)) {
          syncRevision.current++;
          void refreshSyncStatus().catch((cause) => setConnectionError(String(cause)));
        }
      },
      receiptConfirmed: (operationId) => { operations.delete(operationId); },
      authRequired: (reason) => {
        setConnectionError(reason === "auth_required" ? "기기 인증이 만료되었거나 취소됐습니다. 다시 등록해 주세요. 기기 초안은 보존됩니다." : "gateway 인증을 확인해 주세요. 기기 초안은 보존됩니다.");
      },
      permissionDenied: (operationId) => {
        const binding = workspaceBindingRef.current;
        if (binding) setRestorePermission({ binding: JSON.stringify(binding), operationId });
        setRestoreRevision((current) => current + 1);
      },
      failed: (cause) => { if (!(cause instanceof QueueStorageError)) setConnectionError(String(cause)); },
      queueStorageFailed: (scope) => {
        if (controller.current?.isCurrentSession(scope.editSessionId)) reportQueueError(scope, "기기 초안은 저장됐지만 전송을 준비하지 못했습니다. 자동으로 다시 시도합니다.");
      },
      queueRecovered: (scope) => {
        setQueueErrors((errors) => clearQueueError(errors, scope));
      },
    });
    const instance = new EditorController({
      openDocument: async (id) => {
        const local = draftsRef.current.find((draft) => draftSelectionId(draft, modeRef.current) === id);
        const selectedMode = local ? "workspace" : modeRef.current;
        const node = selectedMode === "workspace" ? searchNodesRef.current.get(id) ?? nodesRef.current.find((item) => item.node_id === id && item.kind === "file") : undefined;
        if (selectedMode === "workspace" && !node && !local) throw new Error("문서 목록을 다시 불러와 주세요.");
        let opened;
        try {
          opened = local ? await openLocalDraft(local) : selectedMode === "fixture" ? await openDocument(id) : await openWorkspaceDocument(node!);
        } catch (cause) {
          if (selectedMode !== "workspace") throw cause;
          if (String(cause).includes("pairing_in_progress")) setPendingPair(true);
          throw new Error(workspaceOpenError(cause), { cause });
        }
        if (selectedMode === "workspace") { searchNodesRef.current.clear(); setSearchRevision((current) => current + 1); }
        const canSync = selectedMode === "workspace" && "binding" in opened && "readonly" in opened && !opened.readonly && "has_session" in opened && opened.has_session === true && (!local || (local.current_registration && local.has_session));
        const scope = canSync && "binding" in opened ? { binding: JSON.stringify(opened.binding), documentId: opened.document_id, editSessionId: opened.edit_session_id } : undefined;
        const binding = "binding" in opened ? opened.binding as WorkspaceRegistration : undefined;
        const orphanDraft = !!local && !nodesRef.current.some((item) => item.node_id === opened.document_id && item.kind === "file" && !item.deleted);
        sessionModes.current.set(opened.edit_session_id, { mode: selectedMode, documentId: opened.document_id, uiId: id, scope, binding, orphanDraft });
        setSyncStatus("");
        return { ...opened, document_id: id };
      },
      saveDocument: async (request) => {
        const session = sessionModes.current.get(request.edit_session_id);
        if (!session) throw new Error("편집 세션을 다시 열어 주세요.");
        const original = { ...request, document_id: session.documentId };
        const saved = session.mode === "fixture" ? await saveDocument(original) : await saveWorkspaceDocument(original);
        if (saved.edit_session_id !== request.edit_session_id || saved.generation !== request.generation) throw new Error("저장 응답이 현재 편집 요청과 일치하지 않습니다.");
        if (session.mode === "workspace") { searchNodesRef.current.clear(); setSearchRevision((current) => current + 1); }
        try {
          if (session.scope) scheduler.durable(session.scope, saved.generation);
          else scheduler.unscopedDurable(saved.edit_session_id, saved.generation);
        } catch (cause) { setConnectionError(String(cause)); }
        return saved;
      },
      holdConflictSession: async (id, sessionId) => {
        const session = sessionModes.current.get(sessionId);
        if (!session?.scope || session.uiId !== id) throw new Error("편집 세션을 다시 확인해 주세요.");
        await scheduler.holdConflictSession(session.scope);
      },
      releaseConflictSession: (id, sessionId) => {
        const session = sessionModes.current.get(sessionId);
        if (!session?.scope || session.uiId !== id) throw new Error("편집 세션을 다시 확인해 주세요.");
        scheduler.releaseConflictSession(session.scope);
      },
      retireConflictSession: (id, sessionId) => {
        const session = sessionModes.current.get(sessionId);
        if (!session?.scope || session.uiId !== id) throw new Error("편집 세션을 다시 확인해 주세요.");
        scheduler.retireConflictSession(session.scope);
        sessionModes.current.delete(sessionId);
        unqueuedEdits.current.delete(sessionId);
      },
      closeDocument: async (request) => {
        const session = sessionModes.current.get(request.edit_session_id);
        if (!session) throw new Error("편집 세션을 다시 열어 주세요.");
        const original = { ...request, document_id: session.documentId };
        const close = () => session.mode === "fixture" ? closeDocument(original) : closeWorkspaceDocument(original);
        if (session.scope) {
          let queued = true;
          try { await scheduler.flushDocument(session.scope); }
          catch (cause) {
            if (/checkpoint_pending|composition_in_progress/.test(String(cause))) throw cause;
            queued = false;
            if (cause instanceof QueueStorageError) reportQueueError(session.scope, "기기 초안은 저장됐지만 전송을 준비하지 못했습니다. 초안을 다시 열어 전송해 주세요.");
            else setConnectionError(`기기 초안은 저장됐지만 전송을 준비하지 못했습니다: ${String(cause)}`);
          }
          if (queued) await scheduler.deactivate(session.scope, close);
          else await scheduler.deactivatePreservingDraft(session.scope, close);
        } else await close();
        sessionModes.current.delete(request.edit_session_id);
        unqueuedEdits.current.delete(request.edit_session_id);
      },
      exit: async () => {
        for (const session of sessionModes.current.values()) if (session.scope) {
          try { await scheduler.flushDocument(session.scope); }
          catch (cause) {
            if (/checkpoint_pending|composition_in_progress/.test(String(cause))) throw cause;
            if (!(cause instanceof QueueStorageError)) setConnectionError(`기기 초안은 저장됐지만 전송을 준비하지 못했습니다: ${String(cause)}`);
          }
        }
        await invoke<void>("m0_exit");
      },
      stateChanged: (next) => {
        setState(next);
        if (selectedRef.current !== next.selected) {
          selectedRef.current = next.selected;
          resetDeletedCopy();
          setHasContentConflict(false); setConflictSnapshot(null); setConflictMessage("");
          setPendingConflict(null);
          setPendingMerge(null);
          setPendingCopy(null);
          setCopyRetryOpen(false);
          setPreviewAnchor(null);
          setRecoveryMessage("");
          syncRevision.current++;
          if (next.selected) void refreshSyncStatus().catch((cause) => setConnectionError(String(cause)));
        }
        if (next.error.includes("pairing_in_progress")) setPendingPair(true);
      },
      sourceChanged: (text, documentChanged) => {
        source.current = text;
        clearTimeout(previewTimer.current);
        if (previewEnabled.current) {
          if (documentChanged) setPreviewSource(text);
          else previewTimer.current = setTimeout(() => setPreviewSource(source.current), 400);
        }
      },
      edited: (_id, sessionId, generation) => {
        setConflictSnapshot(null);
        editedDeletedCopy(sessionId);
        setPendingConflict((pending) => pending?.sessionId === sessionId ? null : pending);
        const scope = sessionModes.current.get(sessionId)?.scope;
        try {
          if (scope) {
            unqueuedEdits.current.set(sessionId, generation); syncRevision.current++;
            if (!workspaceBindingRef.current || JSON.stringify(workspaceBindingRef.current) !== scope.binding) {
              setSyncStatus("이전 등록의 미전송 편집");
              setConnectionError("작업 공간 등록이 변경됐습니다. 기기 초안을 확인하세요.");
            } else {
              setSyncStatus("서버 반영 대기");
              scheduler.edit(scope, generation);
            }
          }
          else scheduler.unscopedEdit(sessionId, generation);
        } catch (cause) { setConnectionError(String(cause)); }
      },
      compositionChanged: (_id, sessionId, active) => {
        const scope = sessionModes.current.get(sessionId)?.scope;
        try {
          if (scope) scheduler.composition(scope, active);
          else scheduler.inputComposition(active);
        } catch (cause) { setConnectionError(String(cause)); }
        if (!active) queueMicrotask(() => requestRemoteRefresh());
      },
      opened: (_id, sessionId, generation, localDraft, conflictRecovery) => {
        const session = sessionModes.current.get(sessionId);
        const scope = session?.scope;
        if (scope) {
          if (operationsBinding !== scope.binding) {
            operations.clear();
            operationsBinding = scope.binding;
          }
          scheduler.activateBinding(scope.binding);
          scheduler.openSession(scope);
          if (localDraft && !conflictRecovery) {
            if (!restoreDeletedCopy(sessionId, generation, scheduler)) {
              void scheduler.restore(scope, generation).catch((cause) => { if (!(cause instanceof QueueStorageError)) setConnectionError(String(cause)); });
            }
          }
        }
        queueMicrotask(() => { requestRemoteRefresh(); requestRemoteInspection(); });
      },
      prepareRemoteRefresh: async (id, sessionId) => {
        const session = sessionModes.current.get(sessionId);
        if (!session || session.mode !== "workspace" || !session.scope || session.uiId !== id) return null;
        return prepareWorkspaceRemoteRefresh(session.documentId, sessionId);
      },
      commitRemoteRefresh: async (id, sessionId, token) => {
        const session = sessionModes.current.get(sessionId);
        if (!session || session.mode !== "workspace" || !session.scope || session.uiId !== id) return false;
        return commitWorkspaceRemoteRefresh(session.documentId, sessionId, token);
      },
      inspectRemoteChange: async (id, sessionId, generation) => {
        const session = sessionModes.current.get(sessionId);
        if (!session || session.mode !== "workspace" || !session.scope || session.uiId !== id) return "pending";
        return inspectWorkspaceRemoteChange(session.documentId, sessionId, generation);
      },
      remoteInspectionVersion: () => `${registrationVersion.current}:${JSON.stringify(workspaceBindingRef.current)}`,
      manualSync: async (id, sessionId) => {
        const session = sessionModes.current.get(sessionId);
        if (!session || session.uiId !== id) throw new Error("편집 세션을 다시 열어 주세요.");
        if (session.scope) {
          try { await scheduler.syncNow(session.scope); }
          catch (cause) { if (!(cause instanceof QueueStorageError)) throw cause; }
        }
      },
      createEditor: ({ source, change, inputRejected, composition, save }) => {
        const editing = new Compartment();
        let compositionTimer: ReturnType<typeof setTimeout> | undefined;
        let compositionPending = false;
        let replacing = false;
        const createState = (current: typeof source, selection?: { anchor: number; head: number }) => EditorState.create({
            doc: current.text,
            selection,
            extensions: [
              lineNumbers(), history(), drawSelection(),
              keymap.of([{ key: "Mod-s", run: () => { save(); return true; } }, ...defaultKeymap, ...historyKeymap]),
              editing.of([EditorState.readOnly.of(current.readonly), EditorView.editable.of(!current.readonly)]),
              ...(current.readonly ? [] : [documentSizeGuard(current.bom, inputRejected)]),
              EditorView.contentAttributes.of({ "aria-label": "Markdown 원문" }),
              EditorView.domEventHandlers({
                compositionstart: () => { clearTimeout(compositionTimer); compositionPending = true; composition(true); return false; },
                compositionend: () => { clearTimeout(compositionTimer); compositionTimer = setTimeout(() => { compositionPending = false; composition(false); }, 250); return false; },
              }),
              EditorView.updateListener.of((update) => {
                if (update.docChanged && !replacing) change(update.state.doc.toString());
              }),
              ...(current.original.length > 1024 * 1024 ? [] : [markdown()]),
            ],
          });
        const view = new EditorView({ state: createState(source) });
        return {
          show: () => {
            if (!parent.current) throw new Error("편집기 화면이 닫혔습니다.");
            parent.current.replaceChildren(view.dom);
          },
          destroy: () => { clearTimeout(compositionTimer); view.destroy(); },
          setEditable: (enabled) => view.dispatch({ effects: editing.reconfigure([EditorState.readOnly.of(!enabled), EditorView.editable.of(enabled)]) }),
          isComposing: () => view.composing || compositionPending,
          replaceSource: (current) => {
            const selection = view.state.selection.main;
            const anchor = Math.min(selection.anchor, current.text.length);
            const head = Math.min(selection.head, current.text.length);
            const scrollTop = view.scrollDOM.scrollTop;
            const scrollLeft = view.scrollDOM.scrollLeft;
            const focused = view.hasFocus;
            replacing = true;
            try {
              view.setState(createState(current, { anchor, head }));
              view.scrollDOM.scrollTop = scrollTop;
              view.scrollDOM.scrollLeft = scrollLeft;
              if (focused) view.focus();
            } finally { replacing = false; }
          },
        };
      },
    });
    controller.current = instance;
    transmitter.current = scheduler;
    let disposed = false;
    function requestRemoteRefresh() {
      if (disposed || modeRef.current !== "workspace") return;
      void instance.refreshRemote().catch(() => { if (!disposed) setConnectionError("최신 문서를 확인하지 못했습니다."); });
    }
    function requestRemoteInspection() {
      if (!disposed && modeRef.current === "workspace") void instance.inspectRemoteChange();
    }
    credentialMounted.current = true;
    const stop = listen("close-requested", () => { void instance.close(); });
    const syncReady = listen("workspace-sync-status-changed", () => {
      if (modeRef.current === "workspace") void refreshWorkspaceSync().catch((cause) => setConnectionError(String(cause)));
    });
    const credentialChanged = listen("workspace-credential-status-changed", () => { void refreshCredentialStatus(); }).catch(() => {
      if (credentialMounted.current) setCredentialMessage("인증정보 저장 상태 알림을 연결하지 못했습니다.");
      return () => {};
    });
    void refreshCredentialStatus();
    const credentialTimer = window.setInterval(() => { void refreshCredentialStatus(); }, 60_000);
    const metadataChanged = listen("workspace-metadata-changed", () => {
      if (modeRef.current === "workspace") {
        metadataVersion.current++;
        requestRemoteRefresh();
        requestRemoteInspection();
        void refreshSyncStatus().catch((cause) => setConnectionError(String(cause)));
        searchNodesRef.current.clear();
        setSearchRevision((current) => current + 1);
        void refreshWorkspace(workspacePageStartRef.current);
      }
    });
    void (async () => {
      try {
        await Promise.all([syncReady, metadataChanged]);
        await refreshDrafts();
        await workspaceSyncState();
        if (!disposed) {
          modeRef.current = "workspace";
          setMode("workspace");
          await refreshWorkspaceSync();
          await refreshWorkspace();
        }
      } catch (cause) {
        if (!disposed && String(cause).includes("not_connected")) {
          await instance.open("welcome");
          if (draftsRef.current.length) setConnectionError("이전에 저장한 기기 초안을 열 수 있습니다.");
        }
        else if (!disposed && !String(cause).includes("stale_workspace_list")) {
          modeRef.current = "workspace";
          setMode("workspace");
          if (String(cause).includes("pairing_in_progress")) setPendingPair(true);
          setConnectionError(String(cause));
        }
      } finally {
        if (!disposed) setBusy(false);
      }
    })();
    return () => {
      disposed = true;
      credentialMounted.current = false;
      window.clearInterval(credentialTimer);
      credentialRevision.current++;
      workspace.current.invalidate();
      controller.current = null;
      transmitter.current = null;
      clearTimeout(previewTimer.current);
      void instance.dispose().then(() => scheduler.dispose()).catch(() => {});
      void stop.then((unlisten) => unlisten()).catch(() => {});
      for (const listener of [syncReady, metadataChanged, credentialChanged]) {
        void listener.then((unlisten) => unlisten()).catch(() => {});
      }
    };
  }, []);

  useLayoutEffect(() => {
    const keydown = (event: KeyboardEvent) => {
      if ((event.ctrlKey || event.metaKey) && event.shiftKey && event.key.toLowerCase() === "f" && mode === "workspace" && workspaceBinding) {
        event.preventDefault();
        setSearchOpen(true);
      }
    };
    window.addEventListener("keydown", keydown);
    return () => window.removeEventListener("keydown", keydown);
  }, [mode, workspaceBinding]);

  const loadPreviewImage = useCallback((target: string, signal: AbortSignal): Promise<Uint8Array> => {
    if (mode !== "workspace") return Promise.reject(new Error("작업 공간에 연결되어 있지 않습니다."));
    const found = [...sessionModes.current.entries()].find(([, session]) => session.uiId === state.selected && session.mode === "workspace");
    const resolved = found && resolveWorkspaceReference(nodesRef.current, found[1].documentId, target);
    if (!found || !resolved) return Promise.reject(new Error("이미지를 불러올 수 없습니다."));
    const [sessionId, session] = found;
    return imageQueue.current.load(() => {
      if (sessionModes.current.get(sessionId) !== session) throw new Error("편집 세션이 변경되었습니다.");
      return previewWorkspaceImage(session.documentId, sessionId, resolved.node);
    }, signal);
  }, [mode, state.selected, nodes]);

  useEffect(() => {
    let disposed = false;
    let timer: ReturnType<typeof setTimeout> | undefined;
    const check = async () => {
      try {
        const status = await pendingPairStatus();
        if (disposed) return;
        setPendingPair(status === "pending_adoption");
        if (status === "in_progress") timer = setTimeout(() => void check(), 1000);
      } catch (cause) {
        if (!disposed) setConnectionError(String(cause));
      }
    };
    void check();
    return () => { disposed = true; clearTimeout(timer); };
  }, []);

  const nodeIndex = useMemo(() => new Map(nodes.map((node) => [node.node_id, node])), [nodes]);
  const workspaceButtons = useMemo(() => nodes.filter((node) => node.kind === "file" && !node.deleted).map((node) => {
    const path = workspacePath(node, nodeIndex);
    return <div key={node.node_id} className="workspace-file-row"><button disabled={busy || workspaceConnecting} data-document={node.node_id} aria-current={state.selected === node.node_id ? "page" : undefined} onClick={() => void controller.current?.open(node.node_id)}>{path}</button><button disabled={busy || moveTarget !== null || moveRequestsOpen} data-move-node={node.node_id} aria-label={`${path} 이름 변경 또는 이동`} onClick={() => setMoveTarget({ node, path })}>이동</button><button disabled={busy || deleteTarget !== null || deleteRequestsOpen} data-delete-node={node.node_id} aria-label={`${path} 삭제`} onClick={() => setDeleteTarget({ node, path })}>삭제</button></div>;
  }), [nodes, nodeIndex, busy, workspaceConnecting, state.selected, deleteTarget, deleteRequestsOpen, moveTarget, moveRequestsOpen]);
  const hasWorkspaceDocument = [...sessionModes.current.values()].some((session) => session.mode === "workspace" && session.uiId === state.selected);
  const visibleSyncStatus = state.remoteDeleted && (syncStatus === "서버 반영됨" || syncStatus === "서버 저장됨, 영수증 확인 중") ? "" : syncStatus;

  return <main>
    <header><strong>Notryx</strong><span className="badge">{mode === "fixture" ? "M0" : "M2"}</span><span className="caption">{mode === "fixture" ? "로컬 편집 시험" : "작업 공간"}</span></header>
    <div className="layout">
      <nav aria-label="문서 탐색">
        <form className="workspace-connect" onSubmit={(event) => void connect(event)}>
          <label>서버 주소<input name="origin" type="url" placeholder="https://example.org" required /></label>
          <label>작업 공간 ID<input name="workspace_id" required /></label>
          <label>등록 비밀<input name="pairing_secret" type="password" autoComplete="off" required /></label>
          <button type="submit" disabled={busy}>작업 공간 연결</button>
        </form>
        {credentialMessage && <p role="status" data-testid="credential-status">{credentialMessage}</p>}
        {pendingPair && <div><button disabled={busy} onClick={() => void resumePair(false)}>대기 중인 등록 이어가기</button><button disabled={busy} onClick={() => void resumePair(true)}>대기 중인 등록 버리기</button></div>}
        <p>{mode === "fixture" ? "합성 문서" : "작업 공간 문서"}</p>
        {mode === "fixture" ? documents.map(([id, label]) => <button key={id} disabled={busy} data-document={id} aria-current={state.selected === id ? "page" : undefined} onClick={() => void controller.current?.open(id)}>{label}</button>)
          : workspaceButtons}
        {mode === "workspace" && workspaceBinding && <button data-testid="move-requests-toggle" disabled={busy} aria-expanded={moveRequestsOpen} onClick={() => { setMoveTarget(null); setMoveRequestsOpen((current) => !current); }}>이동 요청</button>}
        {mode === "workspace" && workspaceBinding && moveTarget && <MovePanel key={`${moveTarget.node.node_id}:${moveTarget.node.meta_etag}`} binding={workspaceBinding} node={moveTarget.node} displayName={moveTarget.path} ports={movePorts} revision={restoreRevision} treeRevision={searchRevision} permissionDeniedOperationId={restorePermission?.binding === JSON.stringify(workspaceBinding) ? restorePermission.operationId : null} onClose={() => setMoveTarget(null)} onWorkReady={() => {}} onRecheck={(operationId) => {
          const key = JSON.stringify(workspaceBinding);
          if (!workspaceBindingRef.current || JSON.stringify(workspaceBindingRef.current) !== key) return;
          if (!transmitter.current?.resumeRestoreAfterCancel(key, operationId)) transmitter.current?.kick();
        }} />}
        {mode === "workspace" && workspaceBinding && moveRequestsOpen && <MovePanel binding={workspaceBinding} node={null} ports={movePorts} revision={restoreRevision} treeRevision={searchRevision} permissionDeniedOperationId={restorePermission?.binding === JSON.stringify(workspaceBinding) ? restorePermission.operationId : null} onClose={() => setMoveRequestsOpen(false)} onWorkReady={() => {}} onRecheck={(operationId) => {
          const key = JSON.stringify(workspaceBinding);
          if (!workspaceBindingRef.current || JSON.stringify(workspaceBindingRef.current) !== key) return;
          if (!transmitter.current?.resumeRestoreAfterCancel(key, operationId)) transmitter.current?.kick();
        }} />}
        {mode === "workspace" && workspaceBinding && deleteTarget && <DeletePanel key={`${deleteTarget.node.node_id}:${deleteTarget.node.node_etag}`} binding={workspaceBinding} node={deleteTarget.node} displayName={deleteTarget.path} ports={deletePorts} revision={restoreRevision} permissionDeniedOperationId={restorePermission?.binding === JSON.stringify(workspaceBinding) ? restorePermission.operationId : null} onClose={() => setDeleteTarget(null)} onWorkReady={() => { setRestoreRevision((current) => current + 1); transmitter.current?.kick(); }} onRecheck={(operationId) => {
          const binding = JSON.stringify(workspaceBinding);
          if (!workspaceBindingRef.current || JSON.stringify(workspaceBindingRef.current) !== binding) return;
          if (!transmitter.current?.resumeRestoreAfterCancel(binding, operationId)) transmitter.current?.kick();
        }} />}
        {mode === "workspace" && workspaceBinding && <button data-testid="delete-requests-toggle" disabled={busy || deleteTarget !== null} aria-expanded={deleteRequestsOpen} onClick={() => setDeleteRequestsOpen((current) => !current)}>삭제 요청</button>}
        {mode === "workspace" && workspaceBinding && deleteRequestsOpen && <DeletePanel binding={workspaceBinding} node={null} ports={deletePorts} revision={restoreRevision} permissionDeniedOperationId={restorePermission?.binding === JSON.stringify(workspaceBinding) ? restorePermission.operationId : null} onClose={() => setDeleteRequestsOpen(false)} onWorkReady={() => { setRestoreRevision((current) => current + 1); transmitter.current?.kick(); }} onRecheck={(operationId) => {
          const binding = JSON.stringify(workspaceBinding);
          if (!workspaceBindingRef.current || JSON.stringify(workspaceBindingRef.current) !== binding) return;
          if (!transmitter.current?.resumeRestoreAfterCancel(binding, operationId)) transmitter.current?.kick();
        }} />}
        {mode === "workspace" && <button disabled={busy} onClick={() => void refreshWorkspace()}>목록 새로고침</button>}
        {mode === "workspace" && (workspacePageStart !== null || workspaceNextPage !== null) && <p>목록을 나누어 표시합니다. 다른 구간에 있는 폴더나 첨부는 경로와 링크가 표시되지 않을 수 있습니다.</p>}
        {mode === "workspace" && workspacePageStart !== null && <button disabled={busy} onClick={() => void refreshWorkspace()}>처음 목록</button>}
        {mode === "workspace" && workspaceNextPage !== null && <button disabled={busy} onClick={() => void refreshWorkspace(workspaceNextPage)}>다음 목록</button>}
        {mode === "workspace" && workspaceBinding && <button disabled={busy} aria-expanded={searchOpen} onClick={() => setSearchOpen((current) => !current)}>본문 검색</button>}
        {mode === "workspace" && workspaceBinding && searchOpen && <SearchPanel revision={searchRevision} binding={workspaceBinding} ports={searchPorts} open={(result) => void openSearchResult(result).catch((cause) => { setConnectionError(String(cause)); setSearchRevision((current) => current + 1); })} label={(result) => {
          const node = (result as SearchResult & { node: NodeState | null }).node;
          return node ? workspacePath(node, nodeIndex) : result.node_id;
        }} />}
        {mode === "workspace" && workspaceBinding && <button disabled={busy} aria-expanded={createOpen} onClick={() => setCreateOpen((current) => !current)}>새 폴더 또는 문서</button>}
        {mode === "workspace" && workspaceBinding && createOpen && <CreatePanel binding={workspaceBinding} ports={createPorts} revision={restoreRevision} permissionDeniedOperationId={restorePermission?.binding === JSON.stringify(workspaceBinding) ? restorePermission.operationId : null} onWorkReady={() => { setRestoreRevision((current) => current + 1); transmitter.current?.kick(); }} onRecheck={(operationId) => {
          const binding = JSON.stringify(workspaceBinding);
          if (!workspaceBindingRef.current || JSON.stringify(workspaceBindingRef.current) !== binding) return;
          if (!transmitter.current?.resumeRestoreAfterCancel(binding, operationId)) transmitter.current?.kick();
        }} />}
        {mode === "workspace" && workspaceBinding && <button disabled={busy} aria-expanded={frozenCreateOpen} onClick={() => setFrozenCreateOpen((current) => !current)}>이전 생성 요청 대조</button>}
        {mode === "workspace" && workspaceBinding && frozenCreateOpen && <FrozenCreateReview binding={workspaceBinding} ports={frozenCreatePorts} onApplied={() => undefined} />}
        {mode === "workspace" && workspaceBinding && <button disabled={busy} aria-expanded={frozenDeleteOpen} onClick={() => setFrozenDeleteOpen((current) => !current)}>이전 삭제 요청 대조</button>}
        {mode === "workspace" && workspaceBinding && frozenDeleteOpen && <FrozenDeleteReview binding={workspaceBinding} ports={frozenDeletePorts} onApplied={() => undefined} />}
        {mode === "workspace" && workspaceBinding && <button disabled={busy} aria-expanded={frozenMoveOpen} onClick={() => setFrozenMoveOpen((current) => !current)}>이전 이동 요청 대조</button>}
        {mode === "workspace" && workspaceBinding && frozenMoveOpen && <FrozenMoveReview binding={workspaceBinding} ports={frozenMovePorts} onApplied={() => undefined} />}
        {mode === "workspace" && workspaceBinding && <button disabled={busy} aria-expanded={frozenContentOpen} onClick={() => setFrozenContentOpen((current) => !current)}>이전 본문 요청 대조</button>}
        {mode === "workspace" && workspaceBinding && frozenContentOpen && <FrozenContentReview binding={workspaceBinding} ports={frozenContentPorts} />}
        {mode === "workspace" && workspaceBinding && <button disabled={busy} aria-expanded={trashOpen} onClick={() => setTrashOpen((current) => !current)}>휴지통</button>}
        {mode === "workspace" && workspaceBinding && trashOpen && <TrashPanel binding={workspaceBinding} ports={trashPorts} revision={restoreRevision} permissionDeniedOperationId={restorePermission?.binding === JSON.stringify(workspaceBinding) ? restorePermission.operationId : null} onQueued={() => { setRestoreRevision((current) => current + 1); transmitter.current?.kick(); }} onCancelled={(operationId) => {
          const binding = JSON.stringify(workspaceBinding);
          if (!workspaceBindingRef.current || JSON.stringify(workspaceBindingRef.current) !== binding) return;
          if (!transmitter.current?.resumeRestoreAfterCancel(binding, operationId)) transmitter.current?.kick();
          setRestoreRevision((current) => current + 1);
        }} onRecheck={(operationId) => {
          const binding = JSON.stringify(workspaceBinding);
          if (!workspaceBindingRef.current || JSON.stringify(workspaceBindingRef.current) !== binding) return;
          if (!transmitter.current?.resumeRestoreAfterCancel(binding, operationId)) transmitter.current?.kick();
        }} onRetry={(operationId) => {
          const binding = JSON.stringify(workspaceBinding);
          if (!workspaceBindingRef.current || JSON.stringify(workspaceBindingRef.current) !== binding) return;
          if (transmitter.current?.retryRestoreAfterPermissionDenied(binding, operationId)) {
            setRestorePermission((current) => current?.binding === binding && current.operationId === operationId ? null : current);
          }
        }} />}
        <p>기기에 보존된 초안</p>
        {drafts.map((draft) => <button key={draftKey(draft)} disabled={busy} data-draft-document={draft.document_id} aria-current={state.selected === draftSelectionId(draft, mode) ? "page" : undefined} onClick={() => void controller.current?.open(draftSelectionId(draft, mode))}>{draft.binding.workspace_id.slice(0, 8)} / {draft.document_id}</button>)}
        <button disabled={busy} onClick={() => void refreshDrafts().catch((cause) => setConnectionError(String(cause)))}>초안 목록 새로고침</button>
        <small>{mode === "fixture" ? "연결 전에는 시험 문서가 표시됩니다." : "초안은 먼저 이 기기에 저장됩니다."}</small>
        <button id="isolation-open" onClick={() => { setIsolation(true); void invoke("m0_open_isolation").catch((cause) => setProbeError(String(cause))); }}>격리 시험 열기</button>
      </nav>
      <section className="document">
        <div className="toolbar"><span><span id="save-status" role="status">{state.status}</span>{visibleSyncStatus && <span id="sync-status">, {visibleSyncStatus}</span>}{workspaceSyncMessage && <span id="workspace-sync-status" role="status">, {workspaceSyncMessage}</span>}</span><div className="toolbar-actions"><button id="preview-toggle" aria-pressed={preview} onClick={() => {
          clearTimeout(previewTimer.current);
          previewEnabled.current = !previewEnabled.current;
          setPreview(previewEnabled.current);
          if (previewEnabled.current) setPreviewSource(source.current);
        }}>미리보기</button><button id="save" onClick={() => void controller.current?.save().catch(() => {})}>지금 저장</button>{hasWorkspaceDocument && <><button data-testid="close-document" onClick={() => void controller.current?.closeCurrentDocument()}>문서 닫기</button><button id="copy-source" disabled={recovering} onClick={() => void recoverSource("copy")}>원문 복사</button><button id="export-source" disabled={recovering} onClick={() => void recoverSource("export")}>파일 내보내기</button></>}</div></div>
        {state.notice && <p className="notice">{state.notice}</p>}
        {state.remoteChanged && <p role="status">다른 기기에서 변경됨</p>}
        {state.remoteDeleted && <p role="status">다른 기기에서 삭제됨. 현재 편집 내용은 유지됩니다. 원문을 내보내거나 휴지통에서 복원을 확인하세요.</p>}
        {state.remoteDeleted && hasWorkspaceDocument && <button type="button" data-testid="deleted-copy-open" disabled={deletedCopyBusy || deletedCopyRecorded} onClick={() => void inspectDeletedCopy()}>초안을 새 문서로 보존</button>}
        {deletedCopyMessage && <p role="status">{deletedCopyMessage}</p>}
        {deletedCopyRecovery && <button type="button" data-testid="deleted-copy-recover" disabled={deletedCopyBusy} onClick={() => void recoverDeletedCopy()}>사본 상태 확인 후 문서 다시 열기</button>}
        {deletedCopyStatus && deletedStatusCurrent(deletedCopyStatus) && <section aria-label="삭제 초안 사본 상태">
          <p>{deletedCopyStatus.status.name}: {deletedCopyStatus.status.content_status === "saved" ? "사본 저장 완료" : deletedCopyStatus.status.create_status === "needs_review" || deletedCopyStatus.status.content_status === "needs_review" ? (deletedCopyStatus.status.create_failure === "destination_exists" ? "목적지에 같은 이름이 있습니다." : "사본 상태를 다시 확인해 주세요.") : deletedCopyStatus.status.content_status === "pending" ? "사본 본문 저장 확인 중" : deletedCopyStatus.status.create_status === "created" ? "사본 본문 저장 대기" : deletedCopyStatus.status.create_status === "pending" ? "사본 생성 확인 중" : "사본 저장 대기"}. 원래 삭제 상태는 변경하지 않습니다.</p>
          {deletedCopyStatus.status.content_status === "saved" && <p>확인한 초안 generation {deletedCopyStatus.status.generation}의 내용이 사본에 저장됐습니다. 이후 편집한 내용은 이 사본에 포함되지 않습니다.</p>}
          <button type="button" data-testid="deleted-copy-status-refresh" disabled={deletedCopyBusy} onClick={() => void refreshDeletedCopyStatus()}>사본 상태 확인</button>
          {deletedCopyStatus.status.create_status === "needs_review" && deletedCopyStatus.status.create_failure === "destination_exists" && (!deletedCopyRetryOpen ? <button type="button" data-testid="deleted-copy-retry" disabled={deletedCopyBusy} onClick={() => setDeletedCopyRetryOpen(true)}>다른 사본 대상 선택</button> : <ConflictCopyRetry key={`${JSON.stringify(deletedCopyStatus.binding)}:${deletedCopyStatus.status.create_operation_id}`} binding={deletedCopyStatus.binding} previousOperationId={deletedCopyStatus.status.create_operation_id} previousParentId={deletedCopyStatus.status.parent_id} previousName={deletedCopyStatus.status.name} loadFolders={workspaceCreateParents} busy={deletedCopyBusy} onConfirm={reconfirmDeletedCopy} onCancel={() => setDeletedCopyRetryOpen(false)} />)}
          {deletedCopyStatus.status.blocking_followup && <>
            <p>삭제된 문서의 실패 작업 {deletedCopyStatus.status.blocking_followup.blocker_operation_id}, 후속 작업 {deletedCopyStatus.status.blocking_followup.followup_operation_id} (순번 {deletedCopyStatus.status.blocking_followup.followup_operation_seq}, generation {deletedCopyStatus.status.blocking_followup.followup_generation}, hash {deletedCopyStatus.status.blocking_followup.followup_hash})이 사본 저장을 막고 있습니다. 사본은 확인한 초안 generation {deletedCopyStatus.status.generation}을 보존합니다. 기기의 현재 초안은 별도로 보존됩니다.</p>
            {!deletedFollowupApproval ? <button type="button" data-testid="deleted-copy-followup-open" disabled={deletedCopyBusy} onClick={() => setDeletedFollowupApproval(deletedCopyStatus.status.blocking_followup)}>후속 작업 확인</button> : <>
              <p>후속 작업 {deletedFollowupApproval.followup_operation_id} (순번 {deletedFollowupApproval.followup_operation_seq}, generation {deletedFollowupApproval.followup_generation}, hash {deletedFollowupApproval.followup_hash})의 전송 취소를 요청합니다. 확인한 사본과 기기의 현재 초안은 별도로 보존됩니다.</p>
              <button type="button" key={`${deletedFollowupApproval.blocker_operation_id}:${deletedFollowupApproval.followup_operation_id}:${deletedFollowupApproval.followup_operation_seq}:${deletedFollowupApproval.followup_generation}:${deletedFollowupApproval.followup_hash}`} data-testid="deleted-copy-followup-approve" disabled={deletedCopyBusy} onClick={() => void approveDeletedFollowup()}>이 작업 취소 승인</button>
              <button type="button" data-testid="deleted-copy-followup-cancel" disabled={deletedCopyBusy} onClick={() => setDeletedFollowupApproval(null)}>취소</button>
            </>}
          </>}
          {deletedCopyStatus.status.content_status !== "saved" && deletedCopyStatus.status.create_status !== "needs_review" && deletedCopyStatus.status.content_status !== "needs_review" && deletedCopyStatus.status.blocking_followup === null && <button type="button" data-testid="deleted-copy-progress" disabled={deletedCopyBusy} onClick={() => void progressDeletedCopy()}>사본 저장 진행</button>}
        </section>}
        {deletedCopyReview && deletedCopyCurrent(deletedCopyReview) && <DeletedDraftCopyReview key={`${deletedCopyReview.snapshot.edit_session_id}:${deletedCopyReview.snapshot.generation}:${deletedCopyReview.snapshot.local_hash}:${deletedCopyReview.snapshot.tombstone_seq}`} snapshot={deletedCopyReview.snapshot} localBytes={deletedCopyReview.bytes} loadFolders={workspaceCreateParents} onConfirm={confirmDeletedCopy} onCancel={() => setDeletedCopyReview(null)} />}
        {hasWorkspaceDocument && hasContentConflict && <button data-testid="conflict-review-open" disabled={conflictPreparing} onClick={() => void inspectConflict()}>충돌 비교</button>}
        {conflictMessage && <p role="status">{conflictMessage}</p>}
        {pendingCopy && conflictContextCurrent(pendingCopy) && <section aria-label="사본 저장 상태">
          <p>{pendingCopy.copy.name}: {pendingCopy.copy.content_status === "saved" ? "사본 저장 완료" : pendingCopy.copy.create_status === "needs_review" || pendingCopy.copy.content_status === "needs_review" ? "사본 상태를 다시 확인해 주세요" : pendingCopy.copy.content_status === "pending" ? "사본 본문 저장 확인 중" : pendingCopy.copy.create_status === "created" ? "사본 본문 저장 대기" : pendingCopy.copy.create_status === "pending" ? "사본 생성 확인 중" : "사본 저장 대기"}. 원래 문서의 충돌은 유지됩니다.</p>
          {pendingCopy.copy.create_status === "needs_review" && <p>생성 요청 {pendingCopy.copy.create_operation_id}: {pendingCopy.copy.create_failure === "destination_exists" ? "목적지에 같은 이름이 있습니다." : "생성 결과를 확인해 주세요."} 초안과 실패 기록은 보존됩니다.</p>}
          <button type="button" data-testid="conflict-copy-status-refresh" disabled={conflictRecovering} onClick={() => void refreshCopy()}>사본 상태 확인</button>
          {pendingCopy.copy.create_failure === "destination_exists" && !conflictRecovery && (!copyRetryOpen ? <button type="button" data-testid="conflict-copy-retry" disabled={conflictRecovering} onClick={() => setCopyRetryOpen(true)}>다른 사본 대상 선택</button> : <ConflictCopyRetry key={`${JSON.stringify(pendingCopy.snapshot.binding)}:${pendingCopy.copy.create_operation_id}`} binding={pendingCopy.snapshot.binding} previousOperationId={pendingCopy.copy.create_operation_id} previousParentId={pendingCopy.copy.parent_id} previousName={pendingCopy.copy.name} loadFolders={workspaceCreateParents} busy={conflictRecovering} onConfirm={retryCopy} onCancel={() => setCopyRetryOpen(false)} />)}
          {pendingCopy.copy.content_status !== "saved" && pendingCopy.copy.create_status !== "needs_review" && pendingCopy.copy.content_status !== "needs_review" && !pendingCopy.approval && !conflictRecovery && <button type="button" data-testid="conflict-copy-progress" disabled={conflictRecovering} onClick={() => {
            if (pendingCopy.copy.followup_operation_id) setPendingCopy({ ...pendingCopy, running: false, approval: pendingCopy.copy.followup_operation_id });
            else void progressCopy(pendingCopy, null);
          }}>사본 저장 진행</button>}
          {pendingCopy.approval && <>
            <p>사본 저장을 위해 후속 작업 {pendingCopy.approval}의 전송 취소가 필요합니다. 보존된 초안과 충돌 자료는 유지됩니다.</p>
            <button type="button" key={pendingCopy.approval} data-testid="conflict-copy-approve" disabled={conflictRecovering} onClick={() => { if (pendingCopy.approval === pendingCopy.copy.followup_operation_id) void progressCopy(pendingCopy, pendingCopy.approval); }}>이 작업 취소 승인</button>
            <button type="button" data-testid="conflict-copy-approval-cancel" disabled={conflictRecovering} onClick={() => setPendingCopy({ ...pendingCopy, approval: null, running: false })}>취소</button>
          </>}
        </section>}
        {pendingConflict && <button data-testid="conflict-continue" disabled={conflictRecovering} onClick={() => void continueConflict()}>후속 처리 진행</button>}
        {pendingConflict && <button data-testid="conflict-complete" disabled={conflictRecovering} onClick={() => void completeConflict()}>서버 본문으로 완료</button>}
        {pendingMerge && conflictContextCurrent(pendingMerge) && <section aria-label="합본 저장 상태">
          <p>{pendingMerge.queuedOperationId ? `합본 작업 ${pendingMerge.queuedOperationId} (순번 ${pendingMerge.queuedOperationSeq})의 서버 결과와 ACK를 기다립니다.` : "합본 선택을 기기에 저장했습니다. 서버 저장은 대기 중입니다."}</p>
          <button type="button" data-testid="conflict-merge-status-refresh" disabled={conflictRecovering} onClick={() => void refreshMerge()}>합본 상태 확인</button>
          {!pendingMerge.queuedOperationId && <button type="button" data-testid="conflict-merge-queue" disabled={conflictRecovering} onClick={() => void queueMerge()}>합본 전송 준비</button>}
          {pendingMerge.followup && <>
            <p>후속 작업 {pendingMerge.followup.operation_id} (순번 {pendingMerge.followup.operation_seq}, generation {pendingMerge.followup.generation}, hash {pendingMerge.followup.payload_hash})의 전송 취소가 필요합니다. 선택한 합본과 기기 초안은 보존됩니다.</p>
            <button type="button" data-testid="conflict-merge-followup-refresh" disabled={conflictRecovering} onClick={() => void refreshMergeFollowup()}>후속 작업 다시 확인</button>
            {pendingMerge.approval ? <button type="button" data-testid="conflict-merge-followup-confirm" disabled={conflictRecovering} onClick={() => void approveMergeFollowup()}>이 작업 취소 승인</button> : <button type="button" disabled={conflictRecovering} onClick={() => setPendingMerge((current) => current === pendingMerge ? { ...current, approval:current.followup } : current)}>표시한 작업 확인</button>}
            <button type="button" data-testid="conflict-merge-followup-cancel" disabled={conflictRecovering} onClick={() => setPendingMerge((current) => current === pendingMerge ? { ...current, followup:null, approval:null } : current)}>취소</button>
          </>}
        </section>}
        {conflictRecovery && <button data-testid="conflict-recover-result" disabled={conflictRecovering} onClick={() => void recoverConflictResult()}>선택 결과 확인 후 문서 다시 열기</button>}
        {conflictSnapshot && workspaceBinding && JSON.stringify(conflictSnapshot.snapshot.binding) === JSON.stringify(workspaceBinding) && <ConflictReview binding={conflictSnapshot.snapshot.binding} documentId={conflictSnapshot.snapshot.node_id} sessionId={conflictSnapshot.sessionId} generation={conflictSnapshot.snapshot.generation} operationId={conflictSnapshot.snapshot.operation_id} ports={conflictReviewPorts} onClose={() => setConflictSnapshot(null)} onChooseRemote={conflictSnapshot.canChoose ? chooseRemote : undefined} onKeepCopy={conflictSnapshot.canChoose && pendingCopy?.copy.content_status !== "saved" ? keepConflictCopy : undefined} onSaveMerged={conflictSnapshot.canChoose ? saveMerged : undefined} loadFolders={workspaceCreateParents} />}
        {recoveryMessage && <p role="status">{recoveryMessage}</p>}
        {memoryRecovery && controller.current === memoryRecovery.instance && state.selected === memoryRecovery.id && registrationVersion.current === memoryRecovery.registration && JSON.stringify(workspaceBindingRef.current) === memoryRecovery.binding && sessionModes.current.get(memoryRecovery.session) === memoryRecovery.sessionRef && memoryRecovery.instance.isBlockedRecoverySnapshot(memoryRecovery.session, memoryRecovery.generation, memoryRecovery.revision) && <section data-testid="memory-source-recovery" aria-label="메모리 원문 복구">
          {memoryRecovery.sessionBinding ? <p>{JSON.stringify(memoryRecovery.sessionBinding) === memoryRecovery.binding ? "현재 등록" : "이전 등록"}의 문서: 서버 {memoryRecovery.sessionBinding.server_origin}, 작업 공간 {memoryRecovery.sessionBinding.workspace_id}, epoch {memoryRecovery.sessionBinding.workspace_epoch}</p> : <p>이 문서의 등록 정보를 확인할 수 없습니다.</p>}
          <p>저장에 실패한 내용입니다. 앱을 닫지 말고 아래 텍스트를 직접 전체 선택해 복사한 뒤 안전한 곳에 보관하세요. BOM과 바이트의 동일성은 보장하지 않으며 자동 파일 내보내기는 되지 않습니다.</p>
          <textarea data-testid="memory-source-fallback" aria-label="복구할 메모리 텍스트" readOnly value={memoryRecovery.text} />
        </section>}
        <QueueAlerts error={state.error || probeError || connectionError || inventoryError} queueErrors={queueErrors} />
        <div className="document-content">
          <div id="editor" ref={parent} />
          {preview && <section className="preview-pane" aria-label="Markdown 미리보기">{new TextEncoder().encode(previewSource).length > 1024 * 1024
            ? <p>큰 문서는 미리보기를 표시하지 않습니다.</p>
            : <MarkdownPreview source={previewSource} fragment={previewAnchor?.documentId === state.selected ? previewAnchor : null} loadImage={mode === "workspace" ? loadPreviewImage : undefined} onOpenLink={mode === "workspace" ? (link) => {
              if (link.kind === "external") {
                const found = [...sessionModes.current.entries()].find(([, session]) => session.uiId === state.selected && session.mode === "workspace");
                if (!found) setConnectionError("문서를 다시 열어 주세요.");
                else void openWorkspaceExternal(found[1].documentId, found[0], link.target).catch((cause) => setConnectionError(String(cause)));
                return;
              }
              if (link.kind !== "document") return;
              const resolved = resolveWorkspaceReference(nodesRef.current, state.selected, link.target);
              if (!resolved) setConnectionError("연결된 문서를 찾지 못했습니다. 목록을 새로고침하거나 다른 구간을 확인해 주세요.");
              else void controller.current?.open(resolved.node.node_id).then(() => {
                if (resolved.fragment && selectedRef.current === resolved.node.node_id) {
                  setPreviewAnchor({ documentId: resolved.node.node_id, fragment: resolved.fragment });
                }
              });
            } : undefined} />}</section>}
        </div>
        {isolation && <iframe id="sandbox-probe" title="격리 시험" sandbox="allow-scripts" src="isolation.html" />}
      </section>
    </div>
  </main>;
}

createRoot(document.getElementById("root")!).render(<StartupGate ready={() => <App />} />);
