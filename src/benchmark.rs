//! Benchmark harness: real compilation measurements, no simulated numbers.
//!
//! Scenarios (each measured on the same machine, same dependency graph):
//!
//! | id                     | target dir | cargo registry | rusturbo cache |
//! |------------------------|-----------|----------------|----------------|
//! | `cargo-clean`          | empty     | empty (downloads from crates.io) | n/a |
//! | `cargo-warm-registry`  | empty     | warm           | n/a            |
//! | `rusturbo-clean`       | empty     | warm           | empty          |
//! | `rusturbo-warm`        | warm      | warm           | populated      |
//! | `rusturbo-shared`      | empty     | warm           | populated (second project, same graph) |
//!
//! `cargo-clean` is the only scenario that measures downloads; it is reported
//! separately because it is dominated by network latency, not by rustc.

use crate::process;
use crate::util;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Scenario {
    CargoClean,
    CargoWarmRegistry,
    RustTurboClean,
    RustTurboWarm,
    RustTurboShared,
}

impl Scenario {
    pub fn id(&self) -> &'static str {
        match self {
            Scenario::CargoClean => "cargo-clean",
            Scenario::CargoWarmRegistry => "cargo-warm-registry",
            Scenario::RustTurboClean => "rusturbo-clean",
            Scenario::RustTurboWarm => "rusturbo-warm",
            Scenario::RustTurboShared => "rusturbo-shared",
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Scenario::CargoClean => "cargo run — clean project, empty registry",
            Scenario::CargoWarmRegistry => "cargo run — clean target, warm registry",
            Scenario::RustTurboClean => "rusturbo run — clean project, cold turbo cache",
            Scenario::RustTurboWarm => "rusturbo run — subsequent execution",
            Scenario::RustTurboShared => "rusturbo run — new project, shared deps cache",
        }
    }

    pub fn all() -> [Scenario; 5] {
        [
            Scenario::CargoClean,
            Scenario::CargoWarmRegistry,
            Scenario::RustTurboClean,
            Scenario::RustTurboWarm,
            Scenario::RustTurboShared,
        ]
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sample {
    pub scenario: String,
    pub label: String,
    /// Wall time until the executable existed (cargo/rusturbo returned success).
    pub build_secs: f64,
    /// Wall time from spawn until the process printed `READY` (None if it never did).
    pub startup_secs: Option<f64>,
    /// build + startup: "time to application serving".
    pub total_secs: f64,
    pub peak_rss_mb: u64,
    /// Sum of user+sys CPU seconds across the process tree.
    pub cpu_secs: f64,
    pub downloaded_bytes: u64,
    pub cache_hit: bool,
    pub crate_count: usize,
    pub comment: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchReport {
    pub generated_at: String,
    pub rusturbo_version: String,
    pub toolchain: String,
    pub target: String,
    pub profile: String,
    pub hardware: crate::hardware::Hardware,
    pub linker: String,
    pub project: String,
    pub crate_count: usize,
    pub samples: Vec<Sample>,
}

impl BenchReport {
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_default()
    }

    pub fn to_markdown(&self) -> String {
        let mut s = String::new();
        s.push_str("# RustTurbo benchmark report\n\n");
        s.push_str(&format!("| field | value |\n|---|---|\n"));
        s.push_str(&format!("| generated | {} |\n", self.generated_at));
        s.push_str(&format!("| rusturbo | {} |\n", self.rusturbo_version));
        s.push_str(&format!("| toolchain | {} |\n", self.toolchain));
        s.push_str(&format!("| target | {} |\n", self.target));
        s.push_str(&format!("| profile | {} |\n", self.profile));
        s.push_str(&format!("| linker | {} |\n", self.linker));
        s.push_str(&format!("| project | {} |\n", self.project));
        s.push_str(&format!(
            "| hardware | {} physical / {} logical cores, {} MiB RAM |\n",
            self.hardware.physical_cores, self.hardware.logical_cores, self.hardware.ram_mb
        ));
        s.push_str(&format!("| crates in graph | {} |\n\n", self.crate_count));

        s.push_str("## Results\n\n");
        s.push_str("| scenario | build (s) | startup (s) | total (s) | peak RSS (MiB) | CPU (s) | downloaded | cache hit |\n");
        s.push_str("|---|---|---|---|---|---|---|---|\n");
        for x in &self.samples {
            s.push_str(&format!(
                "| {} | {:.2} | {} | {:.2} | {} | {:.1} | {} | {} |\n",
                x.scenario,
                x.build_secs,
                x.startup_secs
                    .map(|v| format!("{v:.2}"))
                    .unwrap_or_else(|| "—".into()),
                x.total_secs,
                x.peak_rss_mb,
                x.cpu_secs,
                util::human_bytes(x.downloaded_bytes),
                if x.cache_hit { "yes" } else { "no" }
            ));
        }

        s.push_str("\n## Notes\n\n");
        for x in &self.samples {
            s.push_str(&format!("- **{}** — {}\n", x.scenario, x.comment));
        }
        s.push_str("\nPeak RSS and CPU seconds are measured by sampling the whole process tree every 100 ms from `/proc`.\n");
        s
    }

    /// Speed-up of `scenario` relative to `baseline` (build time only).
    pub fn speedup(&self, scenario: &str, baseline: &str) -> Option<f64> {
        let s = self.samples.iter().find(|x| x.scenario == scenario)?;
        let b = self.samples.iter().find(|x| x.scenario == baseline)?;
        if s.build_secs <= 0.0 {
            return None;
        }
        Some(b.build_secs / s.build_secs)
    }
}

/// Samples the process tree for peak RSS and CPU time.
struct TreeSampler {
    root: u32,
    stop: Arc<AtomicBool>,
    peak_rss_kb: Arc<AtomicU64>,
    cpu_ticks: Arc<AtomicU64>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl TreeSampler {
    fn start(root: u32) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let peak = Arc::new(AtomicU64::new(0));
        let cpu = Arc::new(AtomicU64::new(0));
        let (s2, p2, c2) = (stop.clone(), peak.clone(), cpu.clone());
        let handle = std::thread::spawn(move || {
            while !s2.load(Ordering::Relaxed) {
                if let Some((rss, ticks)) = tree_usage(root) {
                    p2.fetch_max(rss, Ordering::Relaxed);
                    c2.fetch_max(ticks, Ordering::Relaxed);
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        });
        Self {
            root,
            stop,
            peak_rss_kb: peak,
            cpu_ticks: cpu,
            handle: Some(handle),
        }
    }

    fn finish(mut self) -> (u64, f64) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        let _ = self.root;
        let rss_mb = self.peak_rss_kb.load(Ordering::Relaxed) / 1024;
        // CLK_TCK is 100 on Linux by default.
        let cpu_secs = self.cpu_ticks.load(Ordering::Relaxed) as f64 / 100.0;
        (rss_mb, cpu_secs)
    }
}

/// Sum RSS and CPU ticks over the process subtree rooted at `root_pid`.
fn tree_usage(root_pid: u32) -> Option<(u64, u64)> {
    let mut pids = vec![root_pid];
    let mut i = 0;
    // Read all (pid, ppid) pairs once, then walk the tree transitively.
    let mut parents: Vec<(u32, u32)> = Vec::new();
    let rd = std::fs::read_dir("/proc").ok()?;
    for e in rd.flatten() {
        let name = e.file_name();
        let Some(pid) = name.to_str().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        if let Ok(stat) = std::fs::read_to_string(e.path().join("stat")) {
            // Format: pid (comm) state ppid ...   — comm may contain spaces/parens.
            if let Some(close) = stat.rfind(')') {
                let rest: Vec<&str> = stat[close + 1..].split_whitespace().collect();
                if rest.len() > 1 {
                    if let Ok(ppid) = rest[1].parse::<u32>() {
                        parents.push((pid, ppid));
                    }
                }
            }
        }
    }
    while i < pids.len() {
        let cur = pids[i];
        for (pid, ppid) in &parents {
            if *ppid == cur && !pids.contains(pid) {
                pids.push(*pid);
            }
        }
        i += 1;
    }
    let mut rss_total = 0u64;
    let mut cpu_total = 0u64;
    for pid in &pids {
        if let Ok(status) = std::fs::read_to_string(format!("/proc/{pid}/status")) {
            for line in status.lines() {
                if let Some(v) = line.strip_prefix("VmRSS:") {
                    if let Some(kb) = v.trim().trim_end_matches("kB").trim().parse::<u64>().ok() {
                        rss_total += kb;
                    }
                }
            }
        }
        if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            if let Some(close) = stat.rfind(')') {
                let rest: Vec<&str> = stat[close + 1..].split_whitespace().collect();
                // utime is field 14 overall → index 11 after comm/state/ppid...
                if rest.len() > 12 {
                    if let (Ok(u), Ok(s)) = (rest[11].parse::<u64>(), rest[12].parse::<u64>()) {
                        cpu_total += u + s;
                    }
                }
            }
        }
    }
    Some((rss_total, cpu_total))
}

/// Run one benchmark scenario.
///
/// `prep` is responsible for arranging the state (cleaning dirs, warming caches) and
/// returns the environment patch + expected target dir + whether a cache hit is
/// expected.
pub struct ScenarioPlan {
    pub env: process::EnvPatch,
    pub cache_hit: bool,
    pub comment: String,
    pub watch_dirs: Vec<PathBuf>,
}

pub fn measure(
    scenario: Scenario,
    root: &Path,
    command: &str,
    args: &[String],
    plan: ScenarioPlan,
    startup_args: &[String],
    binary: &Path,
    crate_count: usize,
) -> Result<Sample> {
    let downloaded_before = total_size(&plan.watch_dirs);
    let start = Instant::now();
    let mut cmd = Command::new(command);
    cmd.args(args)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    plan.env.apply(&mut cmd);
    let child = cmd
        .spawn()
        .with_context(|| format!("failed to spawn `{command}`"))?;
    let pid = child.id();
    let sampler = TreeSampler::start(pid);
    let out = child.wait_with_output()?;
    let build_secs = start.elapsed().as_secs_f64();
    let (peak_rss_mb, cpu_secs) = sampler.finish();
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!(
            "{} failed (exit {:?})\n{}",
            scenario.id(),
            out.status.code(),
            util::truncate_middle(stderr.trim(), 2000)
        );
    }
    let downloaded_after = total_size(&plan.watch_dirs);

    // Startup measurement: spawn the built binary and wait for `READY`.
    let (startup_secs, startup_note) = measure_startup(binary, root, startup_args, &plan.env)?;

    let total_secs = build_secs + startup_secs.unwrap_or(0.0);
    let mut comment = plan.comment;
    if let Some(n) = startup_note {
        comment.push(' ');
        comment.push_str(&n);
    }
    Ok(Sample {
        scenario: scenario.id().to_string(),
        label: scenario.label().to_string(),
        build_secs,
        startup_secs,
        total_secs,
        peak_rss_mb,
        cpu_secs,
        downloaded_bytes: downloaded_after.saturating_sub(downloaded_before),
        cache_hit: plan.cache_hit,
        crate_count,
        comment,
    })
}

