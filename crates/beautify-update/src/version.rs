//! Release version arithmetic: parse the strings GitHub hands us and decide
//! whether one is newer than the version now running.
//!
//! Versions are the workspace's `major.minor.patch` triples. That is the
//! whole grammar — no build metadata, no pre-release ordering. A tag that
//! does not parse as a bare triple (say `v0.3.0-beta.1`) is *ignored* rather
//! than errored: an updater that cannot understand a release must not offer
//! it, because it also cannot order it.

/// Parse `0.2.1` into a comparable triple. Exactly three all-numeric
/// components; anything else is `None`.
pub fn parse_version(s: &str) -> Option<(u64, u64, u64)> {
    let mut parts = s.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    // A fourth component (`1.2.3.4`) or trailing junk after the patch number
    // (`1.2.3-beta`) makes this a release the ordering here cannot place.
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

/// Parse a release tag. The project's tags are the version with a `v` prefix,
/// but a bare version is accepted too — the tag is read from a redirect URL,
/// not from a schema we control.
pub fn parse_tag(tag: &str) -> Option<(u64, u64, u64)> {
    parse_version(tag.strip_prefix('v').unwrap_or(tag))
}

/// Is `candidate` strictly newer than `current`? Unparseable on either side
/// means "no": never offer what cannot be ordered, and never report a parse
/// hiccup in the current version as an available update.
pub fn is_newer(current: &str, candidate: &str) -> bool {
    match (parse_version(current), parse_version(candidate)) {
        (Some(a), Some(b)) => b > a,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_triples_parse() {
        assert_eq!(parse_version("0.2.0"), Some((0, 2, 0)));
        assert_eq!(parse_version("1.10.3"), Some((1, 10, 3)));
    }

    #[test]
    fn junk_is_rejected() {
        assert_eq!(parse_version(""), None);
        assert_eq!(parse_version("0.2"), None);
        assert_eq!(parse_version("0.2.0.1"), None);
        assert_eq!(parse_version("0.2.0-beta"), None);
        assert_eq!(parse_version("x.y.z"), None);
    }

    #[test]
    fn the_v_prefix_is_optional() {
        assert_eq!(parse_tag("v0.2.1"), Some((0, 2, 1)));
        assert_eq!(parse_tag("0.2.1"), Some((0, 2, 1)));
        assert_eq!(parse_tag("release-0.2.1"), None);
    }

    #[test]
    fn newer_means_strictly_greater() {
        assert!(is_newer("0.2.0", "0.2.1"));
        assert!(is_newer("0.2.0", "0.3.0"));
        assert!(is_newer("0.2.0", "1.0.0"));
        assert!(!is_newer("0.2.1", "0.2.1"), "same version is not an update");
        assert!(!is_newer("0.2.1", "0.2.0"), "downgrades are not offered");
        // Lexicographic order would call "0.10.0" older than "0.2.0"; the
        // triple comparison must not.
        assert!(is_newer("0.2.0", "0.10.0"));
    }

    #[test]
    fn unparseable_candidates_are_never_updates() {
        assert!(!is_newer("0.2.0", "v2-beta"));
        assert!(!is_newer("not-a-version", "0.3.0"));
    }
}
