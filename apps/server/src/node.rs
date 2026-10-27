use crate::{Document, Error, NodeSnapshot, Outcome};
use notryx_domain::ChangeSeq;
use notryx_protocol::{NodeKind, NodeState, StrongEtag};

impl From<&Document> for NodeSnapshot {
    fn from(document: &Document) -> Self {
        Self {
            node_id: document.node_id,
            parent_id: Some(document.parent_id),
            name: document.name.clone(),
            kind: NodeKind::File,
            content_rev: Some(document.content_rev.clone()),
            meta_rev: document.meta_rev.clone(),
            content_hash: Some(document.content_hash.clone()),
            size: Some(document.size),
            children_generation: 0,
            last_change_seq: document.last_change_seq,
            deleted: false,
        }
    }
}

impl NodeSnapshot {
    pub fn etag(&self) -> Result<String, Error> {
        // Keep the schema 4 Created receipt's field order and encoding stable.
        let bytes = serde_json::to_vec(self).map_err(crate::state::serialization_error)?;
        Ok(format!("\"n.{}\"", crate::hash(&bytes)))
    }

    pub fn representation(&self) -> Result<NodeState, Error> {
        let meta_etag =
            StrongEtag::from_meta_revision(&self.meta_rev).map_err(|_| Error::InvalidFormat)?;
        let content_etag = self
            .content_rev
            .as_ref()
            .map(StrongEtag::from_content_revision)
            .transpose()
            .map_err(|_| Error::InvalidFormat)?;
        Ok(NodeState {
            node_id: self.node_id,
            parent_id: self.parent_id,
            name: self.name.clone(),
            kind: self.kind,
            deleted: self.deleted,
            content_rev: self.content_rev.clone(),
            content_etag: content_etag.map(|etag| etag.as_str().to_owned()),
            meta_rev: self.meta_rev.clone(),
            meta_etag: meta_etag.as_str().into(),
            node_etag: self.etag()?,
            content_hash: self
                .content_hash
                .as_ref()
                .map(|hash| format!("sha256:{hash}")),
            size: self.size.map(|size| size.to_string()),
            children_generation: ChangeSeq::new(self.children_generation),
            last_change_seq: self.last_change_seq,
        })
    }
}

pub(crate) fn receipt_etag(outcome: &Outcome) -> Result<Option<String>, Error> {
    match outcome {
        Outcome::Saved(document) => NodeSnapshot::from(document).etag().map(Some),
        Outcome::Created(node)
        | Outcome::Moved(node)
        | Outcome::Deleted(node)
        | Outcome::Restored(node) => node.etag().map(Some),
        _ => Ok(None),
    }
}

pub(crate) fn legacy_receipt_etag(outcome: &Outcome) -> Result<Option<String>, Error> {
    match outcome {
        Outcome::Saved(document) => {
            let bytes = serde_json::to_vec(document).map_err(crate::state::serialization_error)?;
            Ok(Some(format!("\"n.{}\"", crate::hash(&bytes))))
        }
        _ => receipt_etag(outcome),
    }
}

pub(crate) fn validate_receipt_etag(outcome: &Outcome, etag: Option<&str>) -> Result<(), Error> {
    if receipt_etag(outcome)?.as_deref() == etag || legacy_receipt_etag(outcome)?.as_deref() == etag
    {
        Ok(())
    } else {
        Err(Error::InvalidFormat)
    }
}
