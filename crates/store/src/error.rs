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
    /// A pending review was asked for on a patch set that is not a local
    /// patch set of the server.
    #[error("only a local patch set can have a pending review")]
    NotLocal,
    /// A pending review was asked for on a local patch set whose change is
    /// consolidated or closed: what it was given on is on Gerrit already,
    /// so a review belongs on the Gerrit change.
    #[error("the local change is {} and takes no new pending reviews", .0.as_str())]
    NotReviewable(model::LocalChangeState),
    /// Consolidation was asked to move a local change onto a Gerrit change
    /// other than the one it already belongs to.
    #[error("the local change already belongs to Gerrit change {0}")]
    OtherGerritChange(model::ChangeNumber),
    /// Consolidation was asked to move data onto local numbers instead of
    /// a Gerrit change and patch set.
    #[error("local data can only be moved onto a Gerrit change and patch set")]
    LocalTarget,
    /// The draft is being sent to Gerrit with a local review right now
    /// (claimed by a consolidation post); it can be edited or deleted
    /// again once that is done.
    #[error("the comment is being sent to Gerrit with your local review")]
    DraftBeingSent,
    /// A held vote whose label may only be dropped (an approval given on
    /// another commit than the one it would go to) was asked to be
    /// published.
    #[error(
        "the {0} vote is an approval given on another commit; approvals are only sent on the \
         exact commit they were given on, so it can only be dropped"
    )]
    DropOnly(String),
    #[error("cached timestamp {0:?} is invalid")]
    BadTimestamp(String),
}
