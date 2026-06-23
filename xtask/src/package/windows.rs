//! The Windows installer, compiled from the committed Inno Setup script. The
//! slim zip is produced separately by `archive::slim`.

use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result, bail};

use super::{Ctx, stem};

pub fn installer(ctx: &Ctx) -> Result<PathBuf> {
    let base = format!("{}-setup", stem(&ctx.version, &ctx.target));
    let iss = ctx.root.join("packaging/windows/scryglass.iss");

    // ISCC flags take no space: /Dname=value, /Fbasename, /Ooutdir.
    let status = Command::new("iscc")
        .arg(format!("/DAppVersion={}", ctx.version))
        .arg(format!("/DBinPath={}", ctx.bin.display()))
        .arg(format!("/DSrcRoot={}", ctx.root.display()))
        .arg(format!("/F{base}"))
        .arg(format!("/O{}", ctx.dist.display()))
        .arg(&iss)
        .status()
        .context("running iscc (is Inno Setup installed?)")?;
    if !status.success() {
        bail!("iscc failed");
    }
    Ok(ctx.dist.join(format!("{base}.exe")))
}
