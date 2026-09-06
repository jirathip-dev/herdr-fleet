//! Canonical JSON serialization and SHA-256 digests.
//!
//! The #3 schema-registry canonical rule (see `docs/contracts/schema-registry.md`)
//! is: UTF-8 of the JSON value with keys sorted lexicographically, compact
//! separators (`,` and `:`), ASCII escaping (`ensure_ascii` semantics), and a
//! single trailing LF. [`canonical_bytes`] implements exactly that rule over
//! [`crate::value::Val`], and [`sha256_hex`] computes the lowercase-hex digest
//! used by plan documents.

use sha2::{Digest, Sha256};

use crate::value::Val;

/// Serialize a document value to canonical JSON bytes (sorted keys, compact
/// separators, ASCII-only escapes, single trailing LF).
pub fn canonical_bytes(value: &Val) -> Vec<u8> {
    let mut out = Vec::new();
    write_value(value, &mut out);
    out.push(b'\n');
    out
}

/// Serialize a document value to canonical JSON text (same rule, no LF).
pub fn canonical_text(value: &Val) -> String {
    let mut out = String::new();
    write_value_to_string(value, &mut out);
    out
}

fn write_value(value: &Val, out: &mut Vec<u8>) {
    match value {
        Val::Null => out.extend_from_slice(b"null"),
        Val::Bool(true) => out.extend_from_slice(b"true"),
        Val::Bool(false) => out.extend_from_slice(b"false"),
        Val::Int(int) => out.extend_from_slice(int.to_string().as_bytes()),
        Val::Float(float) => out.extend_from_slice(float_text(*float).as_bytes()),
        Val::Str(text) => write_string(text, out),
        Val::Arr(items) => {
            out.push(b'[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_value(item, out);
            }
            out.push(b']');
        }
        Val::Obj(map) => {
            out.push(b'{');
            for (i, (key, item)) in map.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_string(key, out);
                out.push(b':');
                write_value(item, out);
            }
            out.push(b'}');
        }
    }
}

fn write_value_to_string(value: &Val, out: &mut String) {
    match value {
        Val::Null => out.push_str("null"),
        Val::Bool(true) => out.push_str("true"),
        Val::Bool(false) => out.push_str("false"),
        Val::Int(int) => out.push_str(&int.to_string()),
        Val::Float(float) => out.push_str(&float_text(*float)),
        Val::Str(text) => write_string_to_string(text, out),
        Val::Arr(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_value_to_string(item, out);
            }
            out.push(']');
        }
        Val::Obj(map) => {
            out.push('{');
            for (i, (key, item)) in map.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_string_to_string(key, out);
                out.push(':');
                write_value_to_string(item, out);
            }
            out.push('}');
        }
    }
}

/// Float text compatible with the Python fixture probe's `json.dumps`
/// (`1.0` keeps a fractional part, exponents keep no `+` unless parsed so).
/// The contract documents carry no floats; this only keeps parity on the
/// canonical families' byte rule for any float that appears in tests.
fn float_text(float: f64) -> String {
    let mut text = float.to_string();
    if !text.contains('.') && !text.contains('e') && !text.contains('E') {
        text.push_str(".0");
    }
    text
}

/// Write one JSON string with `ensure_ascii` escaping: control characters use
/// the short escapes or `\u00XX`, every non-ASCII character uses `\uXXXX`
/// (surrogate pairs above U+FFFF), and `"`/`\` are escaped.
fn write_string(text: &str, out: &mut Vec<u8>) {
    out.push(b'"');
    for c in text.chars() {
        match c {
            '"' => out.extend_from_slice(b"\\\""),
            '\\' => out.extend_from_slice(b"\\\\"),
            '\u{08}' => out.extend_from_slice(b"\\b"),
            '\u{0c}' => out.extend_from_slice(b"\\f"),
            '\n' => out.extend_from_slice(b"\\n"),
            '\r' => out.extend_from_slice(b"\\r"),
            '\t' => out.extend_from_slice(b"\\t"),
            c if (c as u32) < 0x20 => {
                out.extend_from_slice(format!("\\u{:04x}", c as u32).as_bytes());
            }
            c if (c as u32) < 0x80 => {
                out.push(c as u8);
            }
            c if (c as u32) <= 0xffff => {
                out.extend_from_slice(format!("\\u{:04x}", c as u32).as_bytes());
            }
            c => {
                let code = c as u32;
                let high = 0xd800 + ((code - 0x10000) >> 10);
                let low = 0xdc00 + ((code - 0x10000) & 0x3ff);
                out.extend_from_slice(format!("\\u{high:04x}\\u{low:04x}").as_bytes());
            }
        }
    }
    out.push(b'"');
}

fn write_string_to_string(text: &str, out: &mut String) {
    let mut bytes = Vec::new();
    write_string(text, &mut bytes);
    out.push_str(std::str::from_utf8(&bytes).expect("escape output is ASCII"));
}

/// Lowercase-hex SHA-256 digest over arbitrary bytes.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut text = String::with_capacity(64);
    for byte in digest {
        text.push_str(&format!("{byte:02x}"));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::Val;

    fn obj(pairs: &[(&str, Val)]) -> Val {
        Val::Obj(
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect(),
        )
    }

    #[test]
    fn sorts_keys_and_compacts() {
        let doc = obj(&[
            ("zeta", Val::Int(1)),
            ("alpha", Val::Arr(vec![Val::Int(1), Val::Null])),
        ]);
        assert_eq!(canonical_text(&doc), r#"{"alpha":[1,null],"zeta":1}"#);
    }

    #[test]
    fn ascii_escapes_non_ascii_and_controls() {
        let doc = Val::Str("héllo\n😀".to_string());
        assert_eq!(canonical_text(&doc), r#""h\u00e9llo\n\ud83d\ude00""#);
    }

    #[test]
    fn trailing_lf_is_part_of_canonical_bytes() {
        let doc = obj(&[("a", Val::Int(1))]);
        let mut expected = br#"{"a":1}"#.to_vec();
        expected.push(b'\n');
        assert_eq!(canonical_bytes(&doc), expected);
    }

    #[test]
    fn canonical_form_is_idempotent() {
        let doc = obj(&[
            ("b", Val::Str("x".to_string())),
            ("a", Val::Arr(vec![Val::Bool(true), Val::Null])),
        ]);
        let once = canonical_bytes(&doc);
        let parsed = Val::parse_json(std::str::from_utf8(&once).expect("utf8")).expect("parse");
        assert_eq!(canonical_bytes(&parsed), once);
    }

    #[test]
    fn sha256_known_answer() {
        // sha256("") and sha256("abc") known answers.
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn unicode_digest_is_stable() {
        let doc = Val::Str("héllo 😀".to_string());
        let first = canonical_bytes(&doc);
        let second = canonical_bytes(
            &Val::parse_json(std::str::from_utf8(&first).expect("utf8")).expect("parse"),
        );
        assert_eq!(first, second);
    }
}
