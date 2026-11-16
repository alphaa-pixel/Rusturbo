//! Glue between the Cargo graph, the cache and the build scheduler.
//!
//! Two reuse strategies, both keyed by the same fingerprint:
//!
//! * **Shared target directory** (default, fastest): projects with an identical
//!   dependency graph + toolchain + profile point `CARGO_TARGET_DIR` at one directory.
//!   Cargo's own fingerprints then declare every dependency fresh and it links only the
//!   local crate. This is the mechanism behind "new project, familiar dependency set,
//!   no recompile".
//! * **Portable entry**: snapshot of a `<profile>` directory that can be materialized
//!   into a project's own `target/`. Used when a project must keep its own target dir.

use crate::cache::{
    self, ArtifactCache, CacheDecision, CacheKey, EntryMeta,
};
use crate::config::Config;
use crate::hardware::Toolchain;
use crate::metadata::GraphAnalysis;
use crate::process::{self, EnvPatch};
use crate::util;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const RUSTURBO_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Everything that can change the bytes of a dependency artifact.
pub fn build_key(
    toolchain: &Toolchain,
    target_triple: &str,
    profile: &str,
    graph_hash: &str,
    extra_rustflags: &[String],
    cfg: &Config,
) -> CacheKey {
    let mut env_pairs: Vec<String> = cache::relevant_env()
        .into_iter()
        .filter(|(k, _)| k != "CARGO_TARGET_DIR") // our own knob, not an input
        .map(|(k, v)| format!("{k}={v}"))
        .collect();
    env_pairs.sort();
    let mut flags = extra_rustflags.to_vec();
    flags.sort();

    CacheKey::from_inputs(vec![
        ("format".into(), "rusturbo-artifact-v1".into()),
        ("rusturbo".into(), RUSTURBO_VERSION.into()),
        // `rustc -vV` verbatim: host, LLVM version, commit hash, panic strategy.
        ("rustc_vv".into(), toolchain.vv_raw.clone()),
        ("cargo".into(), toolchain.cargo_version.clone()),
        ("target".into(), target_triple.to_string()),
        ("profile".into(), profile.to_string()),
        ("graph".into(), graph_hash.to_string()),
        ("dep_debug".into(), cfg.profile.dep_debug.to_string()),
        (
            "parallel_codegen".into(),
            cfg.profile.parallel_codegen.to_string(),
        ),
        ("preset".into(), cfg.profile.preset.clone()),
        ("rustflags".into(), flags.join(" ")),
        ("env".into(), env_pairs.join("\n")),
    ])
}

/// Dependencies as `(name, version, features)` for the graph fingerprint.
pub fn graph_packages(graph: &GraphAnalysis) -> Vec<(String, String, Vec<String>)> {
    graph
        .nodes
        .values()
        .filter(|n| !n.is_workspace)
        .map(|n| (n.name.clone(), n.version.clone(), n.features.clone()))
        .collect()
}

#[derive(Debug, Clone, PartialEq)]
pub enum Reuse {
    /// Point CARGO_TARGET_DIR at an existing shared dir: deps will not compile again.
    SharedTarget { dir: PathBuf },
    /// Materialized a portable entry into the project target dir.
    Materialized { dir: PathBuf, bytes: u64 },
    /// Nothing reusable; dependencies must compile.
    Cold { reason: String },
}

#[derive(Debug, Clone)]
pub struct WarmPlan {
    pub key: CacheKey,
    pub reuse: Reuse,
    pub decision: CacheDecision,
    pub build_scripts: Vec<String>,
    pub shared_target_dir: Option<PathBuf>,
    pub cache_add_ms: Duration,
}

