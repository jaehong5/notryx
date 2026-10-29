use crate::{Error, Session, json};
use notryx_domain::NodeId;
use notryx_protocol::TrashPage;

impl Session {
    pub async fn trash(&self, after: Option<NodeId>, limit: u16) -> Result<TrashPage, Error> {
        if !(1..=100).contains(&limit) {
            return Err(Error::InvalidRequest);
        }
        let path = match after {
            Some(id) => format!("/v1/trash?after_id={id}&limit={limit}"),
            None => format!("/v1/trash?limit={limit}"),
        };
        let page: TrashPage = json(self.get(&path).await?).await?;
        if page.workspace_epoch != self.binding.workspace_epoch {
            return Err(Error::Binding);
        }
        if page.nodes.len() > usize::from(limit) {
            return Err(Error::Contract);
        }
        let mut previous = after.map(|id| id.to_string());
        for node in &page.nodes {
            let current = node.node_id.to_string();
            if node.node_id == node.parent_id
                || node.meta_rev.as_str().len() > 256
                || previous.as_ref().is_some_and(|last| last >= &current)
            {
                return Err(Error::Contract);
            }
            previous = Some(current);
        }
        if let Some(next) = page.next_after_id
            && page.nodes.last().map(|node| node.node_id) != Some(next)
        {
            return Err(Error::Contract);
        }
        Ok(page)
    }
}
