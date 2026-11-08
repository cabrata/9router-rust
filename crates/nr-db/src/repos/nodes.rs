use crate::connection::Db;
use nr_core::Result;

#[derive(Clone, Debug)]
pub struct ProviderNode {
    pub id: String,
}

pub async fn get_provider_nodes(_db: &Db) -> Result<Vec<ProviderNode>> {
    Ok(vec![])
}

pub async fn get_provider_node_by_id(_db: &Db, _id: &str) -> Result<Option<ProviderNode>> {
    Ok(None)
}

pub async fn create_provider_node(_db: &Db, _node: ProviderNode) -> Result<ProviderNode> {
    todo!()
}

pub async fn delete_provider_node(_db: &Db, _id: &str) -> Result<bool> {
    Ok(false)
}
