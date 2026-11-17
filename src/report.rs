//! Terminal and Markdown rendering of analysis results.

use crate::hardware::{Hardware, LinkerInfo, SccacheInfo, Toolchain};
use crate::metadata::GraphAnalysis;
use crate::planner::{Advice, Schedule};
use crate::util::{bold, cyan, dim, green, magenta, yellow};

pub fn header(title: &str) {
    let line = "─".repeat(title.chars().count().max(10) + 2);
    println!("{}", dim(&line));
    println!("{}", bold(&format!(" {title}")));
    println!("{}", dim(&line));
}

pub fn hardware_block(hw: &Hardware, tc: &Toolchain, linker: &LinkerInfo, scc: &SccacheInfo) {
    header("Hardware & toolchain");
    println!(
        "  {:22} {} physical / {} logical cores",
        "CPU", hw.physical_cores, hw.logical_cores
    );
    println!("  {:22} {} MiB ({} GiB)", "RAM", hw.ram_mb, hw.ram_mb / 1024);
    println!("  {:22} {} {}", "OS / arch", hw.os, hw.arch);
    println!("  {:22} {}", "rustc", tc.rustc_verbose);
    println!("  {:22} {}", "host target", tc.host);
    println!(
        "  {:22} {} {}",
        "linker",
        linker.selected.name(),
        dim(&format!("— {}", linker.reason))
    );
    if !linker.available.is_empty() {
        println!("  {:22} {}", "available", linker.available.join(", "));
    }
    println!(
        "  {:22} {} {}",
        "sccache",
        if scc.installed { "installed" } else { "not installed" },
        dim(&format!("— {}", scc.reason))
    );
    println!("  {:22} {}", "jobs (auto)", hw.recommended_jobs);
}

pub fn graph_block(graph: &GraphAnalysis, sched: &Schedule) {
    header("Dependency graph");
    println!("  {:22} {}", "crates to build", bold(&graph.package_count.to_string()));
    println!(
        "  {:22} {}",
        "workspace crates",
        graph.workspace_packages.join(", ")
    );
    println!(
        "  {:22} {} (max width at one level: {})",
        "parallel width",
        sched.max_width,
        sched.max_width
    );
    println!(
        "  {:22} {} levels",
        "waves",
        sched.waves.len()
    );
    println!(
        "  {:22} cost {} of {} ({:.0}% serial)",
        "critical path",
        sched.critical_path_cost,
        sched.total_cost,
        sched.serial_fraction * 100.0
    );
    println!(
        "  {:22} up to {:.1}× (graph shape limit)",
        "ideal speed-up",
        sched.max_parallel_speedup
    );
    println!("  {:22} {}", "scheduling", dim(&sched.why));

    if !graph.critical_path.is_empty() {
        println!("\n  {}", bold("Critical path (slowest chain):"));
        for (i, id) in graph.critical_path.iter().enumerate() {
            if let Some(n) = graph.nodes.get(id) {
                let marker = if n.has_build_script { " ⚙" } else { "" };
                println!(
                    "   {}{} {}-{} {}",
                    "  ".repeat(0),
                    dim(&format!("{:>2}.", i + 1)),
                    n.name,
                    n.version,
                    dim(&format!("(weight {}{marker})", n.weight))
                );
            }
        }
        println!("{}", dim("  ⚙ = has a build script (compiles a host binary; serialised)"));
    }
}

pub fn top_cost_block(graph: &GraphAnalysis) {
    header("Most expensive dependencies (heuristic weights)");
    let mut nodes: Vec<_> = graph.nodes.values().filter(|n| !n.is_workspace).collect();
    nodes.sort_by_key(|n| std::cmp::Reverse(n.weight));
    println!(
        "  {:<28} {:>7} {:>6} {:>6}  {}",
        "crate", "weight", "fan-in", "depth", "categories"
    );
    for n in nodes.iter().take(15) {
        println!(
            "  {:<28} {:>7} {:>6} {:>6}  {}",
            format!("{}-{}", n.name, n.version),
            n.weight,
            n.fan_in,
            n.depth,
            if n.categories.is_empty() {
                dim("—")
            } else {
                cyan(&n.categories.join(", "))
            }
        );
    }
    println!(
        "\n{}",
        dim("  Weights are heuristics used for scheduling advice; `rusturbo benchmark` measures real time.")
    );
}

pub fn actix_block(graph: &GraphAnalysis) {
    let a = &graph.actix;
    header("Actix Web profile");
    if !a.detected {
        println!("  {}", dim("no actix-web in this graph"));
        return;
    }
    println!(
        "  {:22} {}",
        "actix-web",
        bold(a.version.as_deref().unwrap_or("?"))
    );
    for (label, list) in [
        ("async runtime", &a.runtime),
        ("serialization", &a.serialization),
        ("database", &a.database),
        ("http/networking", &a.http),
    ] {
        println!(
            "  {:22} {}",
            label,
            if list.is_empty() {
                dim("—").to_string()
            } else {
                list.join(", ")
            }
        );
    }
    if !a.observed_features.is_empty() {
        println!("\n  {}", bold("Features Cargo will enable:"));
        for (k, v) in &a.observed_features {
            println!("   {k}: {}", v.join(", "));
        }
    }
}

pub fn advice_block(advice: &Advice) {
    header("Recommendations");
    if advice.recommendations.is_empty() {
        println!("  {}", green("nothing to flag — the graph looks tight"));
        return;
    }
    for r in &advice.recommendations {
        let badge = match r.impact.as_str() {
            "high" => magenta("HIGH"),
            "medium" => yellow("MED "),
            _ => dim("LOW "),
        };
        let risk = match r.risk.as_str() {
            "safe" => green("safe"),
            _ => yellow("needs review"),
        };
        println!("  [{badge}] {} {}", r.title, dim(&format!("({risk})")));
        for line in wrap(&r.detail, 96) {
            println!("        {line}");
        }
    }
}

pub fn cache_note(hit: bool, detail: &str) {
    if hit {
        println!("  {} {}", green("cache hit:"), detail);
    } else {
        println!("  {} {}", yellow("cache miss:"), detail);
    }
}

fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        if line.len() + word.len() + 1 > width && !line.is_empty() {
            out.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        out.push(line);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_respects_width_and_keeps_all_words() {
        let text = "one two three four five six seven eight";
        let lines = wrap(text, 12);
        assert!(lines.iter().all(|l| l.len() <= 12 || !l.contains(' ')));
        assert_eq!(lines.join(" "), text);
    }

    #[test]
    fn wrap_handles_empty() {
        assert!(wrap("", 10).is_empty());
    }
}
