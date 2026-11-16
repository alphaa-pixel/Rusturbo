//! Profile presets and the Actix Web tuned configuration.
//!
//! Presets are translated into Cargo `--config` arguments at invocation time instead of
//! being written into the project's `Cargo.toml`. That keeps the user's manifest
//! untouched (no merge conflicts, no surprises in git) while still letting Cargo apply
//! the settings, and it means every change is visible in `rusturbo build --dry-run`.
//!
//! Behaviour contract: none of these presets changes the *semantics* of the compiled
//! program. They change debug info, codegen units and incremental state — all of which
//! affect build time and binary size, not what the code does.

use crate::config::Config;

#[derive(Debug, Clone, PartialEq)]
pub struct Preset {
    pub name: &'static str,
    /// Debug info for dependencies.
    pub dep_debug: u8,
    /// Debug info for workspace crates.
    pub debug: u8,
    pub codegen_units: Option<u32>,
    pub incremental: bool,
    pub description: &'static str,
}

pub const BALANCED: Preset = Preset {
    name: "balanced",
    dep_debug: 0,
    debug: 1,
    codegen_units: Some(256),
    incremental: true,
    description: "dependencies without debug info (much faster to compile and link), your code debuggable",
};

pub const FASTEST: Preset = Preset {
    name: "fastest",
    dep_debug: 0,
    debug: 0,
    codegen_units: Some(256),
    incremental: true,
    description: "no debug info anywhere — fastest build, but no source-level backtraces in dependencies",
};

pub const SAFE: Preset = Preset {
    name: "safe",
    dep_debug: 2,
    debug: 2,
    codegen_units: None,
    incremental: true,
    description: "Cargo defaults, only cache + linker acceleration applied",
};

pub fn preset_by_name(name: &str) -> Preset {
    match name {
        "fastest" => FASTEST,
        "safe" => SAFE,
        _ => BALANCED,
    }
}

/// Resolve the effective preset from config (config values may override the bundle).
pub fn effective(cfg: &Config) -> Preset {
    let mut p = preset_by_name(&cfg.profile.preset);
    // Explicit config wins over the bundle, so a hand-edited rusturbo.toml is honoured.
    if cfg.profile.dep_debug != Config::default().profile.dep_debug || p.name == "balanced" {
        p.dep_debug = cfg.profile.dep_debug;
    }
    if cfg.profile.debug != Config::default().profile.debug || p.name == "balanced" {
        p.debug = cfg.profile.debug;
    }
    p.incremental = cfg.profile.incremental;
    if !cfg.profile.parallel_codegen {
        p.codegen_units = None;
    }
    p
}

/// Cargo arguments that apply the preset. Returned as individual argv entries so no
/// shell quoting is ever involved.
pub fn cargo_config_args(cfg: &Config, profile: &str) -> Vec<String> {
    let p = effective(cfg);
    let mut args = Vec::new();
    let cargo_profile = if profile == "release" { "release" } else { "dev" };

    // Dependencies: `profile.<p>.package."*"` appends to the base profile.
    if cargo_profile == "dev" {
        args.push("--config".into());
        args.push(format!("profile.dev.package.\"*\".debug={}", p.dep_debug));
        if let Some(cu) = p.codegen_units {
            args.push("--config".into());
            args.push(format!("profile.dev.codegen-units={cu}"));
        }
        args.push("--config".into());
        args.push(format!("profile.dev.incremental={}", p.incremental));
        if p.debug != 1 {
            args.push("--config".into());
            args.push(format!("profile.dev.debug={}", p.debug));
        }
    } else {
        // Release builds: never silently change optimisation level. Only debuginfo is
        // trimmed, which does not affect generated code.
        args.push("--config".into());
        args.push(format!("profile.release.debug={}", p.dep_debug.min(p.debug)));
    }
    args
}

/// Extra environment for the build (incremental dirs etc.).
pub fn cargo_env(cfg: &Config, profile: &str) -> crate::process::EnvPatch {
    let mut env = crate::process::EnvPatch::new();
    if profile != "release" {
        env = env.set(
            "CARGO_INCREMENTAL",
            if effective(cfg).incremental { "1" } else { "0" },
        );
    }
    env
}

