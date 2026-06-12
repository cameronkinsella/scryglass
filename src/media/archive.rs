//! Browsing images inside archives (zip, tar, 7z, rar).
//!
//! An [`ArchiveIndex`] lists an archive's image entries once, then serves
//! entry bytes on demand. Entry names act as the navigation paths. The
//! rest of the app keys caches and messages by `PathBuf` exactly as it
//! does for directories on disk.
//!
//! Random access cost varies by format: zip is cheap, tar and solid 7z
//! re-scan from the start of the archive per read, rar walks headers.
//! Reads run on blocking worker threads either way.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};

use crate::config::AppConfig;

/// Archive container formats we can open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Zip,
    Tar,
    TarGz,
    SevenZ,
    #[cfg(feature = "rar")]
    Rar,
}

/// Returns true if the path looks like a supported archive.
pub fn is_archive(path: &Path) -> bool {
    kind_of(path).is_some()
}

fn kind_of(path: &Path) -> Option<Kind> {
    let name = path.file_name()?.to_str()?.to_ascii_lowercase();
    let ext = name.rsplit('.').next()?;
    match ext {
        "zip" | "cbz" => Some(Kind::Zip),
        "tar" => Some(Kind::Tar),
        "tgz" => Some(Kind::TarGz),
        "gz" if name.ends_with(".tar.gz") => Some(Kind::TarGz),
        "7z" | "cb7" => Some(Kind::SevenZ),
        #[cfg(feature = "rar")]
        "rar" | "cbr" => Some(Kind::Rar),
        _ => None,
    }
}

/// Metadata for one image entry inside an archive.
#[derive(Debug, Clone)]
struct Entry {
    /// The entry's name exactly as stored in the archive.
    name: String,
    /// Uncompressed size in bytes.
    size: u64,
}

/// An opened archive: its image entry listing plus on-demand entry reads.
#[derive(Debug)]
pub struct ArchiveIndex {
    pub archive_path: PathBuf,
    kind: Kind,
    /// Keyed by the entry-name-as-path used for navigation.
    entries: HashMap<PathBuf, Entry>,
}

impl ArchiveIndex {
    /// List the archive's image entries. Blocking, run on a worker.
    pub fn open(path: &Path) -> Result<Self> {
        let kind = kind_of(path).ok_or_else(|| anyhow!("not a supported archive"))?;
        let listing = match kind {
            Kind::Zip => list_zip(path)?,
            Kind::Tar => list_tar(open_tar(path)?)?,
            Kind::TarGz => list_tar(open_tar_gz(path)?)?,
            Kind::SevenZ => list_7z(path)?,
            #[cfg(feature = "rar")]
            Kind::Rar => list_rar(path)?,
        };

        let entries = listing
            .into_iter()
            .filter(|e| {
                Path::new(&e.name)
                    .extension()
                    .and_then(|x| x.to_str())
                    .is_some_and(AppConfig::is_supported_extension)
            })
            .map(|e| (PathBuf::from(&e.name), e))
            .collect();

        Ok(Self {
            archive_path: path.to_path_buf(),
            kind,
            entries,
        })
    }

    /// The image entries as navigation paths, in name order.
    pub fn image_entries(&self) -> Vec<PathBuf> {
        let mut paths: Vec<PathBuf> = self.entries.keys().cloned().collect();
        paths.sort_by(|a, b| crate::nav::name_cmp(a.as_os_str(), b.as_os_str()));
        paths
    }

    /// Uncompressed size of an entry.
    pub fn entry_size(&self, entry: &Path) -> Option<u64> {
        self.entries.get(entry).map(|e| e.size)
    }

    /// Read one entry's bytes. Blocking, run on a worker.
    pub fn read(&self, entry: &Path) -> Result<Vec<u8>> {
        let name = &self
            .entries
            .get(entry)
            .ok_or_else(|| anyhow!("no such entry in archive"))?
            .name;
        match self.kind {
            Kind::Zip => read_zip(&self.archive_path, name),
            Kind::Tar => read_tar(open_tar(&self.archive_path)?, name),
            Kind::TarGz => read_tar(open_tar_gz(&self.archive_path)?, name),
            Kind::SevenZ => read_7z(&self.archive_path, name),
            #[cfg(feature = "rar")]
            Kind::Rar => read_rar(&self.archive_path, name),
        }
    }
}

// --- zip ---

fn list_zip(path: &Path) -> Result<Vec<Entry>> {
    let mut archive = zip::ZipArchive::new(BufReader::new(File::open(path)?))?;
    let mut out = Vec::new();
    for i in 0..archive.len() {
        let file = archive.by_index_raw(i)?;
        if !file.is_dir() {
            out.push(Entry {
                name: file.name().to_string(),
                size: file.size(),
            });
        }
    }
    Ok(out)
}

fn read_zip(path: &Path, name: &str) -> Result<Vec<u8>> {
    let mut archive = zip::ZipArchive::new(BufReader::new(File::open(path)?))?;
    let mut file = archive.by_name(name)?;
    let mut buf = Vec::with_capacity(file.size() as usize);
    file.read_to_end(&mut buf)?;
    Ok(buf)
}

// --- tar / tar.gz ---

fn open_tar(path: &Path) -> Result<tar::Archive<BufReader<File>>> {
    Ok(tar::Archive::new(BufReader::new(File::open(path)?)))
}

fn open_tar_gz(path: &Path) -> Result<tar::Archive<flate2::read::GzDecoder<BufReader<File>>>> {
    Ok(tar::Archive::new(flate2::read::GzDecoder::new(
        BufReader::new(File::open(path)?),
    )))
}

