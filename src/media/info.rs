//! Curated EXIF metadata for the info panel.

use std::io::Cursor;

use exif::{In, Tag};

/// Extract a curated set of human-readable EXIF fields from a file prefix.
/// Returns label/value pairs in display order, empty when there's no EXIF.
pub fn exif_fields(prefix: &[u8]) -> Vec<(String, String)> {
    let Ok(exif) = exif::Reader::new().read_from_container(&mut Cursor::new(prefix)) else {
        return Vec::new();
    };

    // (label, tag) in the order they should appear.
    const FIELDS: &[(&str, Tag)] = &[
        ("Camera make", Tag::Make),
        ("Camera model", Tag::Model),
        ("Lens", Tag::LensModel),
        ("Exposure", Tag::ExposureTime),
        ("Aperture", Tag::FNumber),
        ("ISO", Tag::PhotographicSensitivity),
        ("Focal length", Tag::FocalLength),
        ("Taken", Tag::DateTimeOriginal),
    ];

    let mut out: Vec<(String, String)> = FIELDS
        .iter()
        .filter_map(|(label, tag)| {
            let field = exif.get_field(*tag, In::PRIMARY)?;
            let value = field.display_value().with_unit(&exif).to_string();
            let value = value.trim_matches('"').trim().to_string();
            (!value.is_empty()).then(|| (label.to_string(), value))
        })
        .collect();

    // GPS as plain text, combining coordinate and hemisphere.
    let gps = |coord: Tag, hemisphere: Tag| -> Option<String> {
        let value = exif.get_field(coord, In::PRIMARY)?;
        let reference = exif
            .get_field(hemisphere, In::PRIMARY)
            .map(|f| f.display_value().to_string())
            .unwrap_or_default();
        Some(
            format!("{} {}", value.display_value(), reference.trim_matches('"'))
                .trim()
                .to_string(),
        )
    };
    if let (Some(lat), Some(lon)) = (
        gps(Tag::GPSLatitude, Tag::GPSLatitudeRef),
        gps(Tag::GPSLongitude, Tag::GPSLongitudeRef),
    ) {
        out.push(("Location".to_string(), format!("{lat}, {lon}")));
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_exif_yields_nothing() {
        let img = image::RgbaImage::from_pixel(4, 4, image::Rgba([1, 2, 3, 255]));
        let mut out = Cursor::new(Vec::new());
        img.write_to(&mut out, image::ImageFormat::Png).unwrap();
        assert!(exif_fields(&out.into_inner()).is_empty());
    }

    #[test]
    fn garbage_yields_nothing() {
        assert!(exif_fields(b"not an image").is_empty());
    }
}