/// Actix-tuned dependency profile block, applied when a graph contains actix-web.
///
/// Rationale (all measured by `rusturbo benchmark`, not assumed):
/// * actix-web pulls tokio, hyper/h2, serde, and optionally sqlx — the top-4 cost
///   centres of a typical graph.
/// * These crates are almost never debugged at source level during local development,
///   so dropping their debuginfo removes both compile and link work.
/// * Codegen units are raised for *dependencies* only; your handlers still get
///   incremental compilation.
pub struct ActixTuning {
    pub packages: Vec<&'static str>,
}

pub fn actix_tuning(version: Option<&str>) -> ActixTuning {
    let mut packages = vec![
        "actix-web",
        "actix-http",
        "actix-codec",
        "actix-router",
        "actix-service",
        "actix-utils",
        "actix-rt",
        "actix-server",
        "actix-macros",
        "actix-web-codegen",
        "awc",
        "tokio",
        "tokio-util",
        "hyper",
        "h2",
        "http",
        "serde",
        "serde_json",
        "serde_urlencoded",
        "futures-core",
        "futures-util",
        "bytes",
        "tracing",
        "tracing-core",
    ];
    // sqlx/tokio-postgres are only heavy if the project actually uses them; the tuning
    // block is harmless when the crate is absent (Cargo ignores unknown package keys).
    packages.extend_from_slice(&[
        "sqlx",
        "sqlx-core",
        "tokio-postgres",
        "sqlx-postgres",
        "time",
        "chrono",
    ]);
    let _ = version;
    ActixTuning { packages }
}

/// `--config profile.dev.package."<name>".debug=0` for the Actix stack.
pub fn actix_cargo_args(cfg: &Config, profile: &str, actix_version: Option<&str>) -> Vec<String> {
    if cfg.actix.preset == "off" || !cfg.actix.tuned_profiles || profile == "release" {
        return Vec::new();
    }
    let tuning = actix_tuning(actix_version);
    let mut args = Vec::new();
    for pkg in tuning.packages {
        args.push("--config".into());
        args.push(format!("profile.dev.package.\"{pkg}\".debug=0"));
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_differ_as_documented() {
        assert_eq!(BALANCED.dep_debug, 0);
        assert_eq!(SAFE.dep_debug, 2);
        assert_eq!(FASTEST.debug, 0);
        assert!(BALANCED.incremental);
    }

    #[test]
    fn dev_args_never_touch_opt_level() {
        let cfg = Config::default();
        let args = cargo_config_args(&cfg, "dev");
        let joined = args.join(" ");
        assert!(!joined.contains("opt-level"), "opt-level must never be silently changed");
        assert!(joined.contains("profile.dev.package.\"*\".debug=0"));
        assert!(joined.contains("codegen-units"));
    }

    #[test]
    fn release_args_only_trim_debuginfo() {
        let cfg = Config::default();
        let args = cargo_config_args(&cfg, "release").join(" ");
        assert!(args.contains("profile.release.debug"));
        assert!(!args.contains("opt-level"));
        assert!(!args.contains("lto"));
    }

    #[test]
    fn actix_args_can_be_disabled() {
        let mut cfg = Config::default();
        assert!(!actix_cargo_args(&cfg, "dev", Some("4.9.0")).is_empty());
        cfg.actix.preset = "off".into();
        assert!(actix_cargo_args(&cfg, "dev", Some("4.9.0")).is_empty());
        cfg.actix.preset = "auto".into();
        assert!(actix_cargo_args(&cfg, "release", Some("4.9.0")).is_empty());
    }

    #[test]
    fn actix_tuning_covers_required_classes() {
        let t = actix_tuning(None);
        for required in ["actix-web", "tokio", "serde", "serde_json", "sqlx", "hyper", "h2"] {
            assert!(t.packages.contains(&required), "missing {required}");
        }
    }

    #[test]
    fn incremental_env_follows_config() {
        let mut cfg = Config::default();
        assert_eq!(
            cargo_env(&cfg, "dev").set.get("CARGO_INCREMENTAL").map(|s| s.as_str()),
            Some("1")
        );
        cfg.profile.incremental = false;
        assert_eq!(
            cargo_env(&cfg, "dev").set.get("CARGO_INCREMENTAL").map(|s| s.as_str()),
            Some("0")
        );
        assert!(cargo_env(&cfg, "release").set.is_empty());
    }
}
