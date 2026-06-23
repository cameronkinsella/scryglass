//! The macOS `.app` bundle wrapped in a `.dmg`. Ad-hoc signed only (no
//! Developer ID), so users must clear quarantine on first launch.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use super::archive::NOTICES;
use super::{Ctx, bundle};

/// (iconset name, pixel size) pairs feeding `iconutil`.
const ICON_SIZES: &[(&str, u32)] = &[
    ("16x16", 16),
    ("16x16@2x", 32),
    ("32x32", 32),
    ("32x32@2x", 64),
    ("128x128", 128),
    ("128x128@2x", 256),
    ("256x256", 256),
    ("256x256@2x", 512),
    ("512x512", 512),
    ("512x512@2x", 1024),
];

pub fn dmg(ctx: &Ctx) -> Result<PathBuf> {
    let stage = ctx.root.join("target/macos");
    if stage.exists() {
        fs::remove_dir_all(&stage)?;
    }
    let app = stage.join("scryglass.app");
    let macos = app.join("Contents/MacOS");
    let resources = app.join("Contents/Resources");
    fs::create_dir_all(&macos)?;
    fs::create_dir_all(&resources)?;

    fs::copy(&ctx.bin, macos.join("scryglass"))?;
    bundle::info_plist(&ctx.version)
        .to_file_xml(app.join("Contents/Info.plist"))
        .context("writing Info.plist")?;
    build_icns(ctx, &stage, &resources.join("scryglass.icns"))?;
    for notice in NOTICES {
        fs::copy(ctx.root.join(notice), resources.join(notice))?;
    }

    // Ad-hoc signature: arm64 binaries must carry one to run at all.
    run(
        Command::new("codesign")
            .args(["--force", "--deep", "--sign", "-"])
            .arg(&app),
        "codesign",
    )?;

    let out = ctx
        .dist
        .join(format!("scryglass-v{}-{}.dmg", ctx.version, ctx.target));
    if out.exists() {
        fs::remove_file(&out)?;
    }
    run(
        Command::new("hdiutil")
            .args(["create", "-volname", "scryglass", "-srcfolder"])
            .arg(&app)
            .args(["-ov", "-format", "UDZO"])
            .arg(&out),
        "hdiutil",
    )?;
    Ok(out)
}

fn build_icns(ctx: &Ctx, stage: &Path, out: &Path) -> Result<()> {
    let iconset = stage.join("icon.iconset");
    fs::create_dir_all(&iconset)?;
    let src = ctx.root.join("assets/icon.png");
    for (name, px) in ICON_SIZES {
        let px = px.to_string();
        run(
            Command::new("sips")
                .args(["-z", &px, &px])
                .arg(&src)
                .arg("--out")
                .arg(iconset.join(format!("icon_{name}.png"))),
            "sips",
        )?;
    }
    run(
        Command::new("iconutil")
            .args(["-c", "icns"])
            .arg(&iconset)
            .arg("-o")
            .arg(out),
        "iconutil",
    )?;
    Ok(())
}

fn run(cmd: &mut Command, name: &str) -> Result<()> {
    let status = cmd.status().with_context(|| format!("running {name}"))?;
    if !status.success() {
        bail!("{name} failed");
    }
    Ok(())
}
