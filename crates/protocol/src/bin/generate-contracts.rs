use std::{collections::BTreeMap, error::Error, fs, path::PathBuf};

use notryx_protocol::{
    AbandonRequest, AckRequest, AckResponse, AdminRecoverySummary, AdminRequest, AdminResponse,
    AdminResult, AdminUnsupportedItem, ApiError, ChangesPage, ClientStartupMode,
    ClientStartupStatus, CreateNodeRequest, DeviceCurrentRole, DeviceIdentity, DeviceRenewal,
    HeadHint, InventoryPage, JournalAccounting, M0CloseRequest, M0OpenRequest, M0OpenedDocument,
    M0SaveRequest, M0SavedCheckpoint, MigrationDraft, MigrationInventory,
    MigrationInventoryRequest, MigrationOperation, MigrationOperationKind, MigrationSource,
    MigrationSourceKind, MigrationSourceRequest, MoveNodeRequest, NodeKind, NodeMetadata,
    NodeState, OperationError, OperationReceipt, PairRequest, PairResponse, RestoreNodeRequest,
    ServerInfo, TrashNode, TrashPage, UnsupportedItemReason, UploadResponse,
    WorkspaceAbandonRequest, WorkspaceAbandonState, WorkspaceCacheWarning, WorkspaceConflictChoice,
    WorkspaceConflictResolutionRequest, WorkspaceConflictResolutionResponse,
    WorkspaceConflictResolutionState, WorkspaceContentConflictBlock,
    WorkspaceContentConflictChooseRemoteRequest, WorkspaceContentConflictChooseRemoteResponse,
    WorkspaceContentConflictCopyContentStatus, WorkspaceContentConflictCopyCreateFailure,
    WorkspaceContentConflictCopyCreateStatus, WorkspaceContentConflictCopyProgressRequest,
    WorkspaceContentConflictCopyProgressResponse, WorkspaceContentConflictCopyRequest,
    WorkspaceContentConflictCopyResponse, WorkspaceContentConflictCopyRetryRequest,
    WorkspaceContentConflictCopyStatus, WorkspaceContentConflictCopyStatusResponse,
    WorkspaceContentConflictMergeFollowup, WorkspaceContentConflictMergeFollowupApprovalRequest,
    WorkspaceContentConflictMergeFollowupStatusResponse, WorkspaceContentConflictMergeOperation,
    WorkspaceContentConflictMergeStatusRequest, WorkspaceContentConflictMergeStatusResponse,
    WorkspaceContentConflictProgressRequest, WorkspaceContentConflictProgressResponse,
    WorkspaceContentConflictRef, WorkspaceContentConflictReopenRequest,
    WorkspaceContentConflictReopenResponse, WorkspaceContentConflictRequest,
    WorkspaceContentConflictSnapshot, WorkspaceCreateAbandonRequest, WorkspaceCreateEntry,
    WorkspaceCreateListRequest, WorkspaceCreatePage, WorkspaceCreateParentsPage,
    WorkspaceCreateParentsRequest, WorkspaceCreateQueueRequest, WorkspaceCreateStatus,
    WorkspaceCredentialRenewalStatus, WorkspaceCredentialState, WorkspaceCredentialStatus,
    WorkspaceDeleteAbandonRequest, WorkspaceDeleteEntry, WorkspaceDeleteListRequest,
    WorkspaceDeletePage, WorkspaceDeleteQueueRequest, WorkspaceDeleteStatus,
    WorkspaceDeletedDraftCopyBlockingFollowup, WorkspaceDeletedDraftCopyConfirmRequest,
    WorkspaceDeletedDraftCopyConfirmResult, WorkspaceDeletedDraftCopyConfirmStatus,
    WorkspaceDeletedDraftCopyFollowupAbandonRequest,
    WorkspaceDeletedDraftCopyFollowupAbandonResult, WorkspaceDeletedDraftCopyProgressRequest,
    WorkspaceDeletedDraftCopyProgressResult, WorkspaceDeletedDraftCopyReconfirmRequest,
    WorkspaceDeletedDraftCopyReopenRequest, WorkspaceDeletedDraftCopyReopenResponse,
    WorkspaceDeletedDraftCopySnapshot, WorkspaceDeletedDraftCopySnapshotRequest,
    WorkspaceDeletedDraftCopyStatus, WorkspaceDeletedDraftCopyStatusRequest,
    WorkspaceDirtyRemoteRequest, WorkspaceDirtyRemoteState, WorkspaceDocumentOutboxState,
    WorkspaceDraft, WorkspaceDraftRequest, WorkspaceExternalLinkRequest,
    WorkspaceFrozenContentApplyChoice, WorkspaceFrozenContentApplyRequest,
    WorkspaceFrozenContentApplyResponse, WorkspaceFrozenContentCrossEpochApplyRequest,
    WorkspaceFrozenContentCrossEpochInspection, WorkspaceFrozenContentCrossEpochTarget,
    WorkspaceFrozenContentDecision, WorkspaceFrozenContentDecisionStatusRequest,
    WorkspaceFrozenContentDecisionStatusResponse, WorkspaceFrozenContentDismissRequest,
    WorkspaceFrozenContentDismissed, WorkspaceFrozenContentInspectRequest,
    WorkspaceFrozenContentInspection, WorkspaceFrozenContentLocalDismissRequest,
    WorkspaceFrozenContentLocalInspectRequest, WorkspaceFrozenContentLocalInspection,
    WorkspaceFrozenContentRemoteEvidence, WorkspaceFrozenCreateChoice,
    WorkspaceFrozenCreateDecisionRequest, WorkspaceFrozenCreateEntry,
    WorkspaceFrozenCreateListRequest, WorkspaceFrozenCreatePage, WorkspaceFrozenDeleteChoice,
    WorkspaceFrozenDeleteDecisionRequest, WorkspaceFrozenDeleteEntry,
    WorkspaceFrozenDeleteListRequest, WorkspaceFrozenDeletePage, WorkspaceFrozenMoveChoice,
    WorkspaceFrozenMoveDecisionRequest, WorkspaceFrozenMoveEntry, WorkspaceFrozenMoveListRequest,
    WorkspaceFrozenMovePage, WorkspaceInventoryRequest, WorkspaceLocalRequest,
    WorkspaceMoveAbandonRequest, WorkspaceMoveEntry, WorkspaceMoveListRequest, WorkspaceMovePage,
    WorkspaceMoveQueueRequest, WorkspaceMoveStatus, WorkspaceOpenNotice, WorkspaceOpenRequest,
    WorkspaceOpenedDocument, WorkspaceOutboxEntry, WorkspaceOutboxFailure, WorkspaceOutboxPage,
    WorkspaceOutboxRequest, WorkspaceOutboxStatus, WorkspacePairRequest, WorkspacePairStatus,
    WorkspacePreviewImage, WorkspacePreviewImageRequest, WorkspaceQueueRequest,
    WorkspaceQueuedOperation, WorkspaceRecoveryRequest, WorkspaceRegistration,
    WorkspaceRemoteRefreshCandidate, WorkspaceRemoteRefreshCommitRequest,
    WorkspaceRemoteRefreshRequest, WorkspaceRestoreAbandonRequest, WorkspaceRestoreEntry,
    WorkspaceRestoreListRequest, WorkspaceRestorePage, WorkspaceRestoreQueueRequest,
    WorkspaceRestoreStatus, WorkspaceRole, WorkspaceSearchCancelRequest, WorkspaceSearchPage,
    WorkspaceSearchRequest, WorkspaceSearchResult, WorkspaceSearchScope, WorkspaceSearchSource,
    WorkspaceSendResult, WorkspaceSendStatus, WorkspaceSyncState, WorkspaceSyncStatus,
};
use notryx_protocol::{
    ThemeBase, ThemeFont, ThemePackage, ThemePreferences, ThemeSelection, ThemeStatus, ThemeTokens,
};
use schemars::{JsonSchema, schema_for};
use ts_rs::{Config, TS};

