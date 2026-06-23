//! `cargo xtask package` — build the host platform's two release artifacts:
//! a slim binary archive (zip on Windows, tar.gz on Unix) and the OS-native
//! application (Inno installer / AppImage / dmg). Each is one file, with the
//! license notices embedded inside.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

mod archive;
mod bundle;
mod desktop;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

/// What the per-OS builders need to assemble an artifact.
pub struct Ctx {
    /// Workspace root.
    pub root: PathBuf,
    /// Crate version, e.g. "0.2.0".
    pub version: String,
    /// Target triple the binary was built for.
    pub target: String,
    /// The built release executable.
    pub bin: PathBuf,
    /// Output directory (`target/dist`).
    pub dist: PathBuf,
}

/// Parsed `package` arguments.
struct Options {
    /// Explicit target triple. `None` packages the host build from `target/release`.
    target: Option<String>,
    /// Package the existing binary instead of (re)building it.
    no_build: bool,
    /// Feature flags forwarded verbatim to `cargo build`.
    feature_args: Vec<String>,
    /// Feature summary for the log line.
    feature_desc: String,
}

fn parse_args(args: &[String]) -> Result<Options> {
    let mut target = None;
    let mut no_build = false;
    let mut feature_args = Vec::new();
    let mut desc = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--target" => {
                target = Some(iter.next().context("--target needs a value")?.clone());
            }
            "--no-build" => no_build = true,
            "--all-features" => {
                feature_args.push("--all-features".into());
                desc.push("all".into());
            }
            "--no-default-features" => {
                feature_args.push("--no-default-features".into());
                desc.push("no-default".into());
            }
            "--features" => {
                let list = iter.next().context("--features needs a value")?.clone();
                desc.push(list.clone());
                feature_args.push("--features".into());
                feature_args.push(list);
            }
            other => bail!("unknown package argument: {other}"),
        }
    }
    let feature_desc = if desc.is_empty() {
        "default".to_string()
    } else {
        desc.join(" ")
    };
    Ok(Options {
        target,
        no_build,
        feature_args,
        feature_desc,
    })
}

/// Run `package [--target <triple>] [--all-features | --features <list> |
/// --no-default-features] [--no-build]`. With no `--target` it builds and
/// packages the host binary from `target/release`, where plain `cargo build`
/// writes. Default features unless overridden.
pub fn run(args: &[String]) -> Result<()> {
    let opts = parse_args(args)?;

    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("workspace root")?
        .to_path_buf();
    let version = crate_version(&root)?;
    // The artifact is labelled with the host triple when no target is given,
    // even though that binary lives in the bare `target/release` directory.
    let name_target = match &opts.target {
        Some(t) => t.clone(),
        None => host_triple()?,
    };
    println!(
        "Packaging scryglass v{version} for {name_target} (features: {})",
        opts.feature_desc
    );

    let bin = ensure_binary(
        &root,
        opts.target.as_deref(),
        &opts.feature_args,
        opts.no_build,
    )?;
    let dist = root.join("target/dist");
    std::fs::create_dir_all(&dist)?;

    let ctx = Ctx {
        root,
        version,
        target: name_target,
        bin,
        dist,
    };

    let slim = archive::slim(&ctx)?;
    println!("wrote {}", slim.display());

    let app = build_app(&ctx)?;
    println!("wrote {}", app.display());
    Ok(())
}

#[cfg(target_os = "windows")]
fn build_app(ctx: &Ctx) -> Result<PathBuf> {
    windows::installer(ctx)
}
#[cfg(target_os = "macos")]
fn build_app(ctx: &Ctx) -> Result<PathBuf> {
    macos::dmg(ctx)
}
#[cfg(target_os = "linux")]
fn build_app(ctx: &Ctx) -> Result<PathBuf> {
    linux::appimage(ctx)
}
#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
fn build_app(_ctx: &Ctx) -> Result<PathBuf> {
    bail!("packaging the OS application is not supported on this host")
}

