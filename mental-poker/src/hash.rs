//! Domain-separated hashing and canonical JSON serialization.
//!
//! Every hash in the Mental Poker protocol is **domain-separated**: the first
//! input is a fixed domain string, and every input is length-prefixed with a
//! `u64` little-endian length. This makes it impossible to craft two distinct
//! input tuples that hash to the same value by shifting bytes across argument
//! boundaries.

use serde_json::Value;
use sha2::{Digest, Sha256};

/// A 32-byte SHA-256 digest.
pub type Hash = [u8; 32];

/// The all-zero hash — used as the `previous_event_hash` of the first event.
pub const ZERO_HASH: Hash = [0u8; 32];

/// Domain-separated SHA-256 over a list of byte slices.
///
/// Layout: `len(domain) ‖ domain ‖ count ‖ (len(part) ‖ part)*`.
pub fn ds_hash(domain: &str, parts: &[&[u8]]) -> Hash {
    let mut h = Sha256::new();
    h.update((domain.len() as u64).to_le_bytes());
    h.update(domain.as_bytes());
    h.update((parts.len() as u64).to_le_bytes());
    for part in parts {
        h.update((part.len() as u64).to_le_bytes());
        h.update(part);
    }
    h.finalize().into()
}

/// Hex-encode a hash for transcript fields.
pub fn hex_hash(h: &Hash) -> String {
    hex::encode(h)
}

/// Decode a hex string back into a [`Hash`]. Returns `None` on malformed input.
pub fn parse_hash(s: &str) -> Option<Hash> {
    let bytes = hex::decode(s).ok()?;
    if bytes.len() != 32 {
        return None;
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes);
    Some(out)
}

/// Canonical JSON serialization: object keys sorted recursively, no insignificant
/// whitespace. Two semantically equal JSON values always produce identical bytes,
/// which is required for stable hashing and signing.
pub fn canonical_json(value: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    write_canonical(value, &mut out);
    out
}

fn write_canonical(value: &Value, out: &mut Vec<u8>) {
    match value {
        Value::Null => out.extend_from_slice(b"null"),
        Value::Bool(true) => out.extend_from_slice(b"true"),
        Value::Bool(false) => out.extend_from_slice(b"false"),
        Value::Number(n) => out.extend_from_slice(n.to_string().as_bytes()),
        Value::String(s) => write_json_string(s, out),
        Value::Array(items) => {
            out.push(b'[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_canonical(item, out);
            }
            out.push(b']');
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push(b'{');
            for (i, key) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_json_string(key, out);
                out.push(b':');
                write_canonical(&map[*key], out);
            }
            out.push(b'}');
        }
    }
}

fn write_json_string(s: &str, out: &mut Vec<u8>) {
    // serde_json produces a correctly escaped JSON string literal.
    let encoded = serde_json::to_string(s).expect("string always serializes");
    out.extend_from_slice(encoded.as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ds_hash_is_domain_separated() {
        // Same concatenated bytes, different argument boundaries → different hash.
        let a = ds_hash("d", &[b"abc", b"def"]);
        let b = ds_hash("d", &[b"ab", b"cdef"]);
        assert_ne!(a, b, "length-prefixing must prevent boundary collisions");
    }

    #[test]
    fn ds_hash_domain_matters() {
        assert_ne!(ds_hash("d1", &[b"x"]), ds_hash("d2", &[b"x"]));
    }

    #[test]
    fn canonical_json_sorts_keys() {
        let a = json!({ "b": 1, "a": 2, "c": 3 });
        let b = json!({ "c": 3, "a": 2, "b": 1 });
        assert_eq!(canonical_json(&a), canonical_json(&b));
        assert_eq!(canonical_json(&a), br#"{"a":2,"b":1,"c":3}"#);
    }

    #[test]
    fn canonical_json_is_recursive() {
        let a = json!({ "outer": { "y": 1, "x": 2 } });
        let b = json!({ "outer": { "x": 2, "y": 1 } });
        assert_eq!(canonical_json(&a), canonical_json(&b));
    }

    #[test]
    fn hash_hex_round_trip() {
        let h = ds_hash("d", &[b"payload"]);
        assert_eq!(parse_hash(&hex_hash(&h)), Some(h));
        assert_eq!(parse_hash("not-hex"), None);
        assert_eq!(parse_hash("00"), None);
    }
}
