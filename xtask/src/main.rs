//! Build-time tasks: compile the shader, and package release artifacts.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

mod package;

const SPIRV_SOURCE: &str = "https://github.com/Rust-GPU/rust-gpu";
const SPIRV_VERSION: &str = "v0.10.0-alpha.1";

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("build-shaders") => build_shaders(),
        Some("package") => package::run(&args[1..]),
        _ => bail!("usage: cargo xtask <build-shaders|package>"),
    }
}

fn build_shaders() -> Result<()> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root");
    build_shader(
        root,
        "shaders/yuv",
        "scryglass_yuv_shader.spv",
        "src/ui/video_surface/yuv.spv",
    )?;
    build_shader(
        root,
        "shaders/image",
        "scryglass_image_shader.spv",
        "src/ui/image_surface/image.spv",
    )?;
    Ok(())
}

/// Compile one rust-gpu shader crate with cargo-gpu and copy its SPIR-V to the
/// committed path the app includes.
fn build_shader(root: &Path, crate_dir: &str, spv_name: &str, dest_rel: &str) -> Result<()> {
    let out = root.join("target/shaderout");
    let status = Command::new("cargo")
        .args(["gpu", "build", "--shader-crate"])
        .arg(root.join(crate_dir))
        .args(["--spirv-builder-source", SPIRV_SOURCE])
        .args(["--spirv-builder-version", SPIRV_VERSION])
        .arg("--output-dir")
        .arg(&out)
        .arg("--auto-install-rust-toolchain")
        .status()
        .context("running cargo gpu (is cargo-gpu installed?)")?;
    if !status.success() {
        bail!("cargo gpu build failed for {crate_dir}");
    }
    let dest = root.join(dest_rel);
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::copy(out.join(spv_name), &dest)?;
    println!("wrote {}", dest.display());
    Ok(())
}
