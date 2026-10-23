/// Everything that can go wrong in the cache.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("cached JSON could not be (de)serialized: {0}")]
    Json(#[from] serde_json::Error),
    /// The database was written by a newer version of this application.
    /// It is left untouched.
    #[error(
        "cache database has schema version {found}, but this build only knows up to {supported}"
    )]
    SchemaTooNew { found: u32, supported: u32 },
    /// Another server row already has this URL.
    #[error("another server already uses the URL {0}")]
    ServerUrlTaken(String),
    #[error("no server with this id")]
    NoSuchServer,
    #[error("cached timestamp {0:?} is invalid")]
    BadTimestamp(String),
}
