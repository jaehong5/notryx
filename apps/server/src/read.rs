use crate::{Error, Workspace};
use notryx_domain::{ChangeSeq, NodeId};
use notryx_protocol::{CONTRACT, NodeMetadata, ServerInfo, StrongEtag};

impl Workspace {
    pub async fn server_info(&mut self) -> Result<ServerInfo, Error> {
        Ok(ServerInfo {
            protocol: CONTRACT.into(),
            workspace_id: self.state.workspace_id,
            workspace_epoch: self.state.epoch,
            root_id: self.state.root_id,
            head_seq: self.state.head_seq().await?,
            // Retention has not been introduced; all committed ChangeSets remain available.
            journal_floor_seq: ChangeSeq::new(0),
            ready: self.ready,
            max_document_bytes: notryx_server_storage::MAX_FILE_BYTES as u64,
        })
    }

    pub async fn node_metadata(&mut self, node_id: NodeId) -> Result<NodeMetadata, Error> {
        let record = self.state.metadata(node_id).await?.ok_or(Error::NotFound)?;
        let etag =
            StrongEtag::from_meta_revision(&record.meta_rev).map_err(|_| Error::InvalidFormat)?;
        Ok(NodeMetadata {
            node_id,
            parent_id: record.parent_id,
            name: record.name,
            kind: record.kind,
            deleted: false,
            meta_rev: record.meta_rev,
            meta_etag: etag.as_str().into(),
        })
    }
}
