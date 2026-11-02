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
    /// A pending review was asked for on a patch set that is not local.
    #[error("only a local patch set can have a pending review")]
    NotLocal,
    /// Consolidation was asked to move data onto local numbers instead of
    /// a Gerrit change and patch set.
    #[error("local data can only be moved onto a Gerrit change and patch set")]
    LocalTarget,
    #[error("cached timestamp {0:?} is invalid")]
    BadTimestamp(String),
}
