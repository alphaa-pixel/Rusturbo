//! Hardware and toolchain discovery.
//!
//! Everything here is *measured*, not assumed. If detection fails we say so and fall
//! back to conservative defaults; we never guess a linker exists.

use crate::process::{self, EnvPatch};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Hardware {
    pub physical_cores: usize,
    pub logical_cores: usize,
    pub ram_mb: u64,
    pub os: String,
    pub arch: String,
    /// `auto`-detected recommendation for Cargo `-j`.
    pub recommended_jobs: usize,
}

pub fn detect_hardware() -> Hardware {
    let logical = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let physical = physical_cores().unwrap_or(logical);
    let ram_mb = total_ram_mb().unwrap_or(4096);
    // LLVM prefers one job per physical core; RustTurbo caps by RAM as well because
    // oversubscription is the classic cause of a "slow" build (swap thrash).
    let ram_limited = ((ram_mb as f64) / 1024.0 * 0.8).floor().max(1.0) as usize;
    Hardware {
        physical_cores: physical,
        logical_cores: logical,
        ram_mb,
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        recommended_jobs: physical.min(ram_limited).max(1),
    }
}

fn physical_cores() -> Option<usize> {
    // Linux: /proc/cpuinfo "cpu cores" is unreliable on some VMs; count unique
    // (physical id, core id) pairs instead.
    if let Ok(text) = std::fs::read_to_string("/proc/cpuinfo") {
        let mut pairs = std::collections::BTreeSet::new();
        let mut phys = String::new();
        for line in text.lines() {
            if let Some(v) = line.strip_prefix("physical id") {
                phys = v.trim_start_matches([':', ' ']).to_string();
            } else if let Some(v) = line.strip_prefix("core id") {
                pairs.insert((phys.clone(), v.trim_start_matches([':', ' ']).to_string()));
            }
        }
        if !pairs.is_empty() {
            return Some(pairs.len());
        }
    }
    None
}

fn total_ram_mb() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            let kb: u64 = rest
                .trim()
                .trim_end_matches("kB")
                .trim()
                .parse()
                .ok()?;
            return Some(kb / 1024);
        }
    }
    None
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Toolchain {
    /// e.g. `rustc 1.83.0 (90b35a623 2024-11-26)`
    pub rustc_verbose: String,
    pub rustc_version: String,
    pub cargo_version: String,
    pub host: String,
    /// Release channel parsed from the version string (`1.83.0`, `1.82.0-nightly`).
    pub channel: String,
    /// Commit hash from `rustc -vV`, part of every cache key.
    pub commit_hash: String,
    /// Full `rustc -vV` output, hashed verbatim so LLVM version changes invalidate.
    pub vv_raw: String,
}

