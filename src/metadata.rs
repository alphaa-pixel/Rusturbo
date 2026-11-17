//! Cargo graph analysis.
//!
//! We do not re-implement Cargo's resolver. We call `cargo metadata`, which returns the
//! exact graph Cargo itself will build, and then reason about *build cost* on top of it:
//! critical path, fan-in, build scripts, and Actix-specific dependency classes.

use anyhow::{Context, Result};
use cargo_metadata::{DependencyKind, Metadata, MetadataCommand, PackageId};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NodeInfo {
    pub id: String,
    pub name: String,
    pub version: String,
    /// Crates depending on this one (within the build graph).
    pub fan_in: usize,
    /// Direct dependencies (normal + build).
    pub deps: usize,
    /// Distance from the nearest workspace root crate (roots are depth 0).
    pub depth: usize,
    pub has_build_script: bool,
    pub is_workspace: bool,
    pub features: Vec<String>,
    pub categories: Vec<String>,
    /// Heuristic compile-cost score (see [`cost_weight`]).
    pub weight: u32,
    /// Cumulative cost of the heaviest path from a root to this node.
    pub path_cost: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GraphAnalysis {
    pub workspace_root: String,
    pub package_count: usize,
    pub workspace_packages: Vec<String>,
    pub build_script_packages: Vec<String>,
    pub nodes: BTreeMap<String, NodeInfo>,
    /// Root-to-leaf chain with the largest cumulative cost, most expensive last.
    pub critical_path: Vec<String>,
    /// Nodes sorted by cumulative path cost (the schedule-critical crates).
    pub hot_nodes: Vec<String>,
    pub actix: ActixProfile,
    pub target_dir: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ActixProfile {
    pub detected: bool,
    pub version: Option<String>,
    pub runtime: Vec<String>,
    pub serialization: Vec<String>,
    pub database: Vec<String>,
    pub http: Vec<String>,
    pub observed_features: BTreeMap<String, Vec<String>>,
}

pub struct Analyzer {
    pub metadata: Metadata,
}

impl Analyzer {
    pub fn load(manifest_dir: &std::path::Path) -> Result<Self> {
        let mut cmd = MetadataCommand::new();
        cmd.manifest_path(manifest_dir.join("Cargo.toml"));
        // `cargo metadata` needs the registry; inherit user config so mirrors work.
        cmd.no_deps();
        let shallow = cmd
            .exec()
            .with_context(|| {
                format!(
                    "`cargo metadata` failed in {} — is this a Cargo project, and is the registry reachable?",
                    manifest_dir.display()
                )
            })?;
        // Re-run with dependencies (no_deps above keeps the first call cheap and gives a
        // better error message when the manifest itself is broken).
        let mut full_cmd = MetadataCommand::new();
        full_cmd.manifest_path(manifest_dir.join("Cargo.toml"));
        let full = full_cmd
            .exec()
            .context("`cargo metadata` (with dependencies) failed")?;
        let _ = shallow;
        Ok(Self { metadata: full })
    }

    /// Build the analysis graph. `include_dev` controls whether the workspace's dev
    /// dependencies are part of the graph: `cargo run` does not build them, `cargo test`
    /// does.
    pub fn analyze(&self, include_dev: bool) -> Result<GraphAnalysis> {
        let md = &self.metadata;
        let resolve = md
            .resolve
            .as_ref()
            .context("cargo metadata returned no dependency graph (missing `resolve` section)")?;

        let by_id: BTreeMap<&PackageId, &cargo_metadata::Package> =
            md.packages.iter().map(|p| (&p.id, p)).collect();
        let workspace: BTreeSet<&PackageId> = md.workspace_members.iter().collect();

        // Edge list over normal + build dependencies (dev deps only when requested and
        // only from workspace members, matching Cargo's behaviour).
        let mut edges: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut reverse: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let mut build_deps: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();

        for node in &resolve.nodes {
            let from = node.id.repr.clone();
            let from_is_ws = workspace.contains(&node.id);
            for dep in &node.deps {
                let mut keep = false;
                let mut is_build = false;
                for dk in &dep.dep_kinds {
                    match dk.kind {
                        DependencyKind::Normal => keep = true,
                        DependencyKind::Build => {
                            keep = true;
                            is_build = true;
                        }
                        DependencyKind::Development => {
                            if include_dev && from_is_ws {
                                keep = true;
                            }
                        }
                        _ => {}
                    }
                }
                if !keep {
                    continue;
                }
                let to = dep.pkg.repr.clone();
                edges.entry(from.clone()).or_default().push(to.clone());
                reverse.entry(to.clone()).or_default().insert(from.clone());
                if is_build {
                    build_deps.entry(from.clone()).or_default().insert(to);
                }
            }
        }

        // Only nodes reachable from workspace members are part of a real build.
        let roots: Vec<String> = md
            .workspace_members
            .iter()
            .map(|p| p.repr.clone())
            .filter(|id| by_id.contains_key(&PackageId { repr: id.clone() }))
            .collect();
        let mut reachable: BTreeSet<String> = BTreeSet::new();
        let mut depth: BTreeMap<String, usize> = BTreeMap::new();
        let mut queue: VecDeque<(String, usize)> = VecDeque::new();
        for r in &roots {
            reachable.insert(r.clone());
            depth.insert(r.clone(), 0);
            queue.push_back((r.clone(), 0));
        }
        while let Some((id, d)) = queue.pop_front() {
            for child in edges.get(&id).into_iter().flatten() {
                if reachable.insert(child.clone()) {
                    queue.push_back((child.clone(), d + 1));
                }
                let e = depth.entry(child.clone()).or_insert(d + 1);
                if *e > d + 1 {
                    *e = d + 1;
                }
            }
        }

        // Cost model: per-crate base weight tuned by class of crate.
        let mut nodes: BTreeMap<String, NodeInfo> = BTreeMap::new();
        for id in &reachable {
            let pid = PackageId { repr: id.clone() };
            let Some(pkg) = by_id.get(&pid) else { continue };
            let node = resolve.nodes.iter().find(|n| &n.id == &pid);
            let features = node.map(|n| n.features.clone()).unwrap_or_default();
            let has_build_script = pkg
                .targets
                .iter()
                .any(|t| t.kind.iter().any(|k| k.to_string() == "custom-build"));
            let name = pkg.name.to_string();
            nodes.insert(
                id.clone(),
                NodeInfo {
                    id: id.clone(),
                    weight: cost_weight(&name, has_build_script),
                    name,
                    version: pkg.version.to_string(),
                    fan_in: reverse.get(id).map(|s| s.len()).unwrap_or(0),
                    deps: edges.get(id).map(|v| v.len()).unwrap_or(0),
                    depth: *depth.get(id).unwrap_or(&0),
                    has_build_script,
                    is_workspace: workspace.contains(&pid),
                    categories: categorize(&pkg.name.to_string()),
                    features: features.clone(),
                    path_cost: 0,
                },
            );
        }

        // Cumulative path cost (heaviest dependency chain), computed transitively over
        // the DAG in depth order. Cargo's actual parallelism is bounded by this chain.
        let mut order: Vec<String> = nodes.keys().cloned().collect();
        order.sort_by_key(|id| std::cmp::Reverse(nodes[id].depth));
        for id in order {
            let w = nodes[&id].weight;
            let best_child = edges
                .get(&id)
                .into_iter()
                .flatten()
                .filter_map(|c| nodes.get(c).map(|n| n.path_cost))
                .max()
                .unwrap_or(0);
            if let Some(n) = nodes.get_mut(&id) {
                n.path_cost = w + best_child;
            }
        }

        // Critical path: walk from the heaviest root downwards always choosing the
        // heaviest child. This is the chain that bounds build time.
        let mut critical_path = Vec::new();
        if let Some(start) = roots.iter().max_by_key(|r| nodes.get(*r).map(|n| n.path_cost).unwrap_or(0)) {
            let mut cur = start.clone();
            let mut seen = BTreeSet::new();
            loop {
                if !seen.insert(cur.clone()) {
                    break;
                }
                critical_path.push(cur.clone());
                let next = edges
                    .get(&cur)
                    .into_iter()
                    .flatten()
                    .filter(|c| nodes.contains_key(*c))
                    .max_by_key(|c| nodes[*c].path_cost)
                    .cloned();
                match next {
                    Some(n) => cur = n,
                    None => break,
                }
            }
        }

        let mut hot: Vec<String> = nodes.keys().cloned().collect();
        hot.sort_by_key(|id| std::cmp::Reverse(nodes[id].path_cost));
        hot.truncate(15);

        let build_script_packages = nodes
            .values()
            .filter(|n| n.has_build_script)
            .map(|n| format!("{}-{}", n.name, n.version))
            .collect();

        let actix = actix_profile(&nodes);

        Ok(GraphAnalysis {
            workspace_root: md.workspace_root.to_string(),
            package_count: nodes.len(),
            workspace_packages: roots
                .iter()
                .filter_map(|id| nodes.get(id).map(|n| format!("{}-{}", n.name, n.version)))
                .collect(),
            build_script_packages,
            hot_nodes: hot,
            nodes,
            critical_path,
            actix,
            target_dir: md.target_directory.to_string(),
        })
    }
}

/// Heuristic compile-cost weight. These constants come from published build-time
/// profiles (e.g. `cargo build --timings` reports) and are deliberately conservative:
/// the report labels them as heuristics, and the benchmark harness measures the truth.
pub fn cost_weight(name: &str, has_build_script: bool) -> u32 {
    let base = match name {
        // Heavy proc-macro / codegen crates.
        "syn" | "regex" | "regex-syntax" | "regex-automata" => 30,
        "serde_derive" | "tokio-macros" | "actix-macros" | "actix-web-codegen" => 12,
        // Large generic-heavy libraries: monomorphisation cost dominates.
        "tokio" => 45,
        "hyper" => 22,
        "h2" => 18,
        "sqlx" | "sqlx-core" => 55,
        "sqlx-postgres" | "sqlx-mysql" | "sqlx-sqlite" => 30,
        "diesel" => 40,
        "aws-sdk-s3" | "aws-config" | "aws-smithy-runtime" => 45,
        "ring" => 35,
        "openssl-sys" | "libsqlite3-sys" | "libz-sys" | "zstd-sys" | "bzip2-sys" => 25,
        "rustls" => 20,
        "arrow" => 60,
        "actix-web" | "actix-http" => 35,
        "actix-router" | "actix-service" | "actix-utils" | "actix-codec" => 6,
        "serde" => 14,
        "serde_json" => 12,
        "futures-util" | "futures-core" | "futures-sink" => 6,
        "tracing" | "tracing-core" | "tracing-subscriber" => 8,
        "proc-macro2" | "quote" => 6,
        "time" | "chrono" => 10,
        "flate2" | "brotli" | "miniz_oxide" => 8,
        "http" | "bytes" | "http-body" => 4,
        "mio" | "socket2" | "parking_lot" | "parking_lot_core" => 6,
        "url" | "idna" => 6,
        "unicode-normalization" | "unicode-bidi" => 8,
        _ => {
            if name.ends_with("-derive") || name.ends_with("_derive") {
                10
            } else if name.ends_with("-sys") {
                8
            } else {
                3
            }
        }
    };
    // Build scripts add a full rustc invocation plus a host binary link.
    base + if has_build_script { 4 } else { 0 }
}

pub fn categorize(name: &str) -> Vec<String> {
    let mut out = Vec::new();
    if name.starts_with("actix") {
        out.push("actix".into());
    }
    if name == "tokio" || name.starts_with("tokio-") || name.starts_with("futures") {
        out.push("async-runtime".into());
    }
    if name.starts_with("serde") || name.starts_with("toml") || name == "json" {
        out.push("serialization".into());
    }
    if matches!(
        name,
        "sqlx" | "sqlx-core" | "diesel" | "rusqlite" | "mongodb" | "redis" | "bb8" | "deadpool"
    ) || name.starts_with("sqlx-")
        || name.starts_with("diesel-")
        || name.starts_with("tokio-postgres")
        || name.starts_with("postgres")
    {
        out.push("database".into());
    }
    if matches!(name, "hyper" | "h2" | "http" | "http-body" | "hyper-util" | "tower" | "tower-http" | "reqwest" | "awc")
    {
        out.push("http".into());
    }
    if name == "ring" || name.starts_with("rustls") || name.starts_with("openssl") {
        out.push("crypto".into());
    }
    out
}

fn actix_profile(nodes: &BTreeMap<String, NodeInfo>) -> ActixProfile {
    let find = |pred: &dyn Fn(&str) -> bool| -> Vec<String> {
        let mut v: Vec<String> = nodes
            .values()
            .filter(|n| pred(&n.name))
            .map(|n| format!("{}-{}", n.name, n.version))
            .collect();
        v.sort();
        v
    };
    let actix_web = nodes.values().find(|n| n.name == "actix-web");
    let detected = actix_web.is_some();
    let mut observed = BTreeMap::new();
    for n in nodes.values() {
        if !["actix-web", "tokio", "serde", "sqlx", "reqwest"].contains(&n.name.as_str()) {
            continue;
        }
        let mut f = n.features.clone();
        f.sort();
        observed.insert(format!("{}-{}", n.name, n.version), f);
    }
    ActixProfile {
        detected,
        version: actix_web.map(|n| n.version.clone()),
        runtime: find(&|n| n == "tokio" || n.starts_with("tokio-")),
        serialization: find(&|n| n.starts_with("serde")),
        database: find(&|n| {
            ["sqlx", "diesel", "rusqlite", "mongodb", "redis", "deadpool", "bb8"]
                .iter()
                .any(|p| n.starts_with(p))
        }),
        http: find(&|n| {
            ["hyper", "h2", "http", "reqwest", "awc", "tower"]
                .iter()
                .any(|p| n == *p || n.starts_with(&format!("{p}-")))
        }),
        observed_features: observed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weights_rank_heavy_crates_above_trivial_ones() {
        let tokio = cost_weight("tokio", false);
        let bytes = cost_weight("bytes", false);
        let unknown = cost_weight("some-random-crate", false);
        assert!(tokio > bytes);
        assert!(bytes > unknown);
        assert!(cost_weight("foo", true) > cost_weight("foo", false));
    }

    #[test]
    fn categorization_covers_actix_stack() {
        assert!(categorize("actix-web").contains(&"actix".to_string()));
        assert!(categorize("tokio").contains(&"async-runtime".to_string()));
        assert!(categorize("serde_json").contains(&"serialization".to_string()));
        assert!(categorize("sqlx-postgres").contains(&"database".to_string()));
        assert!(categorize("hyper").contains(&"http".to_string()));
        assert!(categorize("rand").is_empty());
    }

    /// Integration-ish test: analyze this very crate. Requires `cargo metadata` to work,
    /// which is true on any dev machine and in CI.
    #[test]
    fn analyzes_own_workspace() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let a = Analyzer::load(root).expect("metadata");
        let g = a.analyze(true).expect("analyze");
        assert!(g.package_count > 5, "expected dependencies, got {}", g.package_count);
        assert!(!g.critical_path.is_empty());
        assert!(g.critical_path.len() <= g.package_count);
        // Every node on the critical path must exist in the graph.
        for id in &g.critical_path {
            assert!(g.nodes.contains_key(id));
        }
    }
}
