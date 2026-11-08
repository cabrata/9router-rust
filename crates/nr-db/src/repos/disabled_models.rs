use crate::connection::Db;
use nr_core::Result;
use std::collections::HashMap;

pub async fn get_disabled_models(_db: &Db) -> Result<HashMap<String, Vec<String>>> {
    Ok(HashMap::new())
}

pub async fn get_disabled_by_provider(_db: &Db, _provider: &str) -> Result<Vec<String>> {
    Ok(vec![])
}

pub async fn disable_models(_db: &Db, _updates: HashMap<String, Vec<String>>) -> Result<()> {
    Ok(())
}

pub async fn enable_models(_db: &Db, _provider: &str, _models: Vec<String>) -> Result<()> {
    Ok(())
}
