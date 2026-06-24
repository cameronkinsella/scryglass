//! Manual update check: on an explicit button press, ask the GitHub releases
//! API for the latest version and compare it to this build. No background or
//! automatic checks. Networking is rustls-only (a hard project rule).

use semver::Version;

/// Result of the most recent manual check, held in ephemeral app state.
#[derive(Debug, Clone, PartialEq)]
pub enum UpdateStatus {
    Checking,
    UpToDate,
    Available {
        version: String,
        url: String,
    },
    /// Network or parse failure, surfaced calmly.
    Failed,
}

const API_URL: &str = "https://api.github.com/repos/cameronkinsella/scryglass/releases/latest";

/// Run the check off-thread. Any failure resolves to [`UpdateStatus::Failed`].
pub async fn fetch_latest() -> UpdateStatus {
    tokio::task::spawn_blocking(|| match fetch_and_parse() {
        Some((tag, url)) => decide(env!("CARGO_PKG_VERSION"), &tag, &url),
        None => UpdateStatus::Failed,
    })
    .await
    .unwrap_or(UpdateStatus::Failed)
}

/// Blocking GitHub GET, returning the release `(tag_name, html_url)`.
fn fetch_and_parse() -> Option<(String, String)> {
    let body = ureq::get(API_URL)
        // GitHub rejects requests without a User-Agent.
        .header(
            "User-Agent",
            concat!("scryglass/", env!("CARGO_PKG_VERSION")),
        )
        .header("Accept", "application/vnd.github+json")
        .call()
        .ok()?
        .into_body()
        .read_to_string()
        .ok()?;
    parse_release(&body)
}

/// Pull `tag_name` and `html_url` out of a releases/latest response.
fn parse_release(body: &str) -> Option<(String, String)> {
    let json: serde_json::Value = serde_json::from_str(body).ok()?;
    let tag = json.get("tag_name")?.as_str()?.to_string();
    let url = json.get("html_url")?.as_str()?.to_string();
    Some((tag, url))
}

/// Compare the running version to the latest tag (a leading `v` is optional on
/// either). Newer tag wins, equal or older is up to date, unparsable fails.
fn decide(current: &str, tag: &str, url: &str) -> UpdateStatus {
    match (
        Version::parse(strip_v(current)),
        Version::parse(strip_v(tag)),
    ) {
        (Ok(running), Ok(latest)) if latest > running => UpdateStatus::Available {
            version: latest.to_string(),
            url: url.to_string(),
        },
        (Ok(_), Ok(_)) => UpdateStatus::UpToDate,
        _ => UpdateStatus::Failed,
    }
}

fn strip_v(s: &str) -> &str {
    s.strip_prefix('v').unwrap_or(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decide_offers_a_newer_release() {
        assert_eq!(
            decide("0.2.0", "v0.3.0", "https://x"),
            UpdateStatus::Available {
                version: "0.3.0".into(),
                url: "https://x".into(),
            }
        );
    }

    #[test]
    fn decide_is_up_to_date_when_equal_or_older() {
        assert_eq!(decide("0.2.0", "0.2.0", "u"), UpdateStatus::UpToDate);
        assert_eq!(decide("0.2.0", "v0.1.0", "u"), UpdateStatus::UpToDate);
    }

    #[test]
    fn decide_fails_on_unparsable_versions() {
        assert_eq!(decide("0.2.0", "garbage", "u"), UpdateStatus::Failed);
    }

    #[test]
    fn parse_release_pulls_tag_and_url() {
        let body = r#"{"tag_name":"v0.3.0","html_url":"https://example/tag/v0.3.0","x":1}"#;
        assert_eq!(
            parse_release(body),
            Some(("v0.3.0".into(), "https://example/tag/v0.3.0".into()))
        );
    }

    #[test]
    fn parse_release_rejects_malformed_or_incomplete_json() {
        assert_eq!(parse_release("not json"), None);
        assert_eq!(parse_release(r#"{"no_tag":true}"#), None);
    }
}
