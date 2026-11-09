//! SSL-interception proxy for IDE traffic. Port of src/mitm/**.
//!
//! What it does: terminates TLS locally with a certificate signed by a Root CA
//! generated on first use, then either rewrites the request into a call to the
//! local 9Router or passes it through to the real upstream.
//!
//! Layout mirrors the JS tree:
//!   * [`cert`]      — Root CA + per-host leaf certs (rcgen), trust-store install
//!   * [`config`]    — intercepted hosts, URL patterns, model extraction/aliases
//!   * [`dns`]       — hosts-file entries that steer IDE traffic to 127.0.0.1
//!   * [`handlers`]  — per-tool request/response mutation
//!   * [`logger`]    — operator logs + redacted debug dumps
//!   * [`manager`]   — start/stop, settings flags, restart policy
//!   * [`server`]    — the TLS listener and routing decision
//!
//! Security notes:
//!   * `rootCA.key` is written 0600 — it can sign a cert for any hostname.
//!   * Dump files redact Authorization/Cookie/api-key headers.
//!   * Upstream verification is disabled **on the passthrough leg only**, because
//!     the hosts file points the intercepted names back at this process.

pub mod antigravity_ide_version;
pub mod cert;
pub mod config;
pub mod db_reader;
pub mod dns;
pub mod handlers;
pub mod logger;
pub mod manager;
pub mod paths;
pub mod server;

pub use manager::{is_enabled, is_running, start, start_if_enabled, status, stop, trust_cert};
