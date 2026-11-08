//! Shared types and the cross-crate contracts of the 9router Rust port.
//!
//! Everything here is a frozen interface: several crates are written in
//! parallel against it. Add items freely; never change an existing signature
//! without updating every caller in the workspace.

pub mod creds;
pub mod error;
pub mod executor;
pub mod format;
pub mod http;
pub mod json;
pub mod log;
pub mod paths;

pub use creds::Credentials;
pub use error::{Error, Result};
pub use executor::{ExecRequest, ExecResult, Executor};
pub use log::Logger;
pub use serde_json::{json, Value as Json};

/// A JSON object, insertion-ordered like a JS object (`serde_json/preserve_order`).
pub type JsonMap = serde_json::Map<String, Json>;
