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
    let out = root.join("target/shaderout");
    let status = Command::new("cargo")
        .args(["gpu", "build", "--shader-crate"])
        .arg(root.join("shaders/yuv"))
        .args(["--spirv-builder-source", SPIRV_SOURCE])
        .args(["--spirv-builder-version", SPIRV_VERSION])
        .arg("--output-dir")
        .arg(&out)
        .arg("--auto-install-rust-toolchain")
        .status()
        .context("running cargo gpu (is cargo-gpu installed?)")?;
    if !status.success() {
        bail!("cargo gpu build failed");
    }
    let dest = root.join("src/ui/video_surface/yuv.spv");
    std::fs::copy(out.join("scryglass_yuv_shader.spv"), &dest)?;
    println!("wrote {}", dest.display());
    Ok(())
}
