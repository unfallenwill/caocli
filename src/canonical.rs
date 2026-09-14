//! Canonical JSON (RFC 8785) and SHA-256 hashing.
//!
//! The session log records a SHA-256 of every request body so a reader
//! who rebuilds the body from the trace can prove it matches what was
//! sent. The hash has to be over a stable form of the JSON: any reader,
//! any platform, any time, must produce the same bytes for the same
//! request.
//!
//! RFC 8785 is the closest thing to that stable form. It says:
//!
//! - Object keys are sorted in lexicographic order by their UTF-16 code
//!   units. We approximate this by sorting `String`s by their byte order;
//!   ASCII keys are byte-for-byte the same as UTF-16 code units, and the
//!   non-ASCII case is rare enough (every request body in this binary
//!   has ASCII-only keys) that the difference does not matter in
//!   practice.
//! - Numbers are serialised in the JSON-number grammar, with the fewest
//!   digits necessary. `serde_json::Number`'s `Display` already does this.
//! - Strings are escaped with the JSON string grammar.
//! - No insignificant whitespace.
//!
//! SHA-256 is the de-facto content hash on the wire: every cloud
//! service speaks it, every library implements it, and the security
//! tradeoffs are well-understood.
//!
//! These helpers are added in G4 but not called by the rest of the
//! crate until G5 wires `body_sha256` into the request path. The
//! `#[allow(dead_code)]` here buys us a green build with the tests
//! running their own coverage; G5 removes them when the call sites land.
#![allow(dead_code)]

use serde_json::Value;
use sha2::{Digest, Sha256};

/// Serialize `value` to RFC 8785 canonical JSON bytes.
pub fn canonicalize(value: &Value) -> Vec<u8> {
    let mut out = Vec::with_capacity(estimate_size(value));
    write_canonical(&mut out, value);
    out
}

/// Convenience: canonicalize `value` and hash the result. Returns the hex
/// digest a session file stores on a `request` event.
pub fn canonical_sha256_hex(value: &Value) -> String {
    let bytes = canonicalize(value);
    sha256_hex(&bytes)
}

/// Hash `bytes` with SHA-256 and return the digest as a lowercase hex
/// string. The trace records this so a reader who rebuilt the request
/// body byte-for-byte has a single string to compare against.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

fn write_canonical(out: &mut Vec<u8>, value: &Value) {
    match value {
        Value::Null => out.extend_from_slice(b"null"),
        Value::Bool(true) => out.extend_from_slice(b"true"),
        Value::Bool(false) => out.extend_from_slice(b"false"),
        Value::Number(n) => out.extend_from_slice(n.to_string().as_bytes()),
        Value::String(s) => write_string(out, s),
        Value::Array(arr) => {
            out.push(b'[');
            for (i, v) in arr.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_canonical(out, v);
            }
            out.push(b']');
        }
        Value::Object(obj) => {
            out.push(b'{');
            // Sort keys by their UTF-8 byte order, which for ASCII
            // matches RFC 8785's UTF-16 code-unit order. `serde_json`
            // already keeps keys in a `Map<String, Value>`; collecting
            // and sorting is O(k log k) on the key count, and a
            // request body has dozens of keys at most.
            let mut keys: Vec<&String> = obj.keys().collect();
            keys.sort();
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_string(out, k);
                out.push(b':');
                write_canonical(out, &obj[*k]);
            }
            out.push(b'}');
        }
    }
}

fn write_string(out: &mut Vec<u8>, s: &str) {
    // Borrow serde_json's string-escape implementation so a reader can
    // hand back exactly the bytes the writer produced, with no
    // surprise divergence in escaping rules. `serde_json::to_string`
    // already wraps the result in quotes; do not add another pair.
    let escaped = serde_json::to_string(s).expect("serialising a string never fails");
    out.extend_from_slice(escaped.as_bytes());
}

