use crate::connection::Db;
use nr_core::Result;

#[derive(Clone, Debug)]
pub struct ProviderConnection {
    pub id: String,
    pub provider: String,
    pub auth_type: String,
}

pub async fn get_provider_connections(_db: &Db) -> Result<Vec<ProviderConnection>> {
    Ok(vec![])
}

pub async fn get_provider_connection_by_id(_db: &Db, _id: &str) -> Result<Option<ProviderConnection>> {
    Ok(None)
}

pub async fn create_provider_connection(_db: &Db, _provider: String, _auth_type: String) -> Result<ProviderConnection> {
    todo!()
}

pub async fn update_provider_connection(_db: &Db, _id: &str) -> Result<Option<ProviderConnection>> {
    Ok(None)
}

pub async fn delete_provider_connection(_db: &Db, _id: &str) -> Result<bool> {
    Ok(false)
}
