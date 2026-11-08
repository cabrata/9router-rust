//! Translator registry and entry points. Port of open-sse/translator/index.js.
//!
//! JS translators self-register on import; here `request::register_all` and
//! `response::register_all` fill one static [`Registry`].
//! The translator-request agent ports `translateRequest`, `translateResponse`,
//! `needsTranslation` and `initState` into this file.

use std::collections::HashMap;

use nr_core::{Credentials, Json};

use crate::state::StreamState;

/// Options passed to request translators (JS `translationOptions`).
#[derive(Clone, Debug, Default)]
pub struct TranslationOptions {
    pub preserve_cache_control: bool,
}

/// JS request translator `(model, body, stream, credentials, options) => body`.
pub type RequestFn = fn(model: &str, body: Json, stream: bool, credentials: Option<&mut Credentials>, options: &TranslationOptions) -> Json;

/// JS response translator `(chunk, state) => chunk | chunk[] | null`.
/// `chunk == None` is the end-of-stream flush call.
pub type ResponseFn = fn(chunk: Option<&Json>, state: &mut StreamState) -> Vec<Json>;

#[derive(Default)]
pub struct Registry {
    request: HashMap<(String, String), RequestFn>,
    response: HashMap<(String, String), ResponseFn>,
}

impl Registry {
    /// JS `register(from, to, requestFn, responseFn)`.
    pub fn register(&mut self, from: &str, to: &str, request: Option<RequestFn>, response: Option<ResponseFn>) {
        let key = (from.to_string(), to.to_string());
        if let Some(f) = request {
            self.request.insert(key.clone(), f);
        }
        if let Some(f) = response {
            self.response.insert(key, f);
        }
    }

    pub fn request_fn(&self, from: &str, to: &str) -> Option<RequestFn> {
        self.request.get(&(from.to_string(), to.to_string())).copied()
    }

    pub fn response_fn(&self, from: &str, to: &str) -> Option<ResponseFn> {
        self.response.get(&(from.to_string(), to.to_string())).copied()
    }
}