fn measure_startup(
    binary: &Path,
    cwd: &Path,
    args: &[String],
    env: &process::EnvPatch,
) -> Result<(Option<f64>, Option<String>)> {
    if !binary.is_file() {
        return Ok((
            None,
            Some(format!("(binary {} not found; startup not measured)", binary.display())),
        ));
    }
    let start = Instant::now();
    let mut cmd = Command::new(binary);
    cmd.args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    env.apply(&mut cmd);
    let mut child = cmd.spawn()?;
    let stdout = child.stdout.take();
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut elapsed = None;
    if let Some(stdout) = stdout {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        loop {
            line.clear();
            if Instant::now() > deadline {
                break;
            }
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    if line.contains("READY") {
                        elapsed = Some(start.elapsed().as_secs_f64());
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    match elapsed {
        Some(e) => Ok((Some(e), None)),
        None => Ok((
            None,
            Some("(app never printed READY; startup not measured)".into()),
        )),
    }
}

fn total_size(dirs: &[PathBuf]) -> u64 {
    dirs.iter().map(|d| util::dir_size(d)).sum()
}

/// Convenience: build the sample list into a report shell.
pub fn report_shell(
    toolchain: &crate::hardware::Toolchain,
    hw: &crate::hardware::Hardware,
    linker: &str,
    profile: &str,
    project: &Path,
    crate_count: usize,
    samples: Vec<Sample>,
) -> BenchReport {
    BenchReport {
        generated_at: util::iso8601_now(),
        rusturbo_version: env!("CARGO_PKG_VERSION").to_string(),
        toolchain: toolchain.rustc_verbose.clone(),
        target: toolchain.host.clone(),
        profile: profile.to_string(),
        hardware: hw.clone(),
        linker: linker.to_string(),
        project: project.display().to_string(),
        crate_count,
        samples,
    }
}

/// `cargo clean`-equivalent for a project, optionally isolating CARGO_HOME.
pub fn clean_project(root: &Path, target_dir: &Path, extra_env: &process::EnvPatch) -> Result<()> {
    util::remove_dir_all_if_exists(target_dir)?;
    let cargo = crate::cacheops::cargo_bin();
    let args = vec!["clean".to_string()];
    let out = process::capture(&cargo, &args, Some(root), Some(extra_env))?;
    if !out.success() {
        // `cargo clean` may fail if the profile dir is already gone; that is fine.
        return Ok(());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scenario_ids_are_stable() {
        assert_eq!(Scenario::CargoClean.id(), "cargo-clean");
        assert_eq!(Scenario::RustTurboWarm.id(), "rusturbo-warm");
        assert_eq!(Scenario::all().len(), 5);
    }

    #[test]
    fn speedup_is_ratio_of_build_times() {
        let mk = |id: &str, build: f64| Sample {
            scenario: id.into(),
            label: String::new(),
            build_secs: build,
            startup_secs: Some(0.1),
            total_secs: build + 0.1,
            peak_rss_mb: 100,
            cpu_secs: 10.0,
            downloaded_bytes: 0,
            cache_hit: false,
            crate_count: 10,
            comment: String::new(),
        };
        let r = BenchReport {
            generated_at: "now".into(),
            rusturbo_version: "0.1.0".into(),
            toolchain: "t".into(),
            target: "x".into(),
            profile: "dev".into(),
            hardware: crate::hardware::Hardware {
                physical_cores: 1,
                logical_cores: 1,
                ram_mb: 1024,
                os: "linux".into(),
                arch: "x86_64".into(),
                recommended_jobs: 1,
            },
            linker: "lld".into(),
            project: "/p".into(),
            crate_count: 10,
            samples: vec![mk("cargo-clean", 100.0), mk("rusturbo-shared", 4.0)],
        };
        let s = r.speedup("rusturbo-shared", "cargo-clean").unwrap();
        assert!((s - 25.0).abs() < 1e-9);
        assert!(r.speedup("nope", "cargo-clean").is_none());
        let md = r.to_markdown();
        assert!(md.contains("rusturbo-shared"));
        assert!(md.contains("100.00"));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn tree_usage_reads_self() {
        let pid = std::process::id();
        let (rss, cpu) = tree_usage(pid).expect("proc readable");
        assert!(rss > 0, "own RSS should be non-zero");
        let _ = cpu;
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn sampler_records_peak_for_child() {
        let mut child = Command::new("sh")
            .arg("-c")
            .arg("dd if=/dev/zero of=/dev/null bs=1M count=50 2>/dev/null; sleep 0.3")
            .spawn()
            .unwrap();
        let sampler = TreeSampler::start(child.id());
        let _ = child.wait();
        let (rss, _cpu) = sampler.finish();
        assert!(rss >= 1, "child RSS should be observable, got {rss}");
    }
}
