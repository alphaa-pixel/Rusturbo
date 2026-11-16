//! Persistent dependency artifact cache.
//!
//! # What is actually being cached
//!
//! RustTurbo caches *dependency* build products, keyed on a fingerprint of everything
//! that can change their bytes: the exact rustc (`rustc -vV`, verbatim), the target
//! triple, the resolved crate graph including versions and enabled features, the
//! profile settings that affect codegen, and the host environment variables that Cargo
//! itself forwards to build scripts (`CARGO_*`, `RUSTFLAGS`, `TARGET`, ...).
//!
//! # Honest limits
//!
//! A cache cannot make a first-ever build of a crate faster — the artifact has to exist
//! somewhere first. What it does:
//!
//! * `shared_targets`: reuses one `CARGO_TARGET_DIR` across projects whose dependency
//!   graph fingerprint matches, so a brand-new project with a familiar dependency set
//!   skips compiling those dependencies entirely (this is the single biggest win, and
//!   it is measurable).
//! * `entries`: a portable, copyable snapshot of a built dependency set for *different*
//!   target dirs. Used when a project must keep its own `target/` (different profile,
//!   different crate set) but the deps are identical.
//!
//! Both are keyed identically, so a cache hit can be explained, audited and invalidated.

use crate::fingerprint::Fingerprint;
use crate::util;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct KeyInput {
    pub label: String,
    pub value: String,
}

/// A cache key plus the full list of inputs, so `--explain` can show exactly why a
/// build missed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CacheKey {
    pub short: String,
    pub full: String,
    pub inputs: Vec<KeyInput>,
}

impl CacheKey {
    pub fn from_inputs(pairs: Vec<(String, String)>) -> Self {
        let mut fp = Fingerprint::new();
        for (k, v) in &pairs {
            fp.push(k, v);
        }
        Self {
            short: fp.short(),
            full: fp.finish(),
            inputs: pairs
                .into_iter()
                .map(|(label, value)| KeyInput { label, value })
                .collect(),
        }
    }

    pub fn input(&self, label: &str) -> Option<&str> {
        self.inputs
            .iter()
            .find(|i| i.label == label)
            .map(|i| i.value.as_str())
    }
}

/// Compute the fingerprint of a dependency graph: sorted `name-version feat1,feat2`.
pub fn graph_fingerprint(packages: &[(String, String, Vec<String>)]) -> String {
    let mut lines: Vec<String> = packages
        .iter()
        .map(|(name, ver, mut feats)| {
            feats.sort();
            feats.dedup();
            format!("{name}-{ver}[{}]", feats.join(","))
        })
        .collect();
    lines.sort();
    let mut fp = Fingerprint::new();
    fp.push_lines("packages", &lines);
    fp.finish()
}

