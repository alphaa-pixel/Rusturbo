//! Deterministic content hashing used for cache keys.
//!
//! Every cache decision in RustTurbo is derived from an explicit, ordered list of
//! *inputs* (see [`crate::cache::CacheKey::inputs`]). Nothing is keyed on wall-clock
//! time, absolute project paths or environment variables unless we recorded them,
//! which is what makes a key shareable across machines and projects.

use sha2::{Digest, Sha256};

/// A stable, ordered accumulator of fingerprint inputs.
#[derive(Debug, Default, Clone)]
pub struct Fingerprint {
    hasher: Sha256,
    /// Human-readable list of what went into the hash, for `--explain` output.
    inputs: Vec<(String, String)>,
}

impl Fingerprint {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a labelled input. Field names are hashed too so that two different
    /// fields with equal values never collide.
    pub fn push(&mut self, label: &str, value: &str) {
        self.hasher.update(label.as_bytes());
        self.hasher.update(b"\x1f");
        self.hasher.update(value.as_bytes());
        self.hasher.update(b"\x1e");
        self.inputs.push((label.to_string(), value.to_string()));
    }

    pub fn push_lines(&mut self, label: &str, values: &[String]) {
        let mut joined = String::new();
        for v in values {
            joined.push_str(v);
            joined.push('\n');
        }
        self.push(label, joined.trim_end_matches('\n'));
    }

    pub fn inputs(&self) -> &[(String, String)] {
        &self.inputs
    }

    /// Finalize into a hex digest. Short keys (first 16 hex chars = 64 bits) are used
    /// for directory names; the full digest stays available for verification.
    pub fn finish(&self) -> String {
        let h = self.hasher.clone();
        hex(&h.finalize())
    }

    /// A short, filesystem-safe cache key derived from the full digest.
    pub fn short(&self) -> String {
        self.finish().chars().take(16).collect()
    }
}

pub fn sha256_hex(data: impl AsRef<[u8]>) -> String {
    let mut h = Sha256::new();
    h.update(data.as_ref());
    hex(&h.finalize())
}

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_encoding_is_lowercase_and_padded() {
        assert_eq!(hex(&[0x00, 0x0f, 0xff]), "000fff");
    }

    #[test]
    fn same_inputs_same_digest() {
        let mut a = Fingerprint::new();
        a.push("rustc", "1.80.0");
        a.push("target", "x86_64-unknown-linux-gnu");
        let mut b = Fingerprint::new();
        b.push("rustc", "1.80.0");
        b.push("target", "x86_64-unknown-linux-gnu");
        assert_eq!(a.finish(), b.finish());
        assert_eq!(a.short().len(), 16);
    }

    #[test]
    fn order_and_labels_matter() {
        let mut a = Fingerprint::new();
        a.push("a", "1");
        a.push("b", "2");

        let mut b = Fingerprint::new();
        b.push("b", "2");
        b.push("a", "1");
        assert_ne!(a.finish(), b.finish());

        // A value move between fields must not collide, even when the concatenation
        // would look identical.
        let mut c = Fingerprint::new();
        c.push("ab", "");
        let mut d = Fingerprint::new();
        d.push("a", "b");
        assert_ne!(c.finish(), d.finish());
    }

    #[test]
    fn chained_calls_do_not_alias() {
        // "ab"+"c" must differ from "a"+"bc".
        let mut a = Fingerprint::new();
        a.push("v", "ab");
        a.push("v", "c");
        let mut b = Fingerprint::new();
        b.push("v", "a");
        b.push("v", "bc");
        assert_ne!(a.finish(), b.finish());
    }
}
