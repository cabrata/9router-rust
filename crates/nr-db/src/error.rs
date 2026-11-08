use thiserror::Error;

#[derive(Error, Debug)]
pub enum DbError {
    #[error("SQLite error: {0}")]
    Sqlite(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::error::Error),

    #[error("Invalid data: {0}")]
    InvalidData(String),

    #[error("Database not initialized")]
    NotInitialized,

    #[error("Row not found")]
    NotFound,

    #[error("Migration failed: {0}")]
    MigrationFailed(String),

    #[error("Transaction failed: {0}")]
    TransactionFailed(String),
}

impl From<rusqlite::Error> for DbError {
    fn from(err: rusqlite::Error) -> Self {
        DbError::Sqlite(err.to_string())
    }
}

impl From<DbError> for nr_core::Error {
    fn from(err: DbError) -> Self {
        nr_core::Error::other(err.to_string())
    }
}
