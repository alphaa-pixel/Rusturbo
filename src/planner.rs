//! Build scheduling and dependency-graph optimisation advice.
//!
//! Cargo already schedules the DAG correctly; what it cannot do is tell you *where* the
//! time goes before you have paid for it. [`Schedule`] derives the critical path and the
//! per-depth waves, which is what lets RustTurbo answer "how parallel is this graph
//! really, and what would shorten it?".

use crate::config::Config;
use crate::hardware::Hardware;
use crate::metadata::GraphAnalysis;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Wave {
    pub level: usize,
    pub crates: Vec<String>,
    pub cost: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Schedule {
    /// Levels are longest-path-from-leaf heights: a crate can start once all of its
    /// dependencies at level+1 are done.
    pub waves: Vec<Wave>,
    /// Maximum number of crates Cargo can plausibly have in flight at once.
    pub max_width: usize,
    pub total_cost: u32,
    pub critical_path_cost: u32,
    /// `critical_path_cost / total_cost`: 1.0 means perfectly serial, →0 means very wide.
    pub serial_fraction: f64,
    /// Ideal speed-up ceiling implied by the critical path.
    pub max_parallel_speedup: f64,
    pub recommended_jobs: usize,
    pub why: String,
}

/// Compute a build schedule from the analysed graph.
pub fn schedule(graph: &GraphAnalysis, hw: &Hardware, cfg: &Config) -> Schedule {
    // Height of a node = 1 + max(height of dependents) measured from the bottom of the
    // graph. We compute it over dependency edges: level(crate) = 1 + max(level(dep)).
    let mut levels: BTreeMap<String, usize> = BTreeMap::new();
    let mut order: Vec<&String> = graph.nodes.keys().collect();
    // Process in decreasing depth so dependencies are resolved first.
    order.sort_by_key(|id| std::cmp::Reverse(graph.nodes[*id].depth));
    for id in order {
        // Distance from the workspace root is already the level proxy: a crate at depth
        // d can only start after everything at depth > d. We invert it below so that
        // dependencies compile first.
        levels.insert(id.clone(), graph.nodes[id].depth);
    }

    // Build waves from depth (root = 0) reversed: deepest depth compiles first.
    let max_depth = levels.values().copied().max().unwrap_or(0);
    let mut by_level: BTreeMap<usize, Vec<String>> = BTreeMap::new();
    for (id, d) in &levels {
        let level = max_depth.saturating_sub(*d);
        by_level.entry(level).or_default().push(id.clone());
    }
    let mut waves = Vec::new();
    for (level, mut crates) in by_level {
        crates.sort();
        let cost = crates
            .iter()
            .map(|c| graph.nodes.get(c).map(|n| n.weight).unwrap_or(0))
            .sum();
        waves.push(Wave {
            level,
            crates,
            cost,
        });
    }

    let max_width = waves.iter().map(|w| w.crates.len()).max().unwrap_or(0);
    let total_cost: u32 = graph.nodes.values().map(|n| n.weight).sum();
    let critical_path_cost = graph
        .critical_path
        .iter()
        .map(|id| graph.nodes.get(id).map(|n| n.weight).unwrap_or(0))
        .sum::<u32>();
    let serial_fraction = if total_cost == 0 {
        0.0
    } else {
        critical_path_cost as f64 / total_cost as f64
    };
    let max_parallel_speedup = if critical_path_cost == 0 {
        1.0
    } else {
        total_cost as f64 / critical_path_cost as f64
    };

    let jobs = cfg.effective_jobs(hw.logical_cores, hw.ram_mb);
    let why = if max_width < jobs {
        format!(
            "graph width ({max_width}) is narrower than the {jobs} available jobs — extra cores cannot help; the critical path dominates"
        )
    } else if hw.recommended_jobs < hw.logical_cores {
        format!(
            "{} logical cores but ~{} GiB RAM: capping at {} jobs to avoid LLVM memory pressure",
            hw.logical_cores,
            hw.ram_mb / 1024,
            jobs
        )
    } else {
        format!("{jobs} jobs fit both the core count and the dependency width ({max_width})")
    };

    Schedule {
        waves,
        max_width,
        total_cost,
        critical_path_cost,
        serial_fraction,
        max_parallel_speedup,
        recommended_jobs: jobs,
        why,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Recommendation {
    pub title: String,
    pub detail: String,
    /// `high` | `medium` | `low`
    pub impact: String,
    /// `safe` if applying it cannot change program behaviour, else `review`.
    pub risk: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Advice {
    pub recommendations: Vec<Recommendation>,
    /// Duplicate crate versions force duplicate compilation of the same code.
    pub duplicate_versions: Vec<(String, Vec<String>)>,
    /// Features that pull in a lot of code for little benefit.
    pub broad_features: Vec<(String, String)>,
}

pub fn advise(graph: &GraphAnalysis, schedule: &Schedule) -> Advice {
    let mut recs: Vec<Recommendation> = Vec::new();

    if let Some(first) = graph.critical_path.first() {
        if let Some(n) = graph.nodes.get(first) {
            recs.push(Recommendation {
                title: format!("Critical path starts at {}", n.name),
                detail: format!(
                    "{} crates deep, cost {}; shortening this chain is the only way to reduce wall-clock time below {} s of serial work.",
                    graph.critical_path.len(),
                    schedule.critical_path_cost,
                    schedule.critical_path_cost
                ),
                impact: "high".into(),
                risk: "safe".into(),
            });
        }
    }

    // Duplicate versions: real, measurable waste (same crate compiled twice).
    let mut by_name: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for n in graph.nodes.values() {
        by_name
            .entry(n.name.clone())
            .or_default()
            .push(n.version.clone());
    }
    let mut duplicates = Vec::new();
    for (name, mut versions) in by_name {
        versions.sort();
        versions.dedup();
        if versions.len() > 1 {
            duplicates.push((name.clone(), versions.clone()));
            recs.push(Recommendation {
                title: format!("{name} is compiled {} times", versions.len()),
                detail: format!(
                    "versions {} are both in the graph. `cargo tree -d` shows the parents; usually one dependency needs a newer semver range. Collapsing this removes a full crate compilation.",
                    versions.join(", ")
                ),
                impact: "medium".into(),
                risk: "review".into(),
            });
        }
    }

    // Broad features that drag in optional dependencies.
    let mut broad = Vec::new();
    for n in graph.nodes.values() {
        if n.name == "tokio" && n.features.iter().any(|f| f == "full") {
            broad.push((n.name.clone(), "full".to_string()));
            recs.push(Recommendation {
                title: "tokio is built with the `full` feature".into(),
                detail: "`full` enables net, fs, signal, process, io-util, time, sync, macros and rt-multi-thread. Most servers need `rt-multi-thread`, `macros`, `net`, `time` and `signal`. Narrowing the set removes optional crates from the graph entirely.".into(),
                impact: "medium".into(),
                risk: "review".into(),
            });
        }
        if n.name == "serde" && n.features.iter().any(|f| f == "derive") {
            // Not a problem per se; note the cost so it is not a surprise.
            recs.push(Recommendation {
                title: "serde derives are in play".into(),
                detail: "`serde` + `serde_derive` + `syn` is ~45 cost units of the graph. This is normally worth it; it is listed because it is the first place to look if you are trimming a cold build.".into(),
                impact: "low".into(),
                risk: "safe".into(),
            });
        }
    }

    if schedule.max_width < schedule.recommended_jobs {
        recs.push(Recommendation {
            title: format!(
                "Only {} crates can compile in parallel at the widest point",
                schedule.max_width
            ),
            detail: format!(
                "RustTurbo will still use {} jobs, but the graph will not saturate them. Time is bound by the critical path ({}), not by CPU count.",
                schedule.recommended_jobs, schedule.critical_path_cost
            ),
            impact: "high".into(),
            risk: "safe".into(),
        });
    }

    if !graph.build_script_packages.is_empty() {
        recs.push(Recommendation {
            title: format!(
                "{} crates run build scripts",
                graph.build_script_packages.len()
            ),
            detail: "Build scripts are serialised per-crate and compile a host binary, so they punch above their weight. Artifacts from this graph can embed host state (system library versions). RustTurbo shares them with a warning by default; `cache.build_scripts = \"strict\"` disables sharing.".into(),
            impact: "medium".into(),
            risk: "safe".into(),
        });
    }

    if graph.actix.detected {
        recs.push(Recommendation {
            title: "Actix preset available".into(),
            detail: "This graph contains actix-web. `rusturbo init --preset actix` writes tuned profile settings for the Actix/Tokio/serialization stack, and `rusturbo run` applies them.".into(),
            impact: "low".into(),
            risk: "safe".into(),
        });
    }

    if graph.package_count > 150 {
        recs.push(Recommendation {
            title: format!("Large graph: {} crates", graph.package_count),
            detail: "At this size a shared dependency cache is the dominant lever — a cold build is essentially all dependency compilation. Keep one warm cache per (toolchain, target, profile) triple.".into(),
            impact: "high".into(),
            risk: "safe".into(),
        });
    }

    Advice {
        recommendations: recs,
        duplicate_versions: duplicates,
        broad_features: broad,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::{ActixProfile, NodeInfo};
    use std::collections::BTreeMap;

    fn hw(cores: usize, ram: u64) -> Hardware {
        Hardware {
            physical_cores: cores,
            logical_cores: cores,
            ram_mb: ram,
            os: "linux".into(),
            arch: "x86_64".into(),
            recommended_jobs: cores,
        }
    }

    fn node(name: &str, depth: usize, weight: u32, feats: &[&str]) -> NodeInfo {
        NodeInfo {
            id: format!("{name} 1.0.0 (registry+https://x)"),
            name: name.into(),
            version: "1.0.0".into(),
            fan_in: 1,
            deps: 0,
            depth,
            has_build_script: false,
            is_workspace: depth == 0,
            features: feats.iter().map(|s| s.to_string()).collect(),
            categories: vec![],
            weight,
            path_cost: 0,
        }
    }

    fn graph_of(nodes: Vec<NodeInfo>, path: Vec<String>) -> GraphAnalysis {
        let mut m = BTreeMap::new();
        for n in nodes {
            m.insert(n.id.clone(), n);
        }
        GraphAnalysis {
            workspace_root: "/p".into(),
            package_count: m.len(),
            workspace_packages: vec!["app-1.0.0".into()],
            build_script_packages: vec![],
            nodes: m,
            critical_path: path,
            hot_nodes: vec![],
            actix: ActixProfile::default(),
            target_dir: "/p/target".into(),
        }
    }

    #[test]
    fn waves_start_from_deepest_dependencies() {
        let app = node("app", 0, 3, &[]);
        let mid = node("mid", 1, 10, &[]);
        let leaf = node("leaf", 2, 20, &[]);
        let path = vec![app.id.clone(), mid.id.clone(), leaf.id.clone()];
        let g = graph_of(vec![app, mid, leaf], path);
        let s = schedule(&g, &hw(8, 65536), &Config::default());
        assert_eq!(s.waves.len(), 3);
        assert_eq!(s.waves[0].crates.len(), 1);
        assert!(s.waves[0].crates[0].contains("leaf"), "leaves compile first");
        assert_eq!(s.total_cost, 33);
        assert!(s.max_width >= 1);
        assert!(s.max_parallel_speedup >= 1.0);
    }

    #[test]
    fn jobs_are_capped_by_ram() {
        let app = node("app", 0, 3, &[]);
        let g = graph_of(vec![app], vec![]);
        let cfg = Config::default();
        // 32 cores, 2 GiB RAM → 1 job.
        let s = schedule(&g, &hw(32, 2048), &cfg);
        assert_eq!(s.recommended_jobs, 1);
        assert!(s.why.contains("RAM") || s.why.contains("narrow"));
        // 4 cores, 64 GiB → 4 jobs.
        let s2 = schedule(&g, &hw(4, 65536), &cfg);
        assert_eq!(s2.recommended_jobs, 4);
    }

    #[test]
    fn explicit_jobs_override_hardware() {
        let app = node("app", 0, 3, &[]);
        let g = graph_of(vec![app], vec![]);
        let mut cfg = Config::default();
        cfg.profile.jobs = 2;
        let s = schedule(&g, &hw(16, 65536), &cfg);
        assert_eq!(s.recommended_jobs, 2);
    }

    #[test]
    fn advice_flags_duplicate_versions_and_tokio_full() {
        let mut a = node("serde", 2, 14, &["derive"]);
        a.version = "1.0.0".into();
        let mut b = node("serde", 1, 14, &["derive"]);
        b.version = "1.0.200".into();
        b.id = "serde 1.0.200 (registry+x)".into();
        let tok = node("tokio", 1, 45, &["full", "rt-multi-thread"]);
        let app = node("app", 0, 3, &[]);
        let g = graph_of(vec![a, b, tok, app], vec![]);
        let s = schedule(&g, &hw(8, 65536), &Config::default());
        let adv = advise(&g, &s);
        assert_eq!(adv.duplicate_versions.len(), 1);
        assert_eq!(adv.duplicate_versions[0].0, "serde");
        assert!(adv.recommendations.iter().any(|r| r.title.contains("duplicate") || r.title.contains("compiled 2 times")));
        assert!(adv.recommendations.iter().any(|r| r.title.contains("`full` feature")));
        assert!(adv.broad_features.iter().any(|(n, f)| n == "tokio" && f == "full"));
    }

    #[test]
    fn no_false_positive_duplicates_for_single_versions() {
        let a = node("serde", 1, 14, &[]);
        let app = node("app", 0, 3, &[]);
        let g = graph_of(vec![a, app], vec![]);
        let s = schedule(&g, &hw(4, 65536), &Config::default());
        assert!(advise(&g, &s).duplicate_versions.is_empty());
    }
}
