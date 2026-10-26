use super::{State, db_error, parse};
use crate::Error;
use notryx_domain::{MetaRevision, NodeId};
use notryx_protocol::NodeKind;
use sqlx::Row;

pub(crate) struct MetadataRecord {
    pub parent_id: Option<NodeId>,
    pub name: String,
    pub kind: NodeKind,
    pub meta_rev: MetaRevision,
}

impl State {
    pub async fn metadata(&mut self, node_id: NodeId) -> Result<Option<MetadataRecord>, Error> {
        let row = sqlx::query("SELECT parent_id,name,kind,meta_rev FROM nodes WHERE node_id=?")
            .bind(node_id.to_string())
            .fetch_optional(&mut self.connection)
            .await
            .map_err(db_error)?;
        row.map(|row| {
            let parent_id: Option<String> = row.try_get("parent_id").map_err(db_error)?;
            let kind = match row.try_get::<&str, _>("kind").map_err(db_error)? {
                "file" => NodeKind::File,
                "folder" => NodeKind::Folder,
                _ => return Err(Error::InvalidFormat),
            };
            Ok(MetadataRecord {
                parent_id: parent_id.as_deref().map(parse).transpose()?,
                name: row.try_get("name").map_err(db_error)?,
                kind,
                meta_rev: row
                    .try_get::<String, _>("meta_rev")
                    .map_err(db_error)?
                    .try_into()
                    .map_err(|_| Error::InvalidFormat)?,
            })
        })
        .transpose()
    }
}