/// Look for a reusable dependency set *before* invoking Cargo.
pub fn plan(
    cache: &ArtifactCache,
    cfg: &Config,
    toolchain: &Toolchain,
    target_triple: &str,
    profile: &str,
    graph: &GraphAnalysis,
    extra_rustflags: &[String],
    project_target: &Path,
) -> WarmPlan {
    let started = std::time::Instant::now();
    let pkgs = graph_packages(graph);
    let graph_hash = cache::graph_fingerprint(&pkgs);
    let key = build_key(
        toolchain,
        target_triple,
        profile,
        &graph_hash,
        extra_rustflags,
        cfg,
    );
    let sys_crates: Vec<String> = graph
        .nodes
        .values()
        .filter(|n| n.name.ends_with("-sys"))
        .map(|n| n.name.clone())
        .collect();
    let decision = cache::policy_check(&graph.build_script_packages, &cfg.cache.build_scripts, &sys_crates);

    let mut reuse = Reuse::Cold {
        reason: "no matching dependency artifacts yet".into(),
    };
    let mut shared_target_dir = None;

    if !cache.enabled() {
        reuse = Reuse::Cold {
            reason: "cache disabled".into(),
        };
    } else if !decision.cacheable {
        reuse = Reuse::Cold {
            reason: decision.reason.clone(),
        };
    } else if cfg.cache.shared_targets {
        let dir = cache.shared_target(&key.short);
        if cache.shared_exists(&key.short) {
            reuse = Reuse::SharedTarget { dir: dir.clone() };
            shared_target_dir = Some(dir);
        }
    }

    if matches!(reuse, Reuse::Cold { .. }) && cache.enabled() && decision.cacheable {
        if cache.contains(&key.short) {
            let profile_dir = project_target.join(profile);
            match cache.materialize(&key.short, &profile_dir) {
                Ok(bytes) => {
                    reuse = Reuse::Materialized {
                        dir: profile_dir,
                        bytes,
                    };
                }
                Err(e) => {
                    reuse = Reuse::Cold {
                        reason: format!("cache entry {0} could not be materialized: {e}", key.short),
                    }
                }
            }
        }
    }

    WarmPlan {
        key,
        reuse,
        decision,
        build_scripts: graph.build_script_packages.clone(),
        shared_target_dir,
        cache_add_ms: started.elapsed(),
    }
}

/// After a successful dependency build, publish the artifacts.
///
/// `source_dir` is the `<target>/<profile>` directory of the build that just ran.
pub fn publish(
    cache: &ArtifactCache,
    cfg: &Config,
    plan: &WarmPlan,
    source_dir: &Path,
    toolchain: &Toolchain,
    target_triple: &str,
    profile: &str,
    graph: &GraphAnalysis,
) -> Result<Option<EntryMeta>> {
    if !cache.enabled() || !plan.decision.cacheable || !source_dir.is_dir() {
        return Ok(None);
    }
    let meta = EntryMeta {
        key: plan.key.short.clone(),
        rusturbo_version: RUSTURBO_VERSION.to_string(),
        created_at: util::iso8601_now(),
        created_unix: util::unix_now(),
        last_used_unix: util::unix_now(),
        toolchain: toolchain.rustc_verbose.clone(),
        target: target_triple.to_string(),
        profile: profile.to_string(),
        crate_count: graph.nodes.len(),
        graph_hash: plan.key.input("graph").unwrap_or_default().to_string(),
        file_count: 0,
        size_bytes: 0,
        has_build_scripts: !plan.build_scripts.is_empty(),
        inputs: plan.key.inputs.clone(),
    };

    if cfg.cache.shared_targets {
        cache.mark_shared_ready(&plan.key.short, meta.clone())?;
    }
    cache.put(&plan.key.short, source_dir, meta.clone())?;
    let _ = cache.evict();
    Ok(Some(meta))
}

/// Apply the reuse decision to a Cargo invocation's environment.
pub fn apply_reuse(env: &mut EnvPatch, plan: &WarmPlan) {
    if let Some(dir) = &plan.shared_target_dir {
        *env = env.clone().set("CARGO_TARGET_DIR", dir.display().to_string());
    }
}

pub fn shared_target_env(plan: &WarmPlan) -> Option<(String, String)> {
    plan.shared_target_dir
        .as_ref()
        .map(|d| ("CARGO_TARGET_DIR".to_string(), d.display().to_string()))
}

