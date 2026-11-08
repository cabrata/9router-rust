//! Cross-crate error type. Crates may use `anyhow` internally, but public
//! functions return `nr_core::Result`.

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The client went away or the request was cancelled (JS `AbortError`, HTTP 499).
    #[error("Request aborted")]
    Aborted,
    #[error("timeout: {0}")]
    Timeout(String),
    /// Network/transport failure before an upstream status was received (maps to 502).
    #[error("{0}")]
    Network(String),
    /// Upstream answered with an error status.
    #[error("{message}")]
    Upstream { status: u16, message: String },
    #[error("{0}")]
    BadRequest(String),
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Other(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    pub fn other(msg: impl std::fmt::Display) -> Self {
        Error::Other(msg.to_string())
    }

    /// HTTP status this error should surface as.
    pub fn status(&self) -> u16 {
        match self {
            Error::Aborted => 499,
            Error::Timeout(_) => 504,
            Error::Network(_) => 502,
            Error::Upstream { status, .. } => *status,
            Error::BadRequest(_) => 400,
            Error::NotFound(_) => 404,
            Error::Other(_) => 500,
        }
    }
}

impl From<reqwest::Error> for Error {
    fn from(e: reqwest::Error) -> Self {
        if e.is_timeout() {
            Error::Timeout(e.to_string())
        } else {
            Error::Network(e.to_string())
        }
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Other(format!("json: {e}"))
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Other(format!("io: {e}"))
    }
}
