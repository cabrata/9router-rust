use crate::connection::Db;
use nr_core::Result;
use serde_json::Value;

#[derive(Clone, Debug)]
pub struct Combo {
    pub id: String,
    pub name: String,
    pub kind: Option<String>,
    pub models: Vec<String>,
    pub created_at: String,
    pub updated_at: String,
}

pub async fn get_combos(_db: &Db) -> Result<Vec<Combo>> {
    Ok(vec![])
}

pub async fn get_combo_by_id(_db: &Db, _id: &str) -> Result<Option<Combo>> {
    Ok(None)
}

pub async fn get_combo_by_name(_db: &Db, _name: &str) -> Result<Option<Combo>> {
    Ok(None)
}

pub async fn create_combo(_db: &Db, _name: String, _models: Vec<String>) -> Result<Combo> {
    todo!()
}

pub async fn delete_combo(_db: &Db, _id: &str) -> Result<bool> {
    Ok(false)
}

pub async fn update_combo(_db: &Db, _id: &str, _updates: Value) -> Result<Option<Combo>> {
    Ok(None)
}
