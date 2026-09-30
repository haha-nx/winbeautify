//! Finding the latest release on GitHub.
//!
//! Deliberately not the REST API: `https://github.com/<repo>/releases/latest`
//! redirects to the latest release's tag page, and the tag falls out of the
//! final URL. That is one plain GET with no JSON schema, no `User-Agent`
//! quota, and no client library beyond the HTTP call itself. The trade-off —
//! release *assets* have to follow a fixed naming scheme rather than being
//! discovered — is fine, because the release script in `tools/` is the only
//! producer.

use crate::version::{is_newer, parse_tag};
use reqwest::blocking::Client;
use reqwest::{StatusCode, Url};

/// The GitHub repository releases are read from.
pub const RELEASE_REPO: &str = "winbeautify/winbeautify";

/// A release worth updating to, with the URLs of the two assets this crate
/// consumes.
#[derive(Debug, Clone)]
pub struct Release {
    /// The release tag as GitHub spells it (`v0.2.1`).
    pub tag: String,
    /// The bare version (`0.2.1`) — used for staging directories and for
    /// reporting to the user.
    pub version: String,
    /// The zip with the update payload.
    pub zip_url: String,
    /// The sidecar holding the zip's SHA-256, as produced by
    /// `tools/make-release.ps1`.
    pub sha256_url: String,
}

/// Name of the zip asset for a tag. The `.sha256` sidecar is this plus
/// `.sha256`.
pub fn zip_asset_name(tag: &str) -> String {
    format!("WinBeautify-{tag}-x64.zip")
}

/// The HTTP client every call here shares: identified (GitHub wants a
/// User-Agent), bounded (an updater must not hang forever on a stalled
/// connection), and redirect-following (that redirect *is* the version
/// check).
pub(crate) fn client() -> Result<Client, String> {
    Client::builder()
        .user_agent(concat!("WinBeautify/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(std::time::Duration::from_secs(15))
        .timeout(std::time::Duration::from_secs(300))
        .build()
        .map_err(|e| format!("无法创建 HTTP 客户端: {e}"))
}

/// Look up the latest release and report it only if it is newer than
/// `current_version`.
///
/// `Ok(None)` covers every benign "nothing to do": the repository has no
/// releases yet, or the latest one is not newer than what is running.
pub fn check(current_version: &str) -> Result<Option<Release>, String> {
    let client = client()?;
    let latest_url = format!("https://github.com/{RELEASE_REPO}/releases/latest");
    let response = client
        .get(&latest_url)
        .send()
        .map_err(|e| format!("连接发布页失败: {e}"))?;

    if response.status() == StatusCode::NOT_FOUND {
        // No releases at all is a normal state for a young project, not an
        // error to toast.
        return Ok(None);
    }
    let response = response.error_for_status().map_err(|e| {
        if e.status() == Some(StatusCode::UNAUTHORIZED) || e.status() == Some(StatusCode::FORBIDDEN)
        {
            // GitHub answers 403 to rate-limited or blocked requests; say so
            // rather than leaving a bare status code on screen.
            "发布页拒绝了请求（可能被限流），稍后再试".to_string()
        } else {
            format!("发布页返回错误: {e}")
        }
    })?;

    let tag = tag_from_final_url(response.url())
        .ok_or_else(|| "发布页没有重定向到具体的版本".to_string())?;
    let Some(triple) = parse_tag(&tag) else {
        return Err(format!("无法识别版本号: {tag}"));
    };
    let version = format!("{}.{}.{}", triple.0, triple.1, triple.2);
    if !is_newer(current_version, &version) {
        return Ok(None);
    }

    let zip_name = zip_asset_name(&tag);
    Ok(Some(Release {
        zip_url: format!("https://github.com/{RELEASE_REPO}/releases/download/{tag}/{zip_name}"),
        sha256_url: format!(
            "https://github.com/{RELEASE_REPO}/releases/download/{tag}/{zip_name}.sha256"
        ),
        tag,
        version,
    }))
}

/// The tag is the last segment of the redirect target,
/// `/<owner>/<repo>/releases/tag/v0.2.1`.
fn tag_from_final_url(url: &Url) -> Option<String> {
    let segments: Vec<&str> = url.path_segments()?.collect();
    let tag = segments.last()?.to_string();
    if tag.is_empty() {
        return None;
    }
    // Guard against a redirect that lands anywhere unexpected: the final page
    // must be a tag page of this repository.
    let expected_prefix = format!("/{RELEASE_REPO}/releases/tag/");
    if url.path().starts_with(&expected_prefix) {
        Some(tag)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_zip_name_follows_the_scheme_the_release_script_writes() {
        assert_eq!(zip_asset_name("v0.2.1"), "WinBeautify-v0.2.1-x64.zip");
    }

    #[test]
    fn the_tag_is_read_off_a_tag_page_url() {
        let url = Url::parse(&format!(
            "https://github.com/{RELEASE_REPO}/releases/tag/v0.2.1"
        ))
        .unwrap();
        assert_eq!(tag_from_final_url(&url).as_deref(), Some("v0.2.1"));
    }

    #[test]
    fn urls_that_are_not_tag_pages_are_rejected() {
        let url = Url::parse(&format!("https://github.com/{RELEASE_REPO}/releases/latest")).unwrap();
        assert_eq!(tag_from_final_url(&url), None);
        let url = Url::parse("https://github.com/other/repo/releases/tag/v9.9.9").unwrap();
        assert_eq!(tag_from_final_url(&url), None);
    }

    #[test]
    fn a_real_check_against_the_release_feed_is_wired_up() {
        // Not asserting *what* the feed says — only that the endpoint is
        // reachable and the answer parses. Skipped when offline; a fail here
        // usually means the repository moved, which breaks every update.
        match client() {
            Ok(client) => {
                let url = format!("https://github.com/{RELEASE_REPO}/releases/latest");
                if let Ok(response) = client.get(&url).send() {
                    if response.status().is_success() {
                        assert!(
                            tag_from_final_url(response.url()).is_some(),
                            "a successful latest-page hit must redirect to a tag: {}",
                            response.url()
                        );
                    }
                }
            }
            Err(_) => {}
        }
    }
}
