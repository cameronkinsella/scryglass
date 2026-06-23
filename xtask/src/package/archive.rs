//! The slim binary archive: zip on Windows, tar.gz on Unix. The binary plus
//! the license notices and README, under one `scryglass-v{ver}-{target}/`
//! folder so extraction yields a single tidy directory.

use std::fs::{self, File};
use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result};

use super::{Ctx, bin_filename, stem};

/// Files placed in the slim archive.
const ARCHIVE_DOCS: &[&str] = &["LICENSE", "THIRD-PARTY.md", "README.md"];

/// License notices embedded in every OS application bundle (LGPL/BSD/UnRAR
/// terms require these to travel with the binary).
#[allow(dead_code)] // used only by the Linux/macOS builders
pub const NOTICES: &[&str] = &["LICENSE", "THIRD-PARTY.md"];

/// Build the slim archive and return its path.
pub fn slim(ctx: &Ctx) -> Result<PathBuf> {
    let folder = stem(&ctx.version, &ctx.target);
    if cfg!(windows) {
        zip(ctx, &folder)
    } else {
        targz(ctx, &folder)
    }
}

fn zip(ctx: &Ctx, folder: &str) -> Result<PathBuf> {
    use zip::write::SimpleFileOptions;

    let out = ctx.dist.join(format!("{folder}.zip"));
    let file = File::create(&out).with_context(|| format!("create {}", out.display()))?;
    let mut zw = zip::ZipWriter::new(file);
    let opts = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);

    zw.start_file(format!("{folder}/{}", bin_filename()), opts)?;
    zw.write_all(&fs::read(&ctx.bin)?)?;

    for doc in ARCHIVE_DOCS {
        zw.start_file(format!("{folder}/{doc}"), opts)?;
        zw.write_all(&fs::read(ctx.root.join(doc)).with_context(|| format!("read {doc}"))?)?;
    }
    zw.finish()?;
    Ok(out)
}

fn targz(ctx: &Ctx, folder: &str) -> Result<PathBuf> {
    use flate2::Compression;
    use flate2::write::GzEncoder;

    let out = ctx.dist.join(format!("{folder}.tar.gz"));
    let file = File::create(&out).with_context(|| format!("create {}", out.display()))?;
    let mut tar = tar::Builder::new(GzEncoder::new(file, Compression::default()));

    let exe = fs::read(&ctx.bin)?;
    add(
        &mut tar,
        &format!("{folder}/{}", bin_filename()),
        &exe,
        0o755,
    )?;
    for doc in ARCHIVE_DOCS {
        let data = fs::read(ctx.root.join(doc)).with_context(|| format!("read {doc}"))?;
        add(&mut tar, &format!("{folder}/{doc}"), &data, 0o644)?;
    }
    tar.into_inner()?.finish()?;
    Ok(out)
}

/// Append one in-memory file to a tar with an explicit mode, so the executable
/// bit survives regardless of the host that builds the archive.
fn add<W: Write>(tar: &mut tar::Builder<W>, path: &str, data: &[u8], mode: u32) -> Result<()> {
    let mut header = tar::Header::new_gnu();
    header.set_size(data.len() as u64);
    header.set_mode(mode);
    tar.append_data(&mut header, path, data)?;
    Ok(())
}
