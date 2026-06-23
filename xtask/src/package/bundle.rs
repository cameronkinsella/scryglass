//! The macOS `Info.plist`, built as a typed dictionary so the field set is
//! unit-tested rather than written as a heredoc in CI.

use plist::{Dictionary, Value};

/// Build the `Info.plist` for `version`. Document types stay on the broad
/// `public.image`/`public.movie` UTIs (no custom UTI declarations).
#[allow(dead_code)] // used only by the macOS builder, but tested everywhere
pub fn info_plist(version: &str) -> Value {
    let mut doc_type = Dictionary::new();
    doc_type.insert("CFBundleTypeRole".into(), "Viewer".into());
    doc_type.insert(
        "LSItemContentTypes".into(),
        Value::Array(vec!["public.image".into(), "public.movie".into()]),
    );

    let mut dict = Dictionary::new();
    dict.insert("CFBundleExecutable".into(), "scryglass".into());
    dict.insert(
        "CFBundleIdentifier".into(),
        "com.cameronkinsella.scryglass".into(),
    );
    dict.insert("CFBundleName".into(), "scryglass".into());
    dict.insert("CFBundleDisplayName".into(), "scryglass".into());
    dict.insert("CFBundleIconFile".into(), "scryglass".into());
    dict.insert("CFBundlePackageType".into(), "APPL".into());
    dict.insert("CFBundleShortVersionString".into(), version.into());
    dict.insert("CFBundleVersion".into(), version.into());
    dict.insert("LSMinimumSystemVersion".into(), "11.0".into());
    dict.insert("NSHighResolutionCapable".into(), true.into());
    dict.insert(
        "CFBundleDocumentTypes".into(),
        Value::Array(vec![Value::Dictionary(doc_type)]),
    );
    Value::Dictionary(dict)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn carries_identity_and_version() {
        let value = info_plist("0.2.0");
        let dict = value.as_dictionary().unwrap();
        assert_eq!(
            dict["CFBundleIdentifier"].as_string(),
            Some("com.cameronkinsella.scryglass")
        );
        assert_eq!(
            dict["CFBundleShortVersionString"].as_string(),
            Some("0.2.0")
        );
        assert_eq!(dict["CFBundleVersion"].as_string(), Some("0.2.0"));
        assert_eq!(dict["NSHighResolutionCapable"].as_boolean(), Some(true));
    }

    #[test]
    fn declares_image_and_movie_document_types() {
        let value = info_plist("0.2.0");
        let types = value.as_dictionary().unwrap()["CFBundleDocumentTypes"]
            .as_array()
            .unwrap();
        let first = types[0].as_dictionary().unwrap();
        let content = first["LSItemContentTypes"].as_array().unwrap();
        let names: Vec<_> = content.iter().filter_map(|v| v.as_string()).collect();
        assert!(names.contains(&"public.image"));
        assert!(names.contains(&"public.movie"));
    }
}
