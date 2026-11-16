//! `rusturbo.toml` — the project-local configuration file.
//!
//! Design rule: every field has a default that is *safe* (no behaviour change to the
//! built program) and every knob that could change the produced binary is documented
//! in the README under "correctness contract".

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const CONFIG_FILE: &str = "rusturbo.toml";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub profile: ProfileCfg,
    pub cache: CacheCfg,
    pub linker: LinkerCfg,
    pub actix: ActixCfg,
    pub run: RunCfg,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            profile: ProfileCfg::default(),
            cache: CacheCfg::default(),
            linker: LinkerCfg::default(),
            actix: ActixCfg::default(),
            run: RunCfg::default(),
        }
    }
}

/// Compilation profile knobs. These map onto Cargo profile settings; RustTurbo writes
/// them into `rusturbo.toml` and translates them into `RUSTFLAGS`/`--config` arguments
/// at build time so the project's own `Cargo.toml` is never rewritten behind your back.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct ProfileCfg {
    /// `balanced` | `fastest` | `safe`
    pub preset: String,
    /// Debug info level for *dependencies* only. `0` is much faster to compile and
    /// link; it costs you dependency backtraces in local debugging.
    pub dep_debug: u8,
    /// Debug info level for workspace crates (your own code).
    pub debug: u8,
    /// 0 = auto (derived from cores + RAM).
    pub jobs: usize,
    /// `--offline`-friendly builds are unaffected; this only toggles Cargo's
    /// incremental compilation for *your* crates.
    pub incremental: bool,
    /// Increase codegen units for development profiles (faster codegen, slower runtime).
    pub parallel_codegen: bool,
}

