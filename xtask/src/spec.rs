//! Locating the OpenAPI spec shipped with gerrit-sdk-rust.

use std::{
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, bail};
use serde_json::Value;

/// The workspace root (xtask lives one level below it).
pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default()
}

/// Finds `rest-api-openapi.json` at the root of the SDK's git checkout, via
/// `cargo metadata` (package `gerrit_client`, whose manifest is in `client/`).
pub fn locate() -> Result<PathBuf> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let out = Command::new(cargo)
        .args(["metadata", "--format-version", "1"])
        .current_dir(repo_root())
        .output()
        .context("running cargo metadata")?;
    if !out.status.success() {
        bail!(
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let meta: Value = serde_json::from_slice(&out.stdout).context("parsing cargo metadata")?;
    let manifest = meta["packages"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|p| matches!(p["name"].as_str(), Some("gerrit_client" | "gerrit-sdk")))
        .and_then(|p| p["manifest_path"].as_str())
        .context("gerrit_client package not found in cargo metadata")?;
    // <checkout>/client/Cargo.toml -> <checkout>
    let root = Path::new(manifest)
        .ancestors()
        .nth(2)
        .context("unexpected manifest path")?;
    let spec = root.join("rest-api-openapi.json");
    if !spec.is_file() {
        bail!("{} not found; pass --spec <path>", spec.display());
    }
    Ok(spec)
}
