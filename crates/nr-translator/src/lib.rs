//! Format translation between client wire formats and provider formats.
//! Port of open-sse/translator/ and open-sse/transformer/.
//!
//! Pivots through OpenAI as the intermediate format; a translator registered
//! on an exact `source:target` pair runs as a direct route instead.
//!
//! Ownership while the port is in progress (see AGENTS.md):
//! - `registry`, `state`, `schema`, `concerns`, `formats`, `request`: translator-request agent
//! - `response`, `transformer`: translator-response agent (may append fields to `state`)

pub mod concerns;
pub mod formats;
pub mod registry;
pub mod request;
pub mod response;
pub mod schema;
pub mod state;
pub mod transformer;

pub use registry::{RequestFn, ResponseFn, Registry, TranslationOptions};
pub use registry::{translate_request, translate_response, needs_translation, init_state, init_registry};
pub use state::StreamState;

#[cfg(test)]
mod tests;