impl Default for ProfileCfg {
    fn default() -> Self {
        Self {
            preset: "balanced".into(),
            dep_debug: 0,
            debug: 1,
            jobs: 0,
            incremental: true,
            parallel_codegen: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct CacheCfg {
    /// Master switch for RustTurbo's own artifact cache.
    pub enabled: bool,
    /// Root directory. Empty = platform default (`~/.cache/rusturbo`).
    pub dir: String,
    /// Reuse the *same* `CARGO_TARGET_DIR` for projects whose dependency graph
    /// fingerprint matches. This is the mechanism that makes a brand-new project with
    /// a familiar dependency set start without recompiling those dependencies.
    pub shared_targets: bool,
    /// Soft cap in GiB before LRU eviction kicks in.
    pub max_size_gb: u64,
    /// `auto` | `on` | `off` — wrap rustc with sccache when available.
    pub sccache: String,
    /// Deps with build scripts may embed environment data in their artifacts.
    /// `strict` refuses to share artifacts for graphs containing build scripts.
    pub build_scripts: String,
}

impl Default for CacheCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            dir: String::new(),
            shared_targets: true,
            max_size_gb: 20,
            sccache: "auto".into(),
            build_scripts: "warn".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct LinkerCfg {
    /// `auto` | `default` | `lld` | `mold`
    pub mode: String,
    /// Extra flags appended to `RUSTFLAGS` for every build RustTurbo performs.
    pub extra_flags: Vec<String>,
}

impl Default for LinkerCfg {
    fn default() -> Self {
        Self {
            mode: "auto".into(),
            extra_flags: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct ActixCfg {
    /// `auto` | `off`
    pub preset: String,
    /// Apply the Actix-tuned dependency profile block.
    pub tuned_profiles: bool,
}

impl Default for ActixCfg {
    fn default() -> Self {
        Self {
            preset: "auto".into(),
            tuned_profiles: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct RunCfg {
    /// `dev` | `release`
    pub profile: String,
}

impl Default for RunCfg {
    fn default() -> Self {
        Self {
            profile: "dev".into(),
        }
    }
}

impl Config {
    /// Load `rusturbo.toml` from `root`, falling back to defaults when absent.
    /// Returns the config plus whether a file was actually found.
    pub fn load(root: &Path) -> Result<(Self, bool)> {
        let path = root.join(CONFIG_FILE);
        if !path.exists() {
            return Ok((Self::default(), false));
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let cfg: Config = toml::from_str(&text)
            .with_context(|| format!("failed to parse {}", path.display()))?;
        cfg.validate()?;
        Ok((cfg, true))
    }

    pub fn validate(&self) -> Result<()> {
        let valid_presets = ["balanced", "fastest", "safe"];
        if !valid_presets.contains(&self.profile.preset.as_str()) {
            anyhow::bail!(
                "unknown profile.preset `{}` (expected one of: {})",
                self.profile.preset,
                valid_presets.join(", ")
            );
        }
        let valid_linkers = ["auto", "default", "lld", "mold"];
        if !valid_linkers.contains(&self.linker.mode.as_str()) {
            anyhow::bail!(
                "unknown linker.mode `{}` (expected one of: {})",
                self.linker.mode,
                valid_linkers.join(", ")
            );
        }
        if !["auto", "on", "off"].contains(&self.cache.sccache.as_str()) {
            anyhow::bail!("cache.sccache must be auto, on or off");
        }
        if !["strict", "warn", "off"].contains(&self.cache.build_scripts.as_str()) {
            anyhow::bail!("cache.build_scripts must be strict, warn or off");
        }
        if self.profile.dep_debug > 2 || self.profile.debug > 2 {
            anyhow::bail!("debug info levels must be 0, 1 or 2");
        }
        Ok(())
    }

    pub fn to_toml(&self) -> String {
        // `toml::to_string` cannot fail for this shape (no maps with non-string keys,
        // all values are primitives/arrays/tables), but we degrade gracefully anyway.
        toml::to_string_pretty(self).unwrap_or_else(|_| String::from("# rusturbo.toml\n"))
    }

    pub fn write(&self, root: &Path) -> Result<PathBuf> {
        let path = root.join(CONFIG_FILE);
        std::fs::write(&path, self.to_toml())
            .with_context(|| format!("failed to write {}", path.display()))?;
        Ok(path)
    }

    pub fn cache_dir(&self) -> PathBuf {
        if self.cache.dir.is_empty() {
            crate::util::cache_root()
        } else {
            PathBuf::from(&self.cache.dir)
        }
    }

    /// Effective job count given detected hardware.
    pub fn effective_jobs(&self, cores: usize, ram_mb: u64) -> usize {
        if self.profile.jobs > 0 {
            return self.profile.jobs;
        }
        // Each rustc/LLVM instance for a large crate can transiently want ~1 GiB.
        // Oversubscribing RAM causes swap/OOM thrash which is far worse than serialising.
        let ram_limited = ((ram_mb as f64) / 1024.0 * 0.8).floor().max(1.0) as usize;
        cores.max(1).min(ram_limited.max(1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_safe_and_enabled() {
        let c = Config::default();
        assert!(c.cache.enabled);
        assert_eq!(c.profile.preset, "balanced");
        assert_eq!(c.linker.mode, "auto");
        c.validate().unwrap();
    }

    #[test]
    fn toml_roundtrip_preserves_fields() {
        let mut c = Config::default();
        c.profile.jobs = 7;
        c.cache.shared_targets = false;
        c.linker.extra_flags = vec!["-C".into(), "opt-level=1".into()];
        let text = c.to_toml();
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(c, back);
    }

    #[test]
    fn partial_toml_uses_defaults_for_missing_keys() {
        let text = "[cache]\nshared_targets = false\n";
        let c: Config = toml::from_str(text).unwrap();
        assert!(!c.cache.shared_targets);
        assert!(c.cache.enabled); // untouched default
        assert_eq!(c.profile.preset, "balanced");
    }

    #[test]
    fn unknown_keys_are_rejected_loudly() {
        let err = toml::from_str::<Config>("[cache]\nshaerd_targets = true\n").unwrap_err();
        assert!(err.to_string().contains("shaerd_targets"));
    }

    #[test]
    fn validation_catches_typos_in_enum_fields() {
        let mut c = Config::default();
        c.linker.mode = "moldd".into();
        assert!(c.validate().is_err());
    }

    #[test]
    fn jobs_respect_ram_cap() {
        let c = Config::default();
        // 16 cores but only 2 GiB: cap at ~1 job to avoid thrash.
        assert_eq!(c.effective_jobs(16, 2048), 1);
        assert_eq!(c.effective_jobs(4, 65_536), 4);
        let mut explicit = Config::default();
        explicit.profile.jobs = 3;
        assert_eq!(explicit.effective_jobs(16, 2048), 3);
    }
}
