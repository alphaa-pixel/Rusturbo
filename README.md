<div align="center">

<img src="assets/logo-640.png" alt="RustTurbo mascot" width="260" />

# RustTurbo

**Extreme Rust compilation accelerator.**
Make `cargo run` dramatically faster — especially the *first* one.

`rusturbo` is not a wrapper that flips a few Cargo flags. It analyses your dependency
graph, fingerprints it, schedules it against your hardware, reuses dependency artifacts
across projects, drives a fast linker, and **measures the result** instead of claiming it.

</div>

---

## Install

```bash
cargo install --path .          # from a checkout
# or
cargo build --release && cp target/release/rusturbo ~/.local/bin/
```

Requirements: Rust 1.74+, Cargo. Optional: `sccache` (shared compiler cache), `mold` or
`lld` (fast linkers). RustTurbo detects what is present and tells you what it is using.

## Quick start

```bash
cd my-actix-app
rusturbo init          # writes rusturbo.toml (auto-detects Actix Web)
rusturbo analyze       # graph, critical path, hardware, cache key, advice
rusturbo run           # accelerated cargo run
rusturbo benchmark     # real cold/warm measurements → bench/report.md
```

## Commands

| Command | What it does |
|---|---|
| `rusturbo init` | Writes `rusturbo.toml` with tuned defaults. Detects Actix Web and selects its preset. |
| `rusturbo analyze` | Full report: hardware, toolchain, linkers, graph, critical path, expensive deps, Actix profile, recommendations, cache state. `--json` for machines. |
| `rusturbo optimize` | Applies the recommendations it can apply safely and persists them to `rusturbo.toml`. |
| `rusturbo run` | Accelerated `cargo run`, with cache reuse, linker selection, scheduling and Actix tuning. |
| `rusturbo build` | Same acceleration without running the binary. |
| `rusturbo benchmark` | Measures five scenarios for real (see below) and writes JSON + Markdown. |
| `rusturbo clean` | Removes the project `target/` and/or RustTurbo caches. |

Useful flags: `--dry-run` (print the exact commands and environment), `--explain` (why a
cache decision happened), `--no-cache`, `--profile dev|release`, `--manifest-dir`,
`--cargo-arg`, and `-- <args>` to forward arguments to your program.

## Where the time actually goes

For a cold Actix Web project the wall time is roughly:

1. **Registry + download** (network-bound) — nothing RustTurbo does can remove a download
   of a crate you have never fetched. It *can* remove the recompilation.
2. **Dependency compilation** — the dominant cost, 80–95% of a cold build. This is what
   the artifact cache attacks.
3. **Linking** — a second or more for a large binary; `mold`/`lld` cut it significantly.
4. **Your crate** — small, and incremental after the first build.

RustTurbo's leverage is concentrated on (2): if a dependency graph has been compiled once
on this machine, it does not need to be compiled again — not for a rebuild, and not for a
*different project* with the same dependencies.

## How the acceleration works

### 1. Dependency fingerprinting

Every reuse decision is keyed on a hash of exactly the inputs that can change the bytes of
an artifact:

```
rusturbo-artifact-v1
├── rustc -vV (verbatim: host, LLVM version, commit hash)
├── cargo --version
├── target triple
├── profile (dev/release)
├── resolved graph: sorted name-version[features]
├── dep debug level, codegen-units, preset
├── extra RUSTFLAGS
└── environment (CC, CXX, PKG_CONFIG_PATH, OPENSSL_*, RUSTFLAGS, …)
```

Hashes are domain-separated and order-stable (`src/fingerprint.rs`), and the full input
list is stored with every cache entry, so `rusturbo analyze` can show *why* a key is what
it is. `CARGO_TARGET_DIR` is deliberately **not** an input — otherwise the shared-target
strategy could never hit.

### 2. Two reuse strategies

| Strategy | Mechanism | When it wins |
|---|---|---|
| **Shared target dir** (default) | Projects with identical graph + toolchain + profile share one `CARGO_TARGET_DIR` under the cache root. Cargo's own fingerprints then mark every dependency fresh; only your crate compiles and links. | New project, familiar dependency set — the "first build" case. |
| **Portable entry** | A snapshot of `<target>/<profile>` stored under the key, materialised into a project's own `target/` using hard links (falling back to copies). | Project must keep its own target dir, or the graph matches but the toolchain directories differ. |

Artifacts are immutable once rustc writes them, which is why hard-linking a cached
artifact into a target dir is safe.

### 3. Honest limits (read this)

- A cache **cannot** make a crate that has never been built or downloaded compile in zero
  time. `rusturbo-clean` (empty cache, empty target) is therefore *not* faster than
  `cargo-clean` in compilation terms, and the benchmark reports it that way.
- **Build scripts are a real hazard.** Crates like `openssl-sys` and `libsqlite3-sys` can
  embed host library versions into their artifacts. RustTurbo detects them, warns, and
  lets you set `cache.build_scripts = "strict"` to refuse sharing for such graphs.
- First-run gains come from a *previously populated* cache: same machine, a project you
  built yesterday, or a CI cache restored from a previous run.

### 4. Scheduling and graph analysis

`cargo metadata` provides the exact graph Cargo will build. RustTurbo computes:

- **critical path** — the heaviest root-to-leaf chain; it is the hard lower bound on wall
  time regardless of core count;
