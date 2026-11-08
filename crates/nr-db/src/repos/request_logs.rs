use crate::connection::Db;
use nr_core::Result;
use serde_json::Value;

pub async fn insert_request_log(_db: &Db, _log: Value) -> Result<()> {
    Ok(())
}

pub async fn get_recent_logs(_db: &Db, _limit: usize) -> Result<Vec<Value>> {
    Ok(vec![])
}

pub async fn get_api_key_monitoring(_db: &Db, _key_id: &str) -> Result<Value> {
    Ok(Value::Object(Default::default()))
}

pub async fn acquire_api_key_request(_db: &Db, _key: Value) -> Result<Value> {
    Ok(Value::Object(Default::default()))
}

pub struct RequestLogStore;

impl RequestLogStore {
    pub async fn run<F, T>(_ctx: Value, f: F) -> Result<T>
    where
        F: std::future::Future<Output = T>,
    {
        Ok(f.await)
    }
}

pub static REQUEST_LOG_STORE: RequestLogStore = RequestLogStore;

pub fn request_log_store() -> &'static RequestLogStore {
    &REQUEST_LOG_STORE
}