fn add_type<T: JsonSchema + TS>(
    config: &Config,
    declarations: &mut String,
    schemas: &mut BTreeMap<String, serde_json::Value>,
) -> Result<(), serde_json::Error> {
    declarations.push_str("export ");
    declarations.push_str(&T::decl(config));
    declarations.push('\n');
    schemas.insert(T::name(config), serde_json::to_value(schema_for!(T))?);
    Ok(())
}

fn main() -> Result<(), Box<dyn Error>> {
    let arguments: Vec<_> = std::env::args().skip(1).collect();
    let check = match arguments.as_slice() {
        [] => false,
        [flag] if flag == "--check" => true,
        _ => return Err("usage: generate-contracts [--check]".into()),
    };
    let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../packages/protocol-types");
    let config = Config::default();
    let mut declarations = String::from(
        "// Generated by cargo run -p notryx-protocol --bin generate-contracts. Do not edit.\n\
         // M0-prefixed types are spike IPC contracts, not the product HTTP API.\n\
         // Admin-prefixed types are for the local Unix socket, not public HTTP endpoints.\n\n",
    );
    let mut schemas = BTreeMap::new();
    add_type::<ThemeBase>(&config, &mut declarations, &mut schemas)?;
    add_type::<ThemeFont>(&config, &mut declarations, &mut schemas)?;
    add_type::<ThemeSelection>(&config, &mut declarations, &mut schemas)?;
    add_type::<ThemeTokens>(&config, &mut declarations, &mut schemas)?;
    add_type::<ThemePackage>(&config, &mut declarations, &mut schemas)?;
    add_type::<ThemePreferences>(&config, &mut declarations, &mut schemas)?;
    add_type::<ThemeStatus>(&config, &mut declarations, &mut schemas)?;
    add_type::<HeadHint>(&config, &mut declarations, &mut schemas)?;
    add_type::<M0OpenRequest>(&config, &mut declarations, &mut schemas)?;
    add_type::<M0CloseRequest>(&config, &mut declarations, &mut schemas)?;
    add_type::<M0OpenedDocument>(&config, &mut declarations, &mut schemas)?;
    add_type::<M0SaveRequest>(&config, &mut declarations, &mut schemas)?;
    add_type::<M0SavedCheckpoint>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspacePairRequest>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceInventoryRequest>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceOpenRequest>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceRemoteRefreshRequest>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceRemoteRefreshCandidate>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceRemoteRefreshCommitRequest>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceDirtyRemoteRequest>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceDeletedDraftCopySnapshotRequest>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceDeletedDraftCopySnapshot>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceDeletedDraftCopyConfirmRequest>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceDeletedDraftCopyConfirmResult>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceDeletedDraftCopyConfirmStatus>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceDeletedDraftCopyStatusRequest>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceDeletedDraftCopyStatus>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceDeletedDraftCopyBlockingFollowup>(
        &config,
        &mut declarations,
        &mut schemas,
    )?;
    add_type::<WorkspaceDeletedDraftCopyFollowupAbandonRequest>(
        &config,
        &mut declarations,
        &mut schemas,
    )?;
    add_type::<WorkspaceDeletedDraftCopyFollowupAbandonResult>(
        &config,
        &mut declarations,
        &mut schemas,
    )?;
    add_type::<WorkspaceDeletedDraftCopyProgressRequest>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceDeletedDraftCopyProgressResult>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceDeletedDraftCopyReopenRequest>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceDeletedDraftCopyReopenResponse>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceDeletedDraftCopyReconfirmRequest>(
        &config,
        &mut declarations,
        &mut schemas,
    )?;
    add_type::<WorkspaceContentConflictRequest>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceContentConflictSnapshot>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceFrozenContentInspectRequest>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceFrozenContentInspection>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceFrozenContentApplyChoice>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceFrozenContentApplyRequest>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceFrozenContentApplyResponse>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceFrozenContentCrossEpochTarget>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceFrozenContentCrossEpochInspection>(
        &config,
        &mut declarations,
        &mut schemas,
    )?;
    add_type::<WorkspaceFrozenContentCrossEpochApplyRequest>(
        &config,
        &mut declarations,
        &mut schemas,
    )?;
    add_type::<WorkspaceFrozenContentDismissRequest>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceFrozenContentDismissed>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceFrozenContentRemoteEvidence>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceFrozenContentLocalInspection>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceFrozenContentLocalInspectRequest>(
        &config,
        &mut declarations,
        &mut schemas,
    )?;
    add_type::<WorkspaceFrozenContentLocalDismissRequest>(
        &config,
        &mut declarations,
        &mut schemas,
    )?;
    add_type::<WorkspaceFrozenContentDecision>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceFrozenContentDecisionStatusRequest>(
        &config,
        &mut declarations,
        &mut schemas,
    )?;
    add_type::<WorkspaceFrozenContentDecisionStatusResponse>(
        &config,
        &mut declarations,
        &mut schemas,
    )?;
    add_type::<WorkspaceContentConflictChooseRemoteRequest>(
        &config,
        &mut declarations,
        &mut schemas,
    )?;
    add_type::<WorkspaceContentConflictChooseRemoteResponse>(
        &config,
        &mut declarations,
        &mut schemas,
    )?;
    add_type::<WorkspaceContentConflictCopyRequest>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceContentConflictCopyResponse>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceContentConflictCopyProgressResponse>(
        &config,
        &mut declarations,
        &mut schemas,
    )?;
    add_type::<WorkspaceContentConflictCopyProgressRequest>(
        &config,
        &mut declarations,
        &mut schemas,
    )?;
    add_type::<WorkspaceContentConflictCopyRetryRequest>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceContentConflictCopyCreateStatus>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceContentConflictCopyCreateFailure>(
        &config,
        &mut declarations,
        &mut schemas,
    )?;
    add_type::<WorkspaceContentConflictCopyContentStatus>(
        &config,
        &mut declarations,
        &mut schemas,
    )?;
    add_type::<WorkspaceContentConflictCopyStatus>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceContentConflictCopyStatusResponse>(
        &config,
        &mut declarations,
        &mut schemas,
    )?;
    add_type::<WorkspaceContentConflictProgressRequest>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceContentConflictMergeOperation>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceContentConflictMergeFollowup>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceContentConflictMergeFollowupApprovalRequest>(
        &config,
        &mut declarations,
        &mut schemas,
    )?;
    add_type::<WorkspaceContentConflictMergeFollowupStatusResponse>(
        &config,
        &mut declarations,
        &mut schemas,
    )?;
    add_type::<WorkspaceContentConflictMergeStatusRequest>(
        &config,
        &mut declarations,
        &mut schemas,
    )?;
    add_type::<WorkspaceContentConflictMergeStatusResponse>(
        &config,
        &mut declarations,
        &mut schemas,
    )?;
    add_type::<WorkspaceContentConflictProgressResponse>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceContentConflictReopenRequest>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceContentConflictReopenResponse>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceConflictChoice>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceConflictResolutionRequest>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceConflictResolutionResponse>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceConflictResolutionState>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceDirtyRemoteState>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceCacheWarning>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceOpenNotice>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceOpenedDocument>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceRegistration>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceSearchScope>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceSearchSource>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceSearchRequest>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceSearchCancelRequest>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceSearchResult>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceSearchPage>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceRestoreAbandonRequest>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceRestoreQueueRequest>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceRestoreListRequest>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceRestoreStatus>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceRestoreEntry>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceRestorePage>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceDeleteQueueRequest>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceDeleteListRequest>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceDeleteAbandonRequest>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceDeleteStatus>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceDeleteEntry>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceDeletePage>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceMoveQueueRequest>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceMoveListRequest>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceMoveAbandonRequest>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceMoveStatus>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceMoveEntry>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceMovePage>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceCreateAbandonRequest>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceCreateQueueRequest>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceCreateParentsRequest>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceCreateParentsPage>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceFrozenDeleteChoice>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceFrozenDeleteDecisionRequest>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceFrozenDeleteEntry>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceFrozenDeleteListRequest>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceFrozenDeletePage>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceFrozenMoveChoice>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceFrozenMoveDecisionRequest>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceFrozenMoveEntry>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceFrozenMoveListRequest>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceFrozenMovePage>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceFrozenCreateChoice>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceFrozenCreateDecisionRequest>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceFrozenCreateEntry>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceFrozenCreateListRequest>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceFrozenCreatePage>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceCreateListRequest>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceCreateStatus>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceCreateEntry>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceCreatePage>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceDraft>(&config, &mut declarations, &mut schemas)?;
    add_type::<ClientStartupMode>(&config, &mut declarations, &mut schemas)?;
    add_type::<ClientStartupStatus>(&config, &mut declarations, &mut schemas)?;
    add_type::<MigrationDraft>(&config, &mut declarations, &mut schemas)?;
    add_type::<MigrationOperationKind>(&config, &mut declarations, &mut schemas)?;
    add_type::<MigrationOperation>(&config, &mut declarations, &mut schemas)?;
    add_type::<MigrationInventory>(&config, &mut declarations, &mut schemas)?;
    add_type::<MigrationInventoryRequest>(&config, &mut declarations, &mut schemas)?;
    add_type::<MigrationSourceKind>(&config, &mut declarations, &mut schemas)?;
    add_type::<MigrationSourceRequest>(&config, &mut declarations, &mut schemas)?;
    add_type::<MigrationSource>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspacePairStatus>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceSyncStatus>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceSyncState>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceCredentialStatus>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceCredentialState>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceCredentialRenewalStatus>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceLocalRequest>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceDraftRequest>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspacePreviewImageRequest>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspacePreviewImage>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceQueueRequest>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceExternalLinkRequest>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceRecoveryRequest>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceAbandonRequest>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceQueuedOperation>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceSendStatus>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceSendResult>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceContentConflictBlock>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceContentConflictRef>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceOutboxStatus>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceAbandonState>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceOutboxFailure>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceOutboxEntry>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceDocumentOutboxState>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceOutboxPage>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceOutboxRequest>(&config, &mut declarations, &mut schemas)?;
    add_type::<WorkspaceRole>(&config, &mut declarations, &mut schemas)?;
    add_type::<NodeKind>(&config, &mut declarations, &mut schemas)?;
    add_type::<PairRequest>(&config, &mut declarations, &mut schemas)?;
    add_type::<PairResponse>(&config, &mut declarations, &mut schemas)?;
    add_type::<DeviceIdentity>(&config, &mut declarations, &mut schemas)?;
    add_type::<DeviceCurrentRole>(&config, &mut declarations, &mut schemas)?;
    add_type::<DeviceRenewal>(&config, &mut declarations, &mut schemas)?;
    add_type::<ServerInfo>(&config, &mut declarations, &mut schemas)?;
    add_type::<UploadResponse>(&config, &mut declarations, &mut schemas)?;
    add_type::<NodeMetadata>(&config, &mut declarations, &mut schemas)?;
    add_type::<CreateNodeRequest>(&config, &mut declarations, &mut schemas)?;
    add_type::<MoveNodeRequest>(&config, &mut declarations, &mut schemas)?;
    add_type::<RestoreNodeRequest>(&config, &mut declarations, &mut schemas)?;
    add_type::<NodeState>(&config, &mut declarations, &mut schemas)?;
    add_type::<InventoryPage>(&config, &mut declarations, &mut schemas)?;
    add_type::<ChangesPage>(&config, &mut declarations, &mut schemas)?;
    add_type::<TrashNode>(&config, &mut declarations, &mut schemas)?;
    add_type::<TrashPage>(&config, &mut declarations, &mut schemas)?;
    declarations.push_str("export type CreatedNode = NodeState;\n");
    schemas.insert(
        "CreatedNode".into(),
        serde_json::json!({"$ref": "#/$defs/NodeState"}),
    );
    add_type::<ApiError>(&config, &mut declarations, &mut schemas)?;
    add_type::<OperationReceipt>(&config, &mut declarations, &mut schemas)?;
    add_type::<OperationError>(&config, &mut declarations, &mut schemas)?;
    add_type::<AckRequest>(&config, &mut declarations, &mut schemas)?;
    add_type::<AckResponse>(&config, &mut declarations, &mut schemas)?;
    add_type::<AbandonRequest>(&config, &mut declarations, &mut schemas)?;
    add_type::<AdminRequest>(&config, &mut declarations, &mut schemas)?;
    add_type::<AdminRecoverySummary>(&config, &mut declarations, &mut schemas)?;
    add_type::<AdminUnsupportedItem>(&config, &mut declarations, &mut schemas)?;
    add_type::<UnsupportedItemReason>(&config, &mut declarations, &mut schemas)?;
    add_type::<JournalAccounting>(&config, &mut declarations, &mut schemas)?;
    add_type::<AdminResult>(&config, &mut declarations, &mut schemas)?;
    add_type::<AdminResponse>(&config, &mut declarations, &mut schemas)?;

    let schema = serde_json::json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": "Notryx protocol contract definitions",
        "description": "구조와 문자열 형식의 스키마다. Rust 역직렬화는 UUID version과 RFC variant 및 seq, 만료 시각, generation의 u64 범위를 추가로 검사한다. node 이름의 경로 규칙, root와 파일/폴더 content 형태도 Rust에서 검사한다. protocol 일치와 만료 여부, 권한은 application에서 검사한다. M0 generation의 SQLite 저장 가능 범위와 문서 크기, session 및 원문 검증은 native runtime과 store가 담당한다. 이 스키마 단독으로 모든 계약을 검증할 수 없다.",
        "$defs": schemas,
    });
    let files = [
        ("index.ts", declarations),
        (
            "schema.json",
            format!("{}\n", serde_json::to_string_pretty(&schema)?),
        ),
    ];
    if !check {
        fs::create_dir_all(&directory)?;
    }
    for (name, expected) in files {
        let path = directory.join(name);
        if check {
            if fs::read_to_string(&path)? != expected {
                return Err(format!("generated contract drift: {name}").into());
            }
        } else {
            fs::write(path, expected)?;
        }
    }
    Ok(())
}
