//! The Linux AppImage. Mirrors the AppDir layout the release used to build by
//! hand, plus the embedded license notices.

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result, bail};

use super::archive::NOTICES;
use super::{Ctx, desktop};

const APPIMAGETOOL_URL: &str = "https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-x86_64.AppImage";

pub fn appimage(ctx: &Ctx) -> Result<PathBuf> {
    let arch = ctx.target.split('-').next().unwrap_or("x86_64");
    let appdir = ctx.root.join("target/AppDir");
    if appdir.exists() {
        fs::remove_dir_all(&appdir)?;
    }
    let bindir = appdir.join("usr/bin");
    fs::create_dir_all(&bindir)?;
    fs::copy(&ctx.bin, bindir.join("scryglass"))?;

    fs::copy(
        ctx.root.join("assets/icon.png"),
        appdir.join("scryglass.png"),
    )?;
    symlink("usr/bin/scryglass", appdir.join("AppRun"))?;
    fs::write(appdir.join("scryglass.desktop"), desktop::entry())?;

    let docdir = appdir.join("usr/share/doc/scryglass");
    fs::create_dir_all(&docdir)?;
    for notice in NOTICES {
        fs::copy(ctx.root.join(notice), docdir.join(notice))?;
    }

    let tool = ensure_appimagetool(ctx)?;
    let out = ctx
        .dist
        .join(format!("scryglass-v{}-{arch}.AppImage", ctx.version));
    let status = Command::new(&tool)
        // Extract-and-run avoids needing FUSE on CI runners.
        .env("APPIMAGE_EXTRACT_AND_RUN", "1")
        .arg(&appdir)
        .arg(&out)
        .status()
        .context("running appimagetool")?;
    if !status.success() {
        bail!("appimagetool failed");
    }
    Ok(out)
}

fn ensure_appimagetool(ctx: &Ctx) -> Result<PathBuf> {
    let tool = ctx.root.join("target/appimagetool");
    if !tool.exists() {
        let status = Command::new("curl")
            .args(["-fsSL", "-o"])
            .arg(&tool)
            .arg(APPIMAGETOOL_URL)
            .status()
            .context("downloading appimagetool")?;
        if !status.success() {
            bail!("could not download appimagetool");
        }
        let mut perms = fs::metadata(&tool)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&tool, perms)?;
    }
    Ok(tool)
}
