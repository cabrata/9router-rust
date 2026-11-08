use crate::connection::Db;
use nr_core::Result;
use serde_json::Value;

pub async fn get_pricing(_db: &Db) -> Result<Value> {
    Ok(Value::Object(Default::default()))
}

pub async fn get_pricing_for_model(_db: &Db, _provider: &str, _model: &str) -> Result<Option<Value>> {
    Ok(None)
}

pub async fn update_pricing(_db: &Db, _updates: Value) -> Result<()> {
    Ok(())
}

pub async fn reset_pricing(_db: &Db, _provider: &str) -> Result<()> {
    Ok(())
}

pub async fn reset_all_pricing(_db: &Db) -> Result<()> {
    Ok(())
}
