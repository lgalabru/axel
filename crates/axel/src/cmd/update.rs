//! The public installer owns downloading, verification, and installation.

use anyhow::{Context, Result, bail};

pub fn run() -> Result<()> {
    let status = std::process::Command::new("/bin/bash")
        .args(["-o", "pipefail", "-c", "curl -fsSL https://axel.md/install | sh"])
        .status()
        .context("running the Axel install script")?;
    if !status.success() { bail!("axel update failed: {status}") }
    Ok(())
}
