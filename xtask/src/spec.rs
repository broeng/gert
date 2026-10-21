//! Locating the vendored OpenAPI spec.

use std::path::{Path, PathBuf};

/// The workspace root (xtask lives one level below it).
pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default()
}

/// The vendored spec, `spec/rest-api-openapi.json` in the workspace root.
pub fn default_path() -> PathBuf {
    repo_root().join("spec/rest-api-openapi.json")
}
