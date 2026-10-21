/// A failed refresh. The cache is left as it was.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("server request failed: {0}")]
    Api(#[from] gerrit_api::Error),
    #[error("cache error: {0}")]
    Store(#[from] store::Error),
}
