use crate::connection::Db;
use nr_core::Result;

#[derive(Clone, Debug)]
pub struct ProxyPool {
    pub id: String,
}

pub async fn get_proxy_pools(_db: &Db) -> Result<Vec<ProxyPool>> {
    Ok(vec![])
}

pub async fn get_proxy_pool_by_id(_db: &Db, _id: &str) -> Result<Option<ProxyPool>> {
    Ok(None)
}

pub async fn create_proxy_pool(_db: &Db, _pool: ProxyPool) -> Result<ProxyPool> {
    todo!()
}

pub async fn delete_proxy_pool(_db: &Db, _id: &str) -> Result<bool> {
    Ok(false)
}
