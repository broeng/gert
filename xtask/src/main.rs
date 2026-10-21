//! Dev tooling: `cargo xtask <command>`.

mod fixtures;
mod schema;
mod spec;

use std::path::PathBuf;

use anyhow::{Result, bail};

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("gen-fixtures") => {
            let mut spec_path = None;
            let mut out = None;
            while let Some(a) = args.next() {
                match a.as_str() {
                    "--spec" => spec_path = args.next().map(PathBuf::from),
                    "--out" => out = args.next().map(PathBuf::from),
                    other => bail!(
                        "unknown argument {other:?}; usage: gen-fixtures [--spec FILE] [--out DIR]"
                    ),
                }
            }
            let spec_path = match spec_path {
                Some(p) => p,
                None => spec::default_path(),
            };
            let out = out.unwrap_or_else(|| spec::repo_root().join("tests/fixtures"));
            fixtures::generate(&spec_path, &out)
        }
        other => bail!("unknown xtask: {other:?}; available: gen-fixtures"),
    }
}