fn estimate_size(value: &Value) -> usize {
    match value {
        Value::Null | Value::Bool(_) => 4,
        Value::Number(_) => 32,
        Value::String(s) => s.len() * 2 + 2,
        Value::Array(arr) => arr.iter().map(estimate_size).sum::<usize>() + 2,
        Value::Object(obj) => {
            obj.iter()
                .map(|(k, v)| k.len() * 2 + 2 + estimate_size(v) + 1)
                .sum::<usize>()
                + 2
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn empty_object() {
        assert_eq!(canonicalize(&json!({})), b"{}");
    }

    #[test]
    fn empty_array() {
        assert_eq!(canonicalize(&json!([])), b"[]");
    }

    #[test]
    fn scalars() {
        assert_eq!(canonicalize(&json!(null)), b"null");
        assert_eq!(canonicalize(&json!(true)), b"true");
        assert_eq!(canonicalize(&json!(false)), b"false");
        assert_eq!(canonicalize(&json!(42)), b"42");
        assert_eq!(canonicalize(&json!(-3.5)), b"-3.5");
    }

    #[test]
    fn string_is_quoted() {
        assert_eq!(canonicalize(&json!("hi")), br#""hi""#);
    }

    #[test]
    fn string_escapes_match_serde_json() {
        let s = "a\n\t\"\\b\x01c";
        let value = json!(s);
        let expected = serde_json::to_string(&value).unwrap();
        let bytes = canonicalize(&value);
        let as_str = std::str::from_utf8(&bytes).unwrap();
        assert!(as_str.starts_with('"') && as_str.ends_with('"'));
        assert!(as_str.contains("\\n"));
        assert!(as_str.contains("\\t"));
        assert!(as_str.contains("\\\""));
        assert!(as_str.contains("\\\\"));
        assert_eq!(bytes, expected.as_bytes());
    }

    #[test]
    fn object_keys_are_sorted() {
        let value = json!({"b": 2, "a": 1, "c": 3});
        assert_eq!(canonicalize(&value), br#"{"a":1,"b":2,"c":3}"#);
    }

    #[test]
    fn nested_objects_recursively_sort() {
        let value = json!({"z": {"y": 1, "x": 2}, "a": [{"d": 1, "c": 2}, {"b": 3}]});
        assert_eq!(
            canonicalize(&value),
            br#"{"a":[{"c":2,"d":1},{"b":3}],"z":{"x":2,"y":1}}"#
        );
    }

    #[test]
    fn array_order_is_preserved() {
        let value = json!({"tools": [{"name": "c"}, {"name": "a"}, {"name": "b"}]});
        assert_eq!(
            canonicalize(&value),
            br#"{"tools":[{"name":"c"},{"name":"a"},{"name":"b"}]}"#
        );
    }

    #[test]
    fn utf8_keys_are_sorted_by_byte_order() {
        let value = json!({"β": 1, "a": 2, "α": 3});
        let bytes = canonicalize(&value);
        let s = std::str::from_utf8(&bytes).unwrap();
        let a_pos = s.find("\"a\"").unwrap();
        let alpha_pos = s.find("α").unwrap();
        let beta_pos = s.find("β").unwrap();
        assert!(a_pos < alpha_pos);
        assert!(alpha_pos < beta_pos, "got: {s}");
    }

    #[test]
    fn sha256_of_canonical_is_stable() {
        let a = json!({"model": "x", "messages": [], "stream": true});
        let b = json!({"stream": true, "messages": [], "model": "x"});
        assert_eq!(canonical_sha256_hex(&a), canonical_sha256_hex(&b));
    }

    #[test]
    fn sha256_known_vector_empty_string() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn sha256_known_vector_hello_world() {
        assert_eq!(
            sha256_hex(b"hello world"),
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
    }

    #[test]
    fn canonical_request_body_hash_matches_a_real_openai_shape() {
        let body = json!({
            "model": "deepseek-v4-flash",
            "messages": [
                {"role": "system", "content": "SYSTEM"},
                {"role": "user", "content": "hi"}
            ],
            "max_tokens": 384000,
            "stream": true,
            "stream_options": {"include_usage": true},
            "tool_choice": "auto",
            "tools": [
                {"type": "function", "function": {"name": "Bash", "description": "Run a shell command", "parameters": {"type": "object"}}}
            ],
            "thinking": {"type": "enabled"},
            "reasoning_effort": "max"
        });
        let hash = canonical_sha256_hex(&body);
        let expected = sha256_hex(&canonicalize(&body));
        assert_eq!(hash, expected);
    }

    #[test]
    fn round_trip_through_serde_value_is_stable() {
        let raw = r#"{"b":2,"a":1,"nested":{"y":2,"x":1}}"#;
        let parsed: Value = serde_json::from_str(raw).unwrap();
        let literal = json!({"a": 1, "b": 2, "nested": {"x": 1, "y": 2}});
        assert_eq!(canonicalize(&parsed), canonicalize(&literal));
        assert_eq!(
            canonical_sha256_hex(&parsed),
            canonical_sha256_hex(&literal)
        );
    }
}
