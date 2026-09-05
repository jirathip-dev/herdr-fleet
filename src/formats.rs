//! String-format validators for the #3 identity/scalar rules.
//!
//! The rules mirror the fixture probe (`scripts/check-contract-fixtures.py`)
//! exactly: repository identity `owner/name`, slugs, hex digests, RFC3339
//! seconds-`Z` shape, actor ids, error codes, and semantic versions.

/// `owner/name` repository identity, no protocol prefix.
pub fn is_repository_identity(text: &str) -> bool {
    let Some((owner, name)) = text.split_once('/') else {
        return false;
    };
    !owner.is_empty()
        && !name.is_empty()
        && !name.contains('/')
        && path_component_chars(owner)
        && path_component_chars(name)
}

/// One path component of an `owner/name` identity: `[A-Za-z0-9_.-]+`.
fn path_component_chars(text: &str) -> bool {
    !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Slug: `^[a-z0-9][a-z0-9-]{0,63}$` (repository keys, workflow ids, step ids).
pub fn is_slug(text: &str) -> bool {
    let mut chars = text.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return false;
    }
    let mut len = 1;
    for c in chars {
        len += 1;
        if !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-') {
            return false;
        }
    }
    len <= 64
}

/// Lowercase hex of exactly `digits` characters.
pub fn is_lower_hex(text: &str, digits: usize) -> bool {
    text.len() == digits
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// 40-hex (SHA-1 sized) lowercase digest/commit.
pub fn is_hex40(text: &str) -> bool {
    is_lower_hex(text, 40)
}

/// 64-hex (SHA-256 sized) lowercase digest.
pub fn is_hex64(text: &str) -> bool {
    is_lower_hex(text, 64)
}

/// RFC3339 UTC seconds precision with `Z`: `^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z$`
/// (shape rule only, mirroring the fixture probe).
pub fn is_rfc3339_seconds_z(text: &str) -> bool {
    let bytes = text.as_bytes();
    if bytes.len() != 20
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
        || bytes[19] != b'Z'
    {
        return false;
    }
    text.bytes()
        .enumerate()
        .all(|(i, b)| matches!(i, 4 | 7 | 10 | 13 | 16 | 19) || b.is_ascii_digit())
}

/// Actor id: `^[A-Za-z0-9_.-]{1,64}$`.
pub fn is_actor(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 64
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Error code: `^[a-z][a-z0-9_.-]*$`.
pub fn is_error_code(text: &str) -> bool {
    let mut chars = text.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_lowercase() {
        return false;
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
}

/// Plan id: `hf_plan_` + 16 lowercase hex.
pub fn is_plan_id(text: &str) -> bool {
    const PREFIX: &str = "hf_plan_";
    text.len() == PREFIX.len() + 16
        && text.starts_with(PREFIX)
        && is_lower_hex(&text[PREFIX.len()..], 16)
}

/// Parse a semantic version `major.minor.patch` (numeric fields only).
pub fn parse_semver(text: &str) -> Option<(u64, u64, u64)> {
    let mut parts = text.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repository_identity_rules() {
        assert!(is_repository_identity("example-org/widgets"));
        assert!(is_repository_identity("a/b"));
        assert!(is_repository_identity("example_org/widgets.v2"));
        assert!(!is_repository_identity(
            "https://github.com/example-org/widgets"
        ));
        assert!(!is_repository_identity("example-org/"));
        assert!(!is_repository_identity("widgets"));
        assert!(!is_repository_identity(""));
    }

    #[test]
    fn slug_rules() {
        assert!(is_slug("widgets"));
        assert!(is_slug("fleet-doctrine-1"));
        assert!(is_slug("p1"));
        assert!(!is_slug("Widgets"));
        assert!(!is_slug("-widgets"));
        assert!(!is_slug("widgets/other"));
        assert!(!is_slug(""));
    }

    #[test]
    fn hex_rules() {
        assert!(is_hex40(&"0".repeat(40)));
        assert!(!is_hex40(&"0".repeat(39)));
        assert!(is_hex64(&"ab".repeat(32)));
        assert!(!is_hex64(&"AB".repeat(32)), "lowercase only");
    }

    #[test]
    fn rfc3339_shape() {
        assert!(is_rfc3339_seconds_z("2026-09-06T00:00:00Z"));
        assert!(!is_rfc3339_seconds_z("2026-09-06T00:00:00+00:00"));
        // Shape rule parity with the fixture probe: the regex validates
        // positions only; calendar semantics are the emitter's job.
        assert!(is_rfc3339_seconds_z("2026-13-45T99:00:00Z"));
        assert!(!is_rfc3339_seconds_z("2026-9-06T00:00:00Z"));
    }

    #[test]
    fn actor_and_error_code_rules() {
        assert!(is_actor("herdr-fleet"));
        assert!(is_actor("gh"));
        assert!(!is_actor(""));
        assert!(is_error_code("config.invalid"));
        assert!(is_error_code("refusal.stale_state"));
        assert!(!is_error_code("Config.invalid"));
    }

    #[test]
    fn semver_parse_and_order() {
        assert_eq!(parse_semver("0.8.2"), Some((0, 8, 2)));
        assert_eq!(parse_semver("2.100.0"), Some((2, 100, 0)));
        assert_eq!(parse_semver("v0.8.2"), None);
        assert_eq!(parse_semver("0.8"), None);
        assert_eq!(parse_semver("0.8.2.1"), None);
    }
}