/// Where the local crate's binary lands.
pub fn binary_path(target_dir: &Path, profile: &str, bin_name: &str) -> PathBuf {
    let sub = match profile {
        "dev" | "test" => "debug",
        other => other,
    };
    let exe = if cfg!(windows) {
        format!("{bin_name}.exe")
    } else {
        bin_name.to_string()
    };
    target_dir.join(sub).join(exe)
}

/// Resolve the effective target dir for a plan.
pub fn effective_target_dir(plan: &WarmPlan, project_target: &Path) -> PathBuf {
    match &plan.reuse {
        Reuse::SharedTarget { dir } => dir.clone(),
        _ => project_target.to_path_buf(),
    }
}

/// `cargo` path used for all builds. Honours `$CARGO` so cross/custom toolchains work.
pub fn cargo_bin() -> String {
    std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string())
}

/// Run `cargo <args>` from `root` and capture the result.
pub fn run_cargo(
    root: &Path,
    args: &[String],
    env: &EnvPatch,
    stream: bool,
) -> Result<process::Captured> {
    let cargo = cargo_bin();
    if !stream {
        return process::capture(&cargo, args, Some(root), Some(env))
            .with_context(|| format!("failed to run `{} {}`", cargo, args.join(" ")));
    }
    process::capture_streaming(&cargo, args, Some(root), Some(env), |line| {
        // Cargo status lines go to stderr; keep them visible but dimmed so RustTurbo's
        // own summary stands out.
        eprintln!("{}", util::dim(line));
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toolchain() -> Toolchain {
        Toolchain {
            rustc_verbose: "rustc 1.83.0 (90b35a623 2024-11-26)".into(),
            rustc_version: "1.83.0".into(),
            cargo_version: "cargo 1.83.0 (5ffbef321 2024-10-29)".into(),
            host: "x86_64-unknown-linux-gnu".into(),
            channel: "1.83.0".into(),
            commit_hash: "90b35a623".into(),
            vv_raw: "rustc 1.83.0\nhost: x86_64-unknown-linux-gnu\n".into(),
        }
    }

    fn empty_graph() -> GraphAnalysis {
        GraphAnalysis {
            workspace_root: "/tmp/p".into(),
            package_count: 0,
            workspace_packages: vec![],
            build_script_packages: vec![],
            nodes: Default::default(),
            critical_path: vec![],
            hot_nodes: vec![],
            actix: Default::default(),
            target_dir: "/tmp/p/target".into(),
        }
    }

    #[test]
    fn key_is_stable_across_calls() {
        let cfg = Config::default();
        let a = build_key(&toolchain(), "x86_64-unknown-linux-gnu", "dev", "g", &[], &cfg);
        let b = build_key(&toolchain(), "x86_64-unknown-linux-gnu", "dev", "g", &[], &cfg);
        assert_eq!(a.short, b.short);
    }

    #[test]
    fn key_changes_with_toolchain_profile_graph_and_target() {
        let cfg = Config::default();
        let base = build_key(&toolchain(), "x86_64-unknown-linux-gnu", "dev", "g", &[], &cfg);
        let mut tc = toolchain();
        tc.vv_raw.push_str("LLVM 19\n");
        assert_ne!(
            base.short,
            build_key(&tc, "x86_64-unknown-linux-gnu", "dev", "g", &[], &cfg).short
        );
        assert_ne!(
            base.short,
            build_key(&toolchain(), "x86_64-unknown-linux-gnu", "release", "g", &[], &cfg).short
        );
        assert_ne!(
            base.short,
            build_key(&toolchain(), "x86_64-unknown-linux-gnu", "dev", "h", &[], &cfg).short
        );
        assert_ne!(
            base.short,
            build_key(&toolchain(), "aarch64-unknown-linux-gnu", "dev", "g", &[], &cfg).short
        );
    }

    #[test]
    fn target_dir_knob_is_not_an_input() {
        // Two runs with different CARGO_TARGET_DIR must produce the same key, otherwise
        // the shared-target strategy could never hit.
        let cfg = Config::default();
        std::env::set_var("CARGO_TARGET_DIR", "/tmp/one");
        let a = build_key(&toolchain(), "x86_64-unknown-linux-gnu", "dev", "g", &[], &cfg);
        std::env::set_var("CARGO_TARGET_DIR", "/tmp/two");
        let b = build_key(&toolchain(), "x86_64-unknown-linux-gnu", "dev", "g", &[], &cfg);
        std::env::remove_var("CARGO_TARGET_DIR");
        assert_eq!(a.short, b.short);
    }

    #[test]
    fn cold_when_cache_disabled() {
        let dir = std::env::temp_dir().join(format!("rts-cold-{}", std::process::id()));
        let cache = ArtifactCache::new(dir, 20, false);
        let cfg = Config::default();
        let p = plan(
            &cache,
            &cfg,
            &toolchain(),
            "x86_64-unknown-linux-gnu",
            "dev",
            &empty_graph(),
            &[],
            Path::new("/tmp/p/target"),
        );
        assert!(matches!(p.reuse, Reuse::Cold { .. }));
    }

    #[test]
    fn shared_target_hit_after_ready_marker() {
        let dir = std::env::temp_dir().join(format!("rts-shared-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cache = ArtifactCache::new(dir.clone(), 20, true);
        let cfg = Config::default();
        let p = plan(
            &cache,
            &cfg,
            &toolchain(),
            "x86_64-unknown-linux-gnu",
            "dev",
            &empty_graph(),
            &[],
            Path::new("/tmp/p/target"),
        );
        assert!(matches!(p.reuse, Reuse::Cold { .. }), "nothing cached yet");
        let meta = EntryMeta {
            key: p.key.short.clone(),
            rusturbo_version: RUSTURBO_VERSION.into(),
            created_at: util::iso8601_now(),
            created_unix: util::unix_now(),
            last_used_unix: util::unix_now(),
            toolchain: "t".into(),
            target: "t".into(),
            profile: "dev".into(),
            crate_count: 0,
            graph_hash: "g".into(),
            file_count: 0,
            size_bytes: 0,
            has_build_scripts: false,
            inputs: vec![],
        };
        cache.mark_shared_ready(&p.key.short, meta).unwrap();
        let p2 = plan(
            &cache,
            &cfg,
            &toolchain(),
            "x86_64-unknown-linux-gnu",
            "dev",
            &empty_graph(),
            &[],
            Path::new("/tmp/p/target"),
        );
        assert_eq!(
            p2.reuse,
            Reuse::SharedTarget {
                dir: cache.shared_target(&p.key.short)
            }
        );
        let mut env = EnvPatch::new();
        apply_reuse(&mut env, &p2);
        assert!(env.set.contains_key("CARGO_TARGET_DIR"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn binary_path_maps_dev_to_debug_dir() {
        // Compare components so the test holds on Windows (where the separator and the
        // `.exe` suffix differ).
        let p = binary_path(Path::new("/t/target"), "dev", "actix-hello");
        assert_eq!(p.parent().unwrap().file_name().unwrap(), "debug");
        assert!(p.file_name().unwrap().to_string_lossy().starts_with("actix-hello"));
        let r = binary_path(Path::new("/t/target"), "release", "app");
        assert_eq!(r.parent().unwrap().file_name().unwrap(), "release");
        assert!(r.file_name().unwrap().to_string_lossy().starts_with("app"));
    }

    #[test]
    fn effective_target_dir_prefers_shared() {
        let plan = WarmPlan {
            key: CacheKey::from_inputs(vec![("a".into(), "b".into())]),
            reuse: Reuse::SharedTarget {
                dir: PathBuf::from("/shared"),
            },
            decision: CacheDecision {
                cacheable: true,
                reason: String::new(),
                warnings: vec![],
            },
            build_scripts: vec![],
            shared_target_dir: Some(PathBuf::from("/shared")),
            cache_add_ms: Duration::from_millis(0),
        };
        assert_eq!(
            effective_target_dir(&plan, Path::new("/proj/target")),
            PathBuf::from("/shared")
        );
        let cold = WarmPlan {
            shared_target_dir: None,
            reuse: Reuse::Cold {
                reason: "x".into(),
            },
            ..plan.clone()
        };
        assert_eq!(
            effective_target_dir(&cold, Path::new("/proj/target")),
            PathBuf::from("/proj/target")
        );
    }
}