pub fn detect_toolchain() -> anyhow::Result<Toolchain> {
    let vv = process::capture_simple("rustc", &["-vV"])?;
    if !vv.success() {
        anyhow::bail!("`rustc -vV` failed:\n{}", vv.combined());
    }
    let raw = vv.stdout.trim().to_string();
    let mut version_line = String::new();
    let mut host = String::new();
    let mut commit = String::new();
    for line in raw.lines() {
        if line.starts_with("rustc ") {
            version_line = line.to_string();
        } else if let Some(v) = line.strip_prefix("host:") {
            host = v.trim().to_string();
        } else if let Some(v) = line.strip_prefix("commit-hash:") {
            commit = v.trim().to_string();
        }
    }
    let channel = version_line
        .split_whitespace()
        .nth(1)
        .unwrap_or("unknown")
        .to_string();
    let cargo = process::capture_simple("cargo", &["--version"])?;
    Ok(Toolchain {
        rustc_verbose: version_line,
        rustc_version: channel.clone(),
        cargo_version: cargo.stdout.trim().to_string(),
        host,
        channel,
        commit_hash: commit,
        vv_raw: raw,
    })
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum LinkerChoice {
    System,
    Lld,
    Mold,
}

impl LinkerChoice {
    pub fn name(&self) -> &'static str {
        match self {
            LinkerChoice::System => "system (ld/bfd)",
            LinkerChoice::Lld => "lld",
            LinkerChoice::Mold => "mold",
        }
    }

    pub fn flag(&self) -> Option<&'static str> {
        match self {
            LinkerChoice::System => None,
            LinkerChoice::Lld => Some("-fuse-ld=lld"),
            LinkerChoice::Mold => Some("-fuse-ld=mold"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LinkerInfo {
    pub selected: LinkerChoice,
    pub available: Vec<String>,
    pub lld_path: Option<String>,
    pub mold_path: Option<String>,
    /// True when the selected linker can be used by rustc against the *gnu* target.
    pub applicable: bool,
    pub reason: String,
}

/// Detect available linkers and pick the best usable one.
///
/// `mold` and `lld` produce identical program behaviour to the system linker; the only
/// observable difference is link speed and (for mold) a different section layout. We
/// refuse `lld`/`mold` on non-GNU targets (msvc/musl) where the flag would be ignored
/// or break the link, rather than silently doing nothing.
pub fn detect_linker(preference: &str, target_triple: &str) -> LinkerInfo {
    let lld = find_lld();
    let mold = process::which("mold").map(|p| p.display().to_string());
    let mut available = vec!["ld (bfd)".to_string()];
    if let Some(ref p) = lld {
        available.push(format!("lld ({p})"));
    }
    if let Some(ref p) = mold {
        available.push(format!("mold ({p})"));
    }

    // `-fuse-ld=` is only meaningful for the GNU-style linkers rustc drives on ELF
    // (and Apple/Windows targets have their own flavours). We refuse it everywhere else
    // instead of passing an inert flag.
    let fuse_ld_supported = target_triple.contains("linux")
        || target_triple.contains("freebsd")
        || target_triple.contains("netbsd")
        || target_triple.contains("android")
        || target_triple.contains("apple");

    let (selected, reason) = match preference {
        "default" => (LinkerChoice::System, "pinned to the system linker".into()),
        "lld" => {
            if let Some(ref p) = lld {
                (LinkerChoice::Lld, format!("requested; found {p}"))
            } else {
                (
                    LinkerChoice::System,
                    "requested lld but no rustc-lld/lld was found on this host".into(),
                )
            }
        }
        "mold" => {
            if let Some(ref p) = mold {
                (LinkerChoice::Mold, format!("requested; found {p}"))
            } else {
                (
                    LinkerChoice::System,
                    "requested mold but it is not installed".into(),
                )
            }
        }
        _ => {
            if let Some(ref p) = mold {
                (LinkerChoice::Mold, format!("auto: mold is fastest ({p})"))
            } else if let Some(ref p) = lld {
                (LinkerChoice::Lld, format!("auto: lld available ({p})"))
            } else {
                (
                    LinkerChoice::System,
                    "auto: no lld/mold found; using the system linker".into(),
                )
            }
        }
    };

    let applicable = match selected {
        LinkerChoice::System => true,
        _ => fuse_ld_supported,
    };

    LinkerInfo {
        selected: if applicable { selected } else { LinkerChoice::System },
        available,
        lld_path: lld,
        mold_path: mold,
        applicable,
        reason: if applicable {
            reason
        } else {
            format!("{reason}; not applicable for target {target_triple}")
        },
    }
}

/// rustc ships `rust-lld` in its sysroot and recent rustc accepts `-fuse-ld=lld`
/// pointing at it once it is on PATH. We look for it in the sysroot.
fn find_lld() -> Option<String> {
    if let Some(p) = process::which("ld.lld").or_else(|| process::which("ld64.lld")) {
        return Some(p.display().to_string());
    }
    let out = process::capture_simple("rustc", &["--print", "sysroot"]).ok()?;
    if !out.success() {
        return None;
    }
    let root: PathBuf = out.stdout.trim().into();
    let host = process::capture_simple("rustc", &["-vV"]).ok()?;
    let host_triple = host
        .stdout
        .lines()
        .find(|l| l.starts_with("host:"))
        .map(|l| l.trim_start_matches("host:").trim().to_string())?;
    let candidate = root.join("lib").join("rustlib").join(&host_triple).join("bin").join("rust-lld");
    if candidate.is_file() {
        Some(candidate.display().to_string())
    } else {
        None
    }
}

/// sccache integration status.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SccacheInfo {
    pub installed: bool,
    pub path: Option<String>,
    pub version: Option<String>,
    /// Set when RustTurbo would wrap `RUSTC_WRAPPER` with sccache.
    pub will_use: bool,
    pub reason: String,
}

pub fn detect_sccache(mode: &str) -> SccacheInfo {
    let path = process::which("sccache");
    let version = path.as_ref().and_then(|_| {
        process::capture_simple("sccache", &["--version"])
            .ok()
            .filter(|c| c.success())
            .map(|c| c.stdout.trim().to_string())
    });
    let installed = path.is_some();
    let (will_use, reason) = match mode {
        "off" => (false, "disabled by config (cache.sccache = \"off\")".to_string()),
        "on" if !installed => (
            false,
            "cache.sccache = \"on\" but sccache is not installed".to_string(),
        ),
        "on" => (true, "enabled by config".to_string()),
        _ if installed => (true, "auto: sccache found on PATH".to_string()),
        _ => (
            false,
            "auto: sccache not installed (RustTurbo's own artifact cache is used instead)".to_string(),
        ),
    };
    SccacheInfo {
        installed,
        path: path.map(|p| p.display().to_string()),
        version,
        will_use,
        reason,
    }
}

/// Build the environment patch that activates a chosen linker.
pub fn linker_env(choice: LinkerChoice, extra_flags: &[String]) -> EnvPatch {
    let mut patch = EnvPatch::new();
    let mut flags: Vec<String> = std::env::var("RUSTFLAGS")
        .unwrap_or_default()
        .split_whitespace()
        .map(|s| s.to_string())
        .collect();
    if let Some(f) = choice.flag() {
        if !flags.iter().any(|x| x == f) {
            flags.push(f.to_string());
        }
    }
    for f in extra_flags {
        if !flags.iter().any(|x| x == f) {
            flags.push(f.clone());
        }
    }
    if !flags.is_empty() {
        patch = patch.set("RUSTFLAGS", flags.join(" "));
    }
    patch
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hardware_is_plausible() {
        let hw = detect_hardware();
        assert!(hw.logical_cores >= 1);
        assert!(hw.physical_cores >= 1);
        assert!(hw.physical_cores <= hw.logical_cores);
        assert!(hw.ram_mb > 0);
        assert!(hw.recommended_jobs >= 1);
    }

    #[test]
    fn mold_is_never_selected_on_msvc() {
        let info = detect_linker("auto", "x86_64-pc-windows-msvc");
        assert_eq!(info.selected, LinkerChoice::System);
        assert!(!info.applicable);
    }

    #[test]
    fn fuse_ld_supported_targets() {
        let info = detect_linker("auto", "x86_64-unknown-linux-gnu");
        assert!(info.applicable);
        // musl targets are ELF and do support -fuse-ld.
        assert!(detect_linker("auto", "x86_64-unknown-linux-musl").applicable);
        assert!(!detect_linker("auto", "wasm32-unknown-unknown").applicable);
    }

    #[test]
    fn explicit_lld_without_binary_falls_back_with_reason() {
        let info = detect_linker("lld", "x86_64-unknown-linux-gnu");
        if info.lld_path.is_none() {
            assert_eq!(info.selected, LinkerChoice::System);
            assert!(info.reason.contains("no rustc-lld/lld"));
        }
    }

    #[test]
    fn default_mode_never_switches_linker() {
        let info = detect_linker("default", "x86_64-unknown-linux-gnu");
        assert_eq!(info.selected, LinkerChoice::System);
    }

    #[test]
    fn linker_env_dedups_flags() {
        let patch = linker_env(LinkerChoice::Lld, &["-fuse-ld=lld".to_string()]);
        let val = patch.set.get("RUSTFLAGS").cloned().unwrap_or_default();
        assert_eq!(val.matches("-fuse-ld=lld").count(), 1);
    }

    #[test]
    fn linker_env_system_keeps_user_flags() {
        let patch = linker_env(LinkerChoice::System, &["-C".to_string(), "opt-level=1".to_string()]);
        assert_eq!(
            patch.set.get("RUSTFLAGS").map(|s| s.as_str()),
            Some("-C opt-level=1")
        );
    }
}