fn list_tar<R: Read>(mut archive: tar::Archive<R>) -> Result<Vec<Entry>> {
    let mut out = Vec::new();
    for entry in archive.entries()? {
        let entry = entry?;
        if entry.header().entry_type().is_file() {
            out.push(Entry {
                name: entry.path()?.to_string_lossy().into_owned(),
                size: entry.header().size()?,
            });
        }
    }
    Ok(out)
}

fn read_tar<R: Read>(mut archive: tar::Archive<R>, name: &str) -> Result<Vec<u8>> {
    for entry in archive.entries()? {
        let mut entry = entry?;
        if entry.path()?.to_string_lossy() == name {
            let mut buf = Vec::with_capacity(entry.header().size()? as usize);
            entry.read_to_end(&mut buf)?;
            return Ok(buf);
        }
    }
    Err(anyhow!("entry not found in tar"))
}

// --- 7z ---

fn list_7z(path: &Path) -> Result<Vec<Entry>> {
    let archive = sevenz_rust2::Archive::open(path)?;
    Ok(archive
        .files
        .iter()
        .filter(|f| !f.is_directory())
        .map(|f| Entry {
            name: f.name().to_string(),
            size: f.size(),
        })
        .collect())
}

fn read_7z(path: &Path, name: &str) -> Result<Vec<u8>> {
    let mut reader = sevenz_rust2::ArchiveReader::open(path, sevenz_rust2::Password::empty())?;
    Ok(reader.read_file(name)?)
}

// --- rar ---

#[cfg(feature = "rar")]
fn list_rar(path: &Path) -> Result<Vec<Entry>> {
    let archive = unrar::Archive::new(path)
        .open_for_listing()
        .context("open rar")?;
    let mut out = Vec::new();
    for header in archive {
        let header = header.context("read rar header")?;
        if header.is_file() {
            out.push(Entry {
                name: header.filename.to_string_lossy().into_owned(),
                size: header.unpacked_size,
            });
        }
    }
    Ok(out)
}

#[cfg(feature = "rar")]
fn read_rar(path: &Path, name: &str) -> Result<Vec<u8>> {
    let mut archive = unrar::Archive::new(path)
        .open_for_processing()
        .context("open rar")?;
    while let Some(header) = archive.read_header().context("read rar header")? {
        if header.entry().filename.to_string_lossy() == name {
            let (data, _rest) = header.read().context("read rar entry")?;
            return Ok(data);
        }
        archive = header.skip().context("skip rar entry")?;
    }
    Err(anyhow!("entry not found in rar"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};
    use tempfile::TempDir;

    fn png_bytes() -> Vec<u8> {
        let img = image::RgbaImage::from_pixel(4, 2, image::Rgba([9, 9, 9, 255]));
        let mut out = Cursor::new(Vec::new());
        img.write_to(&mut out, image::ImageFormat::Png).unwrap();
        out.into_inner()
    }

    fn write_zip(dir: &TempDir) -> PathBuf {
        let path = dir.path().join("photos.zip");
        let mut writer = zip::ZipWriter::new(File::create(&path).unwrap());
        let options = zip::write::SimpleFileOptions::default();
        for name in ["b10.png", "b2.png", "notes.txt", "sub/a.png"] {
            writer.start_file(name, options).unwrap();
            if name.ends_with(".png") {
                writer.write_all(&png_bytes()).unwrap();
            } else {
                writer.write_all(b"not an image").unwrap();
            }
        }
        writer.finish().unwrap();
        path
    }

    fn write_tar(dir: &TempDir) -> PathBuf {
        let path = dir.path().join("photos.tar");
        let mut builder = tar::Builder::new(File::create(&path).unwrap());
        let png = png_bytes();
        for name in ["x.png", "skip.txt"] {
            let data: &[u8] = if name.ends_with(".png") {
                &png
            } else {
                b"text"
            };
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append_data(&mut header, name, data).unwrap();
        }
        builder.finish().unwrap();
        path
    }

    #[test]
    fn detects_archive_extensions() {
        assert!(is_archive(Path::new("a.zip")));
        assert!(is_archive(Path::new("comic.CBZ")));
        assert!(is_archive(Path::new("a.tar")));
        assert!(is_archive(Path::new("a.tar.gz")));
        assert!(is_archive(Path::new("a.tgz")));
        assert!(is_archive(Path::new("a.7z")));
        assert!(!is_archive(Path::new("a.png")));
        assert!(!is_archive(Path::new("a.gz"))); // bare .gz isn't browsable
    }

    #[test]
    fn zip_lists_only_images_naturally_sorted() {
        let dir = TempDir::new().unwrap();
        let index = ArchiveIndex::open(&write_zip(&dir)).unwrap();
        let names: Vec<String> = index
            .image_entries()
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["b2.png", "b10.png", "sub/a.png"]);
    }

    #[test]
    fn zip_reads_entry_bytes() {
        let dir = TempDir::new().unwrap();
        let index = ArchiveIndex::open(&write_zip(&dir)).unwrap();
        let bytes = index.read(Path::new("b2.png")).unwrap();
        assert_eq!(bytes, png_bytes());
        assert_eq!(
            index.entry_size(Path::new("b2.png")),
            Some(png_bytes().len() as u64)
        );
    }

    #[test]
    fn zip_missing_entry_errors() {
        let dir = TempDir::new().unwrap();
        let index = ArchiveIndex::open(&write_zip(&dir)).unwrap();
        assert!(index.read(Path::new("nope.png")).is_err());
    }

    #[test]
    fn tar_lists_and_reads() {
        let dir = TempDir::new().unwrap();
        let index = ArchiveIndex::open(&write_tar(&dir)).unwrap();
        let entries = index.image_entries();
        assert_eq!(entries, [PathBuf::from("x.png")]);
        assert_eq!(index.read(Path::new("x.png")).unwrap(), png_bytes());
    }
}
