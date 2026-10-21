use anyhow::{Result, bail};

fn main() -> Result<()> {
    match std::env::args().nth(1).as_deref() {
        Some("gen-fixtures") => bail!("gen-fixtures: not implemented yet"),
        other => bail!("unknown xtask: {other:?}; available: gen-fixtures"),
    }
}