/// The crate version, read from cargo metadata rather than parsed by hand.
fn crate_version(root: &Path) -> Result<String> {
    let meta = cargo_metadata::MetadataCommand::new()
        .manifest_path(root.join("Cargo.toml"))
        .no_deps()
        .exec()
        .context("cargo metadata")?;
    let pkg = meta
        .packages
        .iter()
        .find(|p| p.name.as_str() == "scryglass")
        .context("scryglass package not found in metadata")?;
    Ok(pkg.version.to_string())
}

/// The host target triple, from `rustc -vV`.
fn host_triple() -> Result<String> {
    let out = Command::new("rustc")
        .arg("-vV")
        .output()
        .context("running rustc -vV")?;
    let text = String::from_utf8(out.stdout).context("rustc -vV output was not utf-8")?;
    text.lines()
        .find_map(|l| l.strip_prefix("host: "))
        .map(str::to_string)
        .context("no host line in rustc -vV")
}

/// Build the release binary with the requested features (unless `--no-build`),
/// then return its path. No target means the host build in `target/release`,
/// where plain `cargo build` writes. `--target` selects `target/<triple>/release`.
/// Cargo's own fingerprint decides freshness: an up-to-date tree is a no-op, and
/// any source change (committed or not) rebuilds.
fn ensure_binary(
    root: &Path,
    target: Option<&str>,
    feature_args: &[String],
    no_build: bool,
) -> Result<PathBuf> {
    let mut dir = root.join("target");
    if let Some(t) = target {
        dir = dir.join(t);
    }
    let bin = dir.join("release").join(bin_filename());

    if !no_build {
        let mut cmd = Command::new("cargo");
        cmd.current_dir(root).args(["build", "--release"]);
        if let Some(t) = target {
            cmd.args(["--target", t]);
        }
        cmd.args(feature_args);
        if !cmd.status().context("cargo build")?.success() {
            bail!("cargo build failed");
        }
    }
    if !bin.exists() {
        bail!(
            "no binary at {} (build it, or drop --no-build)",
            bin.display()
        );
    }
    Ok(bin)
}

/// `scryglass-v{version}-{target}`: the slim archive's name and inner folder.
pub fn stem(version: &str, target: &str) -> String {
    format!("scryglass-v{version}-{target}")
}

/// The executable's filename on this host.
pub fn bin_filename() -> &'static str {
    if cfg!(windows) {
        "scryglass.exe"
    } else {
        "scryglass"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn svec(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parse_defaults_to_host_build_with_default_features() {
        let o = parse_args(&[]).unwrap();
        assert!(o.target.is_none());
        assert!(!o.no_build);
        assert!(o.feature_args.is_empty());
        assert_eq!(o.feature_desc, "default");
    }

    #[test]
    fn parse_forwards_target_and_feature_flags() {
        let o = parse_args(&svec(&["--target", "x64", "--all-features", "--no-build"])).unwrap();
        assert_eq!(o.target.as_deref(), Some("x64"));
        assert!(o.no_build);
        assert_eq!(o.feature_args, ["--all-features"]);
        assert_eq!(o.feature_desc, "all");
    }

    #[test]
    fn parse_passes_a_feature_list_through() {
        let o = parse_args(&svec(&[
            "--no-default-features",
            "--features",
            "video,heif",
        ]))
        .unwrap();
        assert_eq!(
            o.feature_args,
            ["--no-default-features", "--features", "video,heif"]
        );
        assert_eq!(o.feature_desc, "no-default video,heif");
    }

    #[test]
    fn parse_rejects_unknown_args_and_missing_values() {
        assert!(parse_args(&svec(&["--bogus"])).is_err());
        assert!(parse_args(&svec(&["--target"])).is_err());
        assert!(parse_args(&svec(&["--features"])).is_err());
    }

    #[test]
    fn stem_carries_version_and_target() {
        assert_eq!(
            stem("0.2.0", "x86_64-pc-windows-msvc"),
            "scryglass-v0.2.0-x86_64-pc-windows-msvc"
        );
    }

    #[test]
    fn bin_filename_matches_host() {
        let name = bin_filename();
        if cfg!(windows) {
            assert_eq!(name, "scryglass.exe");
        } else {
            assert_eq!(name, "scryglass");
        }
    }
}
