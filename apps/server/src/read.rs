use crate::{Error, Workspace};
use notryx_domain::{ChangeSeq, NodeId};
use notryx_protocol::{CONTRACT, NodeMetadata, ServerInfo, StrongEtag};

impl Workspace {
    pub async fn changes(
        &mut self,
        after: ChangeSeq,
        through: Option<ChangeSeq>,
    ) -> Result<notryx_protocol::ChangesPage, Error> {
        if self.journal_integrity_failed {
            return Err(Error::RecoveryRequired);
        }
        let result = self.state.changes(after, through).await;
        if matches!(&result, Err(Error::InvalidFormat)) {
            self.journal_integrity_failed = true;
        }
        result
    }

    pub async fn node_state(
        &mut self,
        node_id: NodeId,
    ) -> Result<notryx_protocol::NodeState, Error> {
        self.state
            .node(node_id)
            .await?
            .ok_or(Error::NotFound)?
            .representation()
    }

    pub async fn inventory(
        &mut self,
        after_id: Option<NodeId>,
        limit: u16,
    ) -> Result<notryx_protocol::InventoryPage, Error> {
        let nodes = self.state.inventory_nodes(after_id, limit).await?;
        let mut page = notryx_protocol::InventoryPage {
            workspace_epoch: self.epoch(),
            nodes: Vec::new(),
            next_after_id: None,
        };
        for (index, node) in nodes.iter().take(usize::from(limit)).enumerate() {
            page.nodes.push(node.representation()?);
            page.next_after_id = (index + 1 < nodes.len()).then_some(node.node_id);
            if serde_json::to_vec(&page)
                .map_err(crate::state::serialization_error)?
                .len()
                > 1024 * 1024
            {
                page.nodes.pop();
                page.next_after_id = Some(page.nodes.last().ok_or(Error::InvalidFormat)?.node_id);
                break;
            }
        }
        Ok(page)
    }

    pub async fn server_info(&mut self) -> Result<ServerInfo, Error> {
        let position = self.state.journal_position().await;
        if matches!(&position, Err(Error::InvalidFormat)) {
            self.journal_integrity_failed = true;
        }
        let (head_seq, journal_floor_seq) = position?;
        Ok(ServerInfo {
            protocol: CONTRACT.into(),
            workspace_id: self.state.workspace_id,
            workspace_epoch: self.state.epoch,
            root_id: self.state.root_id,
            head_seq,
            journal_floor_seq,
            ready: self.ready(),
            max_document_bytes: notryx_server_storage::MAX_DOCUMENT_BYTES as u64,
            max_attachment_bytes: notryx_server_storage::MAX_FILE_BYTES as u64,
        })
    }

    pub async fn node_metadata(&mut self, node_id: NodeId) -> Result<NodeMetadata, Error> {
        let record = self.state.node(node_id).await?.ok_or(Error::NotFound)?;
        let etag =
            StrongEtag::from_meta_revision(&record.meta_rev).map_err(|_| Error::InvalidFormat)?;
        Ok(NodeMetadata {
            node_id,
            parent_id: record.parent_id,
            name: record.name,
            kind: record.kind,
            deleted: record.deleted,
            meta_rev: record.meta_rev,
            meta_etag: etag.as_str().into(),
        })
    }
}
