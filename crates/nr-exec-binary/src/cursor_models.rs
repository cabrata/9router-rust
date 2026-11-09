//! Port of open-sse/services/cursorModels.js
//! Cursor live model catalog fetcher

use nr_core::Result;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

const FETCH_TIMEOUT_MS: u64 = 10_000;
const CACHE_TTL_MS: u64 = 5 * 60 * 1000;

lazy_static::lazy_static! {
    static ref CATALOG_CACHE: Arc<Mutex<HashMap<String, CacheEntry>>> = Arc::new(Mutex::new(HashMap::new()));
}

struct CacheEntry {
    expires_at: u64,
    models: Vec<Value>,
}

/// Resolve live Cursor model catalog
pub async fn resolve_cursor_models(
    _credentials: &Value,
    _options: &HashMap<String, Value>,
) -> Result<Option<Value>> {
    // TODO: Implement full model fetching via HTTP/2
    Ok(None)
}

/// Clear the model cache
pub fn clear_cursor_model_cache() {
    // TODO: Clear the cache
}
