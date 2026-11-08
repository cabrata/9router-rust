use crate::connection::Db;
use nr_core::Result;
use serde_json::Value;

pub async fn save_request_detail(_db: &Db, _detail: Value) -> Result<()> {
    Ok(())
}

pub async fn get_request_details(_db: &Db) -> Result<Vec<Value>> {
    Ok(vec![])
}

pub async fn get_request_detail_by_id(_db: &Db, _id: &str) -> Result<Option<Value>> {
    Ok(None)
}

pub async fn get_distinct_providers(_db: &Db) -> Result<Vec<String>> {
    Ok(vec![])
}