/// Environment variables that Cargo forwards to build scripts and that `rustc` reads.
/// Anything here that differs must invalidate the cache.
pub fn relevant_env() -> BTreeMap<String, String> {
    let keys = [
        "RUSTFLAGS",
        "CARGO_ENCODED_RUSTFLAGS",
        "RUSTC",
        "RUSTC_WRAPPER",
        "RUSTC_WORKSPACE_WRAPPER",
        "CARGO_TARGET_DIR",
        "CARGO_BUILD_TARGET",
        "CARGO_BUILD_RUSTFLAGS",
        "CC",
        "CXX",
        "AR",
        "CFLAGS",
        "CXXFLAGS",
        "PKG_CONFIG_PATH",
        "MACOSX_DEPLOYMENT_TARGET",
        "OPENSSL_DIR",
        "OPENSSL_LIB_DIR",
        "OPENSSL_INCLUDE_DIR",
        "SQLITE_MAX_VARIABLE_NUMBER",
        "PROTOC",
        "ZSTD_SYS_USE_PKG_CONFIG",
    ];
    let mut out = BTreeMap::new();
    for k in keys {
        if let Ok(v) = std::env::var(k) {
            out.insert(k.to_string(), v);
        }
    }
    out
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EntryMeta {
    pub key: String,
    pub rusturbo_version: String,
    pub created_at: String,
    pub created_unix: u64,
    pub last_used_unix: u64,
    pub toolchain: String,
    pub target: String,
    pub profile: String,
    pub crate_count: usize,
    pub graph_hash: String,
    pub file_count: u64,
    pub size_bytes: u64,
    pub has_build_scripts: bool,
    pub inputs: Vec<KeyInput>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CacheStats {
    pub root: String,
    pub entries: usize,
    pub shared: usize,
    pub size_bytes: u64,
    pub oldest: Option<String>,
    pub newest: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ArtifactCache {
    root: PathBuf,
    max_size_gb: u64,
    enabled: bool,
}

impl ArtifactCache {
    pub fn new(root: PathBuf, max_size_gb: u64, enabled: bool) -> Self {
        Self {
            root,
            max_size_gb,
            enabled,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    fn entries_dir(&self) -> PathBuf {
        self.root.join("entries")
    }

    fn shared_dir(&self) -> PathBuf {
        self.root.join("shared")
    }

    pub fn ensure(&self) -> Result<()> {
        std::fs::create_dir_all(self.entries_dir())
            .with_context(|| format!("failed to create {}", self.entries_dir().display()))?;
        std::fs::create_dir_all(self.shared_dir())
            .with_context(|| format!("failed to create {}", self.shared_dir().display()))?;
        Ok(())
    }

    // ----- portable entries -------------------------------------------------

    pub fn entry_path(&self, key: &str) -> PathBuf {
        self.entries_dir().join(key)
    }

    pub fn contains(&self, key: &str) -> bool {
        self.entry_path(key).join("meta.json").is_file()
    }

    pub fn get(&self, key: &str) -> Option<Buf> {
        if !self.enabled {
            return None;
        }
        let dir = self.entry_path(key);
        let meta_text = std::fs::read_to_string(dir.join("meta.json")).ok()?;
        let meta: EntryMeta = serde_json::from_str(&meta_text).ok()?;
        Some(Buf { dir, meta })
    }

    /// Store `source` (a directory tree of dependency artifacts) under `key`.
    /// Existing entries are left untouched: first writer wins, so a partially copied
    /// entry can never overwrite a complete one.
    pub fn put(&self, key: &str, source: &Path, meta: EntryMeta) -> Result<Option<PathBuf>> {
        if !self.enabled {
            return Ok(None);
        }
        self.ensure()?;
        let dst = self.entry_path(key);
        if dst.join("meta.json").is_file() {
            return Ok(Some(dst));
        }
        let tmp = self.entries_dir().join(format!(".tmp-{key}-{}", std::process::id()));
        util::remove_dir_all_if_exists(&tmp).ok();
        std::fs::create_dir_all(&tmp)?;
        let stats = copy_tree(source, &tmp)?;
        let mut meta = meta;
        meta.file_count = stats.files;
        meta.size_bytes = stats.bytes;
        meta.key = key.to_string();
        std::fs::write(tmp.join("meta.json"), serde_json::to_string_pretty(&meta)?)?;
        // Atomic publish. If another process won the race, keep theirs.
        match std::fs::rename(&tmp, &dst) {
            Ok(()) => Ok(Some(dst)),
            Err(_) => {
                util::remove_dir_all_if_exists(&tmp).ok();
                Ok(Some(dst))
            }
        }
    }

    /// Copy an entry into `dest` (typically a project `target/<profile>/deps`).
    /// Uses hard links when source and destination share a filesystem, falling back to
    /// byte copies. Returns bytes written.
    pub fn materialize(&self, key: &str, dest: &Path) -> Result<u64> {
        let Some(buf) = self.get(key) else {
            anyhow::bail!("cache entry {key} not found");
        };
        // Refresh LRU timestamp; failure to touch is never fatal.
        let _ = touch(&buf.dir.join("meta.json"), &buf.meta);
        let stats = copy_tree(&buf.dir, dest)?;
        Ok(stats.bytes)
    }

    pub fn touch_entry(&self, key: &str) {
        if let Some(buf) = self.get(key) {
            let _ = touch(&buf.dir.join("meta.json"), &buf.meta);
        }
    }

    // ----- shared target dirs ----------------------------------------------

    pub fn shared_target(&self, key: &str) -> PathBuf {
        self.shared_dir().join(key)
    }

    pub fn shared_exists(&self, key: &str) -> bool {
        self.shared_target(key).join(".rusturbo-ready").is_file()
    }

    pub fn mark_shared_ready(&self, key: &str, meta: EntryMeta) -> Result<()> {
        self.ensure()?;
        let dir = self.shared_target(key);
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join("meta.json"), serde_json::to_string_pretty(&meta)?)?;
        std::fs::write(dir.join(".rusturbo-ready"), b"ok")?;
        Ok(())
    }

    // ----- maintenance ------------------------------------------------------

    pub fn list_entries(&self) -> Vec<EntryMeta> {
        let mut out = Vec::new();
        let Ok(rd) = std::fs::read_dir(self.entries_dir()) else {
            return out;
        };
        for e in rd.flatten() {
            let p = e.path().join("meta.json");
            if let Ok(text) = std::fs::read_to_string(&p) {
                if let Ok(m) = serde_json::from_str::<EntryMeta>(&text) {
                    out.push(m);
                }
            }
        }
        out.sort_by_key(|m| std::cmp::Reverse(m.last_used_unix));
        out
    }

    pub fn stats(&self) -> CacheStats {
        let entries = self.list_entries();
        let size: u64 = entries.iter().map(|e| e.size_bytes).sum();
        let shared = std::fs::read_dir(self.shared_dir())
            .map(|rd| rd.flatten().count())
            .unwrap_or(0);
        let oldest = entries.iter().map(|e| e.created_at.clone()).min();
        let newest = entries.iter().map(|e| e.created_at.clone()).max();
        CacheStats {
            root: self.root.display().to_string(),
            entries: entries.len(),
            shared,
            size_bytes: size,
            oldest,
            newest,
        }
    }

    /// Evict least-recently-used entries until the cache fits `max_size_gb`.
    /// Returns the number of evicted entries.
    pub fn evict(&self) -> Result<usize> {
        if self.max_size_gb == 0 {
            return Ok(0);
        }
        let limit = self.max_size_gb * 1024 * 1024 * 1024;
        let entries = self.list_entries(); // newest-used first
        let mut total: u64 = entries.iter().map(|e| e.size_bytes).sum();
        let mut removed = 0;
        for e in entries.iter().rev() {
            if total <= limit {
                break;
            }
            let dir = self.entry_path(&e.key);
            if util::remove_dir_all_if_exists(&dir).is_ok() {
                total = total.saturating_sub(e.size_bytes);
                removed += 1;
            }
        }
        Ok(removed)
    }

    pub fn clear(&self) -> Result<()> {
        util::remove_dir_all_if_exists(&self.entries_dir())?;
        util::remove_dir_all_if_exists(&self.shared_dir())?;
        self.ensure()
    }
}

pub struct Buf {
    pub dir: PathBuf,
    pub meta: EntryMeta,
}

fn touch(meta_path: &Path, meta: &EntryMeta) -> Result<()> {
    let mut m = meta.clone();
    m.last_used_unix = util::unix_now();
    std::fs::write(meta_path, serde_json::to_string_pretty(&m)?)?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TreeStats {
    pub files: u64,
    pub bytes: u64,
    pub hardlinked: u64,
}

/// Recursively copy `src` into `dst`, preferring hard links.
///
/// Hard links are safe here because artifacts are immutable once written by rustc;
/// nothing in the Rust toolchain modifies a finished `.rlib`/`.rmeta` in place.
pub fn copy_tree(src: &Path, dst: &Path) -> Result<TreeStats> {
    let mut stats = TreeStats {
        files: 0,
        bytes: 0,
        hardlinked: 0,
    };
    copy_tree_inner(src, dst, &mut stats)?;
    Ok(stats)
}

fn copy_tree_inner(src: &Path, dst: &Path, stats: &mut TreeStats) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    let rd = match std::fs::read_dir(src) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", src.display())),
    };
    for entry in rd.flatten() {
        let ft = entry.file_type()?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        // Never copy a cache's own metadata bookkeeping recursively.
        if entry.file_name() == "meta.json" || entry.file_name() == ".rusturbo-ready" {
            continue;
        }
        if ft.is_dir() {
            copy_tree_inner(&from, &to, stats)?;
        } else if ft.is_file() {
            let len = entry.metadata().map(|m| m.len()).unwrap_or(0);
            if std::fs::hard_link(&from, &to).is_ok() {
                stats.hardlinked += 1;
            } else {
                std::fs::copy(&from, &to)
                    .with_context(|| format!("copying {} -> {}", from.display(), to.display()))?;
            }
            stats.files += 1;
            stats.bytes += len;
        }
        // Symlinks inside a target dir are not produced by rustc; skip them rather
        // than risk escaping the cache root.
    }
    Ok(())
}

/// Why a given graph is or is not safe to share.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CacheDecision {
    pub cacheable: bool,
    pub reason: String,
    pub warnings: Vec<String>,
}

/// Decide whether a dependency graph may be shared through the cache.
///
/// Build scripts are the one real hazard: `openssl-sys` or `libsqlite3-sys` can embed
/// the local library version/features into the compiled artifact, so an artifact built
/// on a machine with a different system library is *not* equivalent. We surface that
/// instead of pretending it is safe.
pub fn policy_check(
    build_script_packages: &[String],
    build_scripts_mode: &str,
    dependents_of_sys_crates: &[String],
) -> CacheDecision {
    let mut warnings = Vec::new();
    if !dependents_of_sys_crates.is_empty() {
        warnings.push(format!(
            "system-library crates in graph ({}); artifacts depend on host libraries",
            util::truncate_middle(&dependents_of_sys_crates.join(", "), 90)
        ));
    }
    if build_script_packages.is_empty() {
        return CacheDecision {
            cacheable: true,
            reason: "no build scripts in the dependency graph".into(),
            warnings,
        };
    }
    match build_scripts_mode {
        "off" => CacheDecision {
            cacheable: true,
            reason: format!(
                "{} build-script crates present; sharing allowed (cache.build_scripts = \"off\")",
                build_script_packages.len()
            ),
            warnings,
        },
        "strict" => CacheDecision {
            cacheable: false,
            reason: format!(
                "{} build-script crates present; refusing to share (cache.build_scripts = \"strict\")",
                build_script_packages.len()
            ),
            warnings,
        },
        _ => {
            warnings.push(format!(
                "{} crate(s) run build scripts ({}); their artifacts can embed host state",
                build_script_packages.len(),
                util::truncate_middle(&build_script_packages.join(", "), 90)
            ));
            CacheDecision {
                cacheable: true,
                reason: format!(
                    "{} build-script crates present; sharing with warnings (cache.build_scripts = \"warn\")",
                    build_script_packages.len()
                ),
                warnings,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "rusturbo-test-{tag}-{}-{}",
            std::process::id(),
            util::unix_now()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn meta(key: &str) -> EntryMeta {
        EntryMeta {
            key: key.to_string(),
            rusturbo_version: "0.1.0".into(),
            created_at: util::iso8601_now(),
            created_unix: util::unix_now(),
            last_used_unix: util::unix_now(),
            toolchain: "rustc 1.83.0".into(),
            target: "x86_64-unknown-linux-gnu".into(),
            profile: "dev".into(),
            crate_count: 3,
            graph_hash: "abc".into(),
            file_count: 0,
            size_bytes: 0,
            has_build_scripts: false,
            inputs: vec![],
        }
    }

    #[test]
    fn key_changes_when_any_input_changes() {
        let base = vec![
            ("rustc".to_string(), "1.83.0".to_string()),
            ("target".to_string(), "x86_64-unknown-linux-gnu".to_string()),
            ("graph".to_string(), "aaa".to_string()),
        ];
        let k1 = CacheKey::from_inputs(base.clone());
        let mut changed = base.clone();
        changed[0].1 = "1.84.0".into();
        let k2 = CacheKey::from_inputs(changed);
        assert_ne!(k1.short, k2.short);
        assert_ne!(k1.full, k2.full);
        assert_eq!(k1.short.len(), 16);
        assert_eq!(k1.input("graph"), Some("aaa"));
    }

    #[test]
    fn graph_fingerprint_is_order_and_feature_order_insensitive() {
        let a = graph_fingerprint(&[
            ("serde".into(), "1.0.0".into(), vec!["derive".into(), "std".into()]),
            ("tokio".into(), "1.40.0".into(), vec!["rt-multi-thread".into()]),
        ]);
        let b = graph_fingerprint(&[
            ("tokio".into(), "1.40.0".into(), vec!["rt-multi-thread".into()]),
            ("serde".into(), "1.0.0".into(), vec!["std".into(), "derive".into()]),
        ]);
        assert_eq!(a, b);
        let c = graph_fingerprint(&[
            ("serde".into(), "1.0.0".into(), vec!["std".into()]),
            ("tokio".into(), "1.40.0".into(), vec!["rt-multi-thread".into()]),
        ]);
        assert_ne!(a, c, "dropping a feature must invalidate");
    }

    #[test]
    fn put_then_get_roundtrip_with_file_contents() {
        let root = temp_dir("rt");
        let src = temp_dir("src");
        std::fs::write(src.join("libserde.rlib"), b"artifact").unwrap();
        std::fs::create_dir_all(src.join("nested")).unwrap();
        std::fs::write(src.join("nested/x.rmeta"), b"meta").unwrap();

        let cache = ArtifactCache::new(root.join("cache"), 20, true);
        let key = "deadbeefdeadbeef";
        cache.put(key, &src, meta(key)).unwrap();

        let buf = cache.get(key).expect("hit");
        assert_eq!(buf.meta.crate_count, 3);
        assert_eq!(buf.meta.file_count, 2);
        assert_eq!(buf.meta.size_bytes, 8 + 4);

        let out = temp_dir("out");
        let bytes = cache.materialize(key, &out).unwrap();
        assert_eq!(bytes, 12);
        assert_eq!(std::fs::read(out.join("libserde.rlib")).unwrap(), b"artifact");
        assert_eq!(std::fs::read(out.join("nested/x.rmeta")).unwrap(), b"meta");
        // meta.json must not be copied into a target dir.
        assert!(!out.join("meta.json").exists());
    }

    #[test]
    fn missing_entries_miss_cleanly() {
        let root = temp_dir("miss");
        let cache = ArtifactCache::new(root, 20, true);
        assert!(cache.get("nope").is_none());
        assert!(!cache.contains("nope"));
        assert!(cache.materialize("nope", &root.join("x")).is_err());
    }

    #[test]
    fn disabled_cache_never_hits() {
        let root = temp_dir("disabled");
        let src = temp_dir("disabled-src");
        std::fs::write(src.join("a.rlib"), b"x").unwrap();
        let cache = ArtifactCache::new(root, 20, false);
        assert!(cache.put("k", &src, meta("k")).unwrap().is_none());
        assert!(cache.get("k").is_none());
    }

    #[test]
    fn corrupt_meta_is_a_miss_not_a_crash() {
        let root = temp_dir("corrupt");
        let cache = ArtifactCache::new(root.clone(), 20, true);
        cache.ensure().unwrap();
        let dir = cache.entry_path("bad");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("meta.json"), b"{not json").unwrap();
        assert!(cache.get("bad").is_none());
    }

    #[test]
    fn eviction_respects_size_limit_and_keeps_newest() {
        let root = temp_dir("evict");
        let cache = ArtifactCache::new(root, 1, true); // 1 GiB
        cache.ensure().unwrap();
        // Fake two 700 MiB entries claimed in metadata (we do not write the bytes).
        for (k, used) in [("old", 1_000u64), ("new", 2_000u64)] {
            let dir = cache.entry_path(k);
            std::fs::create_dir_all(&dir).unwrap();
            let mut m = meta(k);
            m.size_bytes = 700 * 1024 * 1024;
            m.last_used_unix = used;
            std::fs::write(dir.join("meta.json"), serde_json::to_string(&m).unwrap()).unwrap();
        }
        assert_eq!(cache.list_entries().len(), 2);
        let removed = cache.evict().unwrap();
        assert_eq!(removed, 1);
        assert!(cache.contains("new"), "most recently used must survive");
        assert!(!cache.contains("old"));
    }

    #[test]
    fn policy_flags_build_scripts_per_mode() {
        let bs = vec!["openssl-sys-0.9.104".to_string()];
        let warn = policy_check(&bs, "warn", &["openssl-sys".to_string()]);
        assert!(warn.cacheable);
        assert_eq!(warn.warnings.len(), 2);

        let strict = policy_check(&bs, "strict", &[]);
        assert!(!strict.cacheable);

        let clean = policy_check(&[], "warn", &[]);
        assert!(clean.cacheable);
        assert!(clean.warnings.is_empty());

        let off = policy_check(&bs, "off", &[]);
        assert!(off.cacheable);
        assert!(off.reason.contains("sharing allowed"));
    }

    #[test]
    fn shared_target_readiness_marker() {
        let root = temp_dir("shared");
        let cache = ArtifactCache::new(root, 20, true);
        assert!(!cache.shared_exists("k1"));
        cache.mark_shared_ready("k1", meta("k1")).unwrap();
        assert!(cache.shared_exists("k1"));
        assert!(cache.shared_target("k1").join("meta.json").is_file());
    }

    #[test]
    fn stats_aggregate_sizes() {
        let root = temp_dir("stats");
        let src = temp_dir("stats-src");
        std::fs::write(src.join("f.rlib"), vec![0u8; 32]).unwrap();
        let cache = ArtifactCache::new(root, 20, true);
        cache.put("a", &src, meta("a")).unwrap();
        let s = cache.stats();
        assert_eq!(s.entries, 1);
        assert_eq!(s.size_bytes, 32);
        assert!(s.oldest.is_some());
    }

    #[test]
    fn copy_tree_is_idempotent() {
        let a = temp_dir("cp-a");
        let b = temp_dir("cp-b");
        std::fs::write(a.join("x.rlib"), b"123").unwrap();
        let s1 = copy_tree(&a, &b).unwrap();
        let s2 = copy_tree(&a, &b).unwrap();
        assert_eq!(s1.files, 1);
        assert_eq!(s2.files, 1);
        assert_eq!(std::fs::read(b.join("x.rlib")).unwrap(), b"123");
    }
}
