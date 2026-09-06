//! Conservative secret-shaped text redaction.
//!
//! The #3 CLI spec (spec-cli.md §6) requires one shared redaction pass at
//! the adapter boundary: anything captured from adapters/remotes that becomes
//! a canonical record is redacted first. Redaction is conservative —
//! false positives cost nothing, false negatives leak — and deterministic
//! (the same input always produces the same output, so acceptance-revision
//! digests over redacted text stay stable).

const REPLACEMENT: &str = "[REDACTED]";

/// Token characters that may appear inside a secret-shaped run.
fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | '=' | '+' | '$')
}

/// A match is `(start, end)` char indices into the input.
type Match = (usize, usize);

/// Redact secret-shaped text, returning an owned copy with each match
/// replaced by `[REDACTED]`.
pub fn redact(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < chars.len() {
        match find_match(&chars, i) {
            Some((start, end)) => {
                out.extend(chars[i..start].iter());
                out.push_str(REPLACEMENT);
                i = end;
            }
            None => {
                out.push(chars[i]);
                i += 1;
            }
        }
    }
    out
}

/// Find the first secret match at or after `from`. Returns absolute char
/// indices (start, end) of the full redacted span.
fn find_match(chars: &[char], from: usize) -> Option<Match> {
    let mut start = from;
    while start < chars.len() {
        if let Some(matched) = anchored_match(chars, start) {
            return Some(matched);
        }
        start += 1;
    }
    None
}

/// Detect a secret anchored exactly at `start`; returns the absolute span to
/// redact (rules below).
fn anchored_match(chars: &[char], start: usize) -> Option<Match> {
    let rest: String = chars[start..].iter().collect();

    // Token-prefixed secrets (ghp_, github_pat_, glpat-, xox*, sk-, AKIA)
    // must begin at a token boundary so prose words never trigger.
    let at_boundary = start == 0 || !is_token_char(chars[start - 1]);
    if at_boundary {
        const TOKEN_PREFIXES: [(&str, usize); 10] = [
            ("github_pat_", 20),
            ("ghp_", 8),
            ("gho_", 8),
            ("ghu_", 8),
            ("ghs_", 8),
            ("ghr_", 8),
            ("glpat-", 8),
            ("xoxb-", 8),
            ("xoxp-", 8),
            ("sk-", 20),
        ];
        for (prefix, floor) in TOKEN_PREFIXES {
            if rest.starts_with(prefix) {
                let run = token_run_len(&chars[start + prefix.len()..]);
                if run >= floor {
                    return Some((start, start + prefix.len() + run));
                }
            }
        }
        for (prefix, floor) in [("xoxa-", 8), ("xoxr-", 8), ("AKIA", 16)] {
            if rest.starts_with(prefix) {
                let run = token_run_len(&chars[start + prefix.len()..]);
                if run >= floor {
                    return Some((start, start + prefix.len() + run));
                }
            }
        }
        // PEM private-key blocks: redact through the END marker line. The
        // line's trailing newline (when present) is left in place so line
        // structure survives.
        if rest.starts_with("-----BEGIN") {
            let full: String = chars[start..].iter().collect();
            return match full.find("-----END") {
                Some(end_marker) => {
                    let line_end = full[end_marker..]
                        .find('\n')
                        .map(|offset| end_marker + offset)
                        .unwrap_or(full.len());
                    Some((start, start + line_end))
                }
                None => Some((start, chars.len())), // unterminated: redact to end
            };
        }
    }

    // URL userinfo: scheme://user[:pass]@host — redact the part between the
    // "://" and the "@", keeping the scheme/host text outside the span. This
    // shape is recognized regardless of the preceding character.
    if let Some(after) = rest.strip_prefix("://")
        && let Some(at) = after.find('@')
    {
        let before_at = &after[..at];
        let has_slash_before = before_at.contains('/');
        if !has_slash_before && before_at.len() >= 3 {
            let segment_start = start + 3;
            return Some((segment_start, segment_start + at + 1));
        }
    }
    None
}

/// Length of the maximal token-character run.
fn token_run_len(chars: &[char]) -> usize {
    chars.iter().take_while(|c| is_token_char(**c)).count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_common_token_shapes() {
        let token = format!("ghp_{}", "0123456789abcdef0123456789abcdef012345");
        assert_eq!(
            redact(&format!("token {token} here")),
            "token [REDACTED] here"
        );
        assert_eq!(
            redact("github_pat_0123456789abcdef0123456789abcdef"),
            "[REDACTED]"
        );
        assert_eq!(
            redact("sk-ant-0123456789abcdef0123456789abcdef0123456789abcdef"),
            "[REDACTED]"
        );
        assert_eq!(redact(concat!("AKIA", "0123456789ABCDEF")), "[REDACTED]");
    }

    #[test]
    fn redacts_pem_blocks_including_end_line() {
        let pem = format!(
            "-----BEGIN {} KEY-----\nMIIB\n-----END {} KEY-----\n",
            "PRIVATE", "PRIVATE"
        );
        assert_eq!(redact(&pem), "[REDACTED]\n");
        let no_trailing_newline = format!(
            "-----BEGIN {} KEY-----\nMIIB\n-----END {} KEY-----",
            "PRIVATE", "PRIVATE"
        );
        assert_eq!(redact(&no_trailing_newline), "[REDACTED]");
    }

    #[test]
    fn redacts_url_userinfo() {
        assert_eq!(
            redact("https://user:supersecret@example.com/path"),
            "https://[REDACTED]example.com/path"
        );
        assert_eq!(
            redact("clone from https://user@example.com/repo now"),
            "clone from https://[REDACTED]example.com/repo now"
        );
    }

    #[test]
    fn idempotent_and_conservative() {
        let text = "plain prose with ghp_short token (too short) and normal words";
        let once = redact(text);
        assert_eq!(redact(&once), once, "redaction is idempotent");
        assert!(once.contains("plain prose"));
        assert!(once.contains("ghp_short"), "short runs stay prose");
    }

    #[test]
    fn short_prefixes_stay_untouched_in_words() {
        // A ghp_-like prefix inside a longer word must not trigger.
        assert_eq!(redact("myghp_thing"), "myghp_thing");
        assert_eq!(redact("prefix-ghp_short-suffix"), "prefix-ghp_short-suffix");
    }

    #[test]
    fn adjacent_text_survives() {
        assert_eq!(
            redact(&format!(
                "before ghp_{} after",
                "0123456789abcdef0123456789abcdef012345"
            )),
            "before [REDACTED] after"
        );
    }
}