- **waves** — which crates can compile concurrently;
- **graph width** at the widest level, compared against cores *and* RAM (each large
  rustc/LLVM instance can transiently want ~1 GiB; oversubscription causes swap thrash,
  which is much worse than serialising);
- **duplicate crate versions** — the same crate compiled twice is pure waste;
- **over-broad features** (e.g. `tokio = { features = ["full"] }`) that pull optional
  crates into the graph.

`rusturbo analyze` prints all of it with the reasoning spelled out.

### 5. Linker engine

`mold` → `lld` → system `ld` is selected automatically when present, via
`-fuse-ld=…` in `RUSTFLAGS`. Selection is **refused** on targets where the flag is not
applicable (msvc/musl) rather than silently doing nothing, and `linker.mode = "default"`
pins the system linker. Link speed is measured separately from compile speed by the
benchmark (build time with a fully warm cache is dominated by linking).

### 6. Actix Web preset

`rusturbo init` detects `actix-web` and writes a preset that trims debug info for the
Actix/Tokio/Hyper/serde stack (including optional `sqlx`/`postgres` crates when present)
while keeping your own handlers fully debuggable and incremental. The preset is applied
through `cargo --config` arguments, so **your `Cargo.toml` is never edited**.

## Correctness contract

RustTurbo will not change what your program does. Specifically:

- **Optimisation levels are never touched.** Grep the source: no `opt-level` is generated
  anywhere. Dev stays `opt-level = 0`, release stays as your manifest says.
- **Your `Cargo.toml` is never rewritten.** Presets are passed as `--config` flags.
- **No fake cache hits.** A key mismatch is a miss, and the miss reason is always printed.
- **No shell.** Every command is an explicit argv vector.
- **Release builds only get debuginfo trimmed**, never LTO/codegen changes.
- **`--dry-run` shows everything** it would execute.

What it *does* change, by design: debug info for dependencies (configurable), codegen
units for dev, `CARGO_TARGET_DIR`, linker choice, and cache reuse. All are build-time
properties; none change compiled semantics.

## Benchmarking

`rusturbo benchmark` runs five scenarios and measures wall time, time to first
executable, time to `READY` printed by the app, peak RSS and CPU seconds of the whole
process tree (sampled from `/proc` every 100 ms).

| Scenario | Target dir | Registry | Turbo cache |
|---|---|---|---|
| `cargo-clean` | empty | empty (real downloads) | n/a |
| `cargo-warm-registry` | empty | warm | n/a |
| `rusturbo-clean` | empty | warm | empty |
| `rusturbo-warm` | warm | warm | populated |
| `rusturbo-shared` | **empty** | warm | populated |

`rusturbo-shared` is the headline number: it copies your project to a fresh directory with
no `target/` and builds it with the shared cache. That is the "new project, familiar
dependencies" case.

> **No numbers are published in this README.** Benchmark results are machine-specific;
> run `rusturbo benchmark` on your own hardware. Committed results live in
> [`docs/BENCHMARKS.md`](docs/BENCHMARKS.md) and are regenerated by CI, never invented.

## Configuration (`rusturbo.toml`)

```toml
[profile]
preset          = "balanced"   # balanced | fastest | safe
dep_debug       = 0            # debuginfo for dependencies
debug           = 1            # debuginfo for your crates
jobs            = 0            # 0 = auto (cores, capped by RAM)
incremental     = true
parallel_codegen = true

[cache]
enabled         = true
dir             = ""           # empty = ~/.cache/rusturbo
shared_targets  = true         # reuse CARGO_TARGET_DIR across matching graphs
max_size_gb     = 20           # LRU eviction above this
sccache         = "auto"       # auto | on | off
build_scripts   = "warn"       # strict | warn | off

[linker]
mode            = "auto"       # auto | default | lld | mold
extra_flags     = []

[actix]
preset          = "auto"       # auto | off
tuned_profiles  = true

[run]
profile         = "dev"
```

Unknown keys are rejected loudly, because a silent typo in a build configuration is a bug
waiting to happen.

## Architecture

```
src/
  main.rs         CLI (clap), command wiring
  config.rs       rusturbo.toml: parsing, validation, RAM-aware job count
  metadata.rs     cargo metadata → graph, weights, critical path, Actix profile
  planner.rs      waves, width, ideal speed-up, recommendations
  fingerprint.rs  domain-separated SHA-256 input accumulator
  cache.rs        artifact entries, shared targets, LRU eviction, policy check
  cacheops.rs     key construction, reuse planning, publish, cargo invocation
  presets.rs      profile presets + Actix tuning as --config args
  hardware.rs     cores, RAM, linkers, sccache, toolchain fingerprint
  benchmark.rs    five scenarios, /proc sampling, RSS + CPU, reports
  report.rs       terminal and Markdown rendering
  process.rs      argv-only process execution, env patching, timing
demo/
  actix-hello     small Actix server (workspace member, used by CI)
  actix-large     dependency-heavy Actix server (same code path, bigger graph)
```

## Roadmap (evidence-driven)

Deliberately **not** shipped yet, because each needs a trustworthy verification story
first:

- **Remote/shared cache server** — the fingerprint is already portable; what is missing is
  a content-addressed transport and per-entry signatures.
- **Compatible prebuilt dependency distribution** — must verify exact rustc + features +
  target before use; shipping binaries without that check would be unsafe.
- **Distributed compilation fan-out** — meaningful only when the critical path is wide,
  which `rusturbo analyze` now measures per project.

## License

MIT
