# Design notes

This document records the engineering decisions behind RustTurbo, including the ones that
were rejected. It exists so future changes can be judged against the original reasoning
rather than re-litigated from vibes.

## Problem statement

`cargo run` on a new Actix Web project takes minutes. Breakdown for a typical ~150-crate
graph on 8 cores:

| phase | share of cold wall time | attackable by RustTurbo? |
|---|---|---|
| registry index + crate downloads | 5–15% (network-bound) | Only by avoiding the need to rebuild after download |
| dependency compilation | 70–90% | **Yes** — this is the target |
| linking | 5–15% | Yes, partially (linker choice) |
| user crate compilation | 2–10% | Yes, partially (preset) |

The cache cannot remove phase 1 for a crate never downloaded — that would be a lie. It
removes the *recompilation* in phase 2, which is the larger and more variable cost, and it
does so for graphs compiled by *any* earlier project on the machine.

## Decision: fingerprint inputs

A dependency artifact's bytes depend on far more than its source:

- exact rustc build (`rustc -vV`: commit hash, LLVM version, panic strategy, host);
- target triple;
- the *resolved feature set* of every crate in the graph (a feature can add a dependency
  and therefore change codegen);
- profile settings that affect codegen (`debug` info, `codegen-units`);
- `RUSTFLAGS` (including linker flags via `-C link-arg`/`-fuse-ld` in RUSTFLAGS);
- environment variables leaked into build scripts (`CC`, `PKG_CONFIG_PATH`, `OPENSSL_*`).

Every one of these is in the key. The full input list is stored in each entry's
`meta.json`, so a user can diff two keys and see the cause of a miss.

`CARGO_TARGET_DIR` is explicitly excluded: it is RustTurbo's own output knob, and
including it would make shared-target hits impossible.

## Decision: shared target directory as the primary strategy

Two ways to reuse a built dependency set:

1. copy artifacts into a project's own `target/`;
2. point all compatible projects at one `target/`.

Option 2 is strictly better when it applies: no copying, no size duplication, and Cargo's
own fingerprint files travel with the artifacts, so Cargo itself confirms freshness
instead of RustTurbo second-guessing it. The risk is Cargo profile drift between projects
(a project with `[profile.dev] opt-level = 1` would poison the shared dir), which is why
the key includes the preset and dep-debug settings, and why `cargo --config` args are part
of the run rather than being assumed.

Option 1 remains for the portable case (different toolchain layout, or a project that must
keep its own target dir). Copying uses hard links where possible — artifacts are immutable
after rustc writes them.

## Decision: presets as `--config` arguments, never manifest edits

Rewriting `<project>/Cargo.toml` is how build accelerators lose trust: it creates merge
conflicts, surprises in `git status`, and changes that persist after the tool is
uninstalled. Cargo's `--config` flag can set profile values per invocation, so RustTurbo
keeps the project pristine and every applied setting is visible in `--dry-run` output.

Consequence: `rusturbo` must be in the loop for its settings to apply. That is an explicit
trade — the alternative (a `[profile]` block in the manifest) would accelerate plain
`cargo run` too, but at the cost of silent manifest mutations.

## Decision: never touch `opt-level`

The single most tempting "speed-up" is dropping dependencies to `opt-level = 0`... which
they already are in dev. The dangerous variant is changing *release* optimisation, or
enabling LTO for dev builds to shrink link time. Both change generated code. RustTurbo
does neither, and a unit test (`presets::tests::dev_args_never_touch_opt_level`) asserts
that no `opt-level`, `lto` or `panic` key is ever emitted.

The one aggressive-but-safe lever used is **debug info**: `debug = 0` for dependencies is
a pure compile-and-link cost reduction with no effect on executed instructions. It costs
you source-level backtraces inside dependency frames; `preset = "safe"` restores full
debuginfo.

## Decision: RAM-aware job count

`cargo -j $(nproc)` is the standard advice and it is often wrong on containers. Eight
rustc/LLVM processes on a 4 GiB box swap or get OOM-killed, and a build that would have
taken 90 seconds serialised takes 4 minutes of thrash. RustTurbo caps jobs at
`min(physical_cores, floor(0.8 × RAM_GiB))` and reports the reason in `analyze`.

## Decision: refuse linkers that cannot apply

On `x86_64-pc-windows-msvc`, `-fuse-ld=lld` in `RUSTFLAGS` is inert or harmful. Detecting
`lld` on PATH and then passing the flag anyway would make the tool *look* like it is doing
something while the linker is untouched. RustTurbo reports "found but not applicable for
this target" and keeps the system linker.

## Rejected: patching Cargo's fingerprint files

It is technically possible to rewrite `.fingerprint/**/lib-*.json` to make Cargo believe
dependencies are fresh. This is how some cache tools got fast — and how they produced
subtly miscompiled artifacts, because the fingerprint is the *only* record of what was
compiled with which flags. RustTurbo never writes into Cargo's fingerprint directory; it
lets Cargo decide, and only manipulates which directory Cargo uses.

## Rejected: synthesising prebuilt dependency artifacts

Downloading `.rlib`s built elsewhere requires verifying rustc build, target, features,
profile and RUSTFLAGS *exactly*. Without that verification it is a supply-chain and
correctness hazard, so it is listed as roadmap with the verification requirement spelled
out, rather than shipped as a fast path that occasionally miscompiles.

## Rejected: claiming cold-start wins

Marketing ("10× faster first build, no cache needed") requires either skipping compilation
or lying. A cold build of a never-before-built graph is bounded by rustc. RustTurbo's
benchmark reports `rusturbo-clean` honestly, including when it is *not* faster than
`cargo-warm-registry`.

## Measurement methodology

`benchmark.rs` runs real commands and samples the process *tree* from `/proc` every 100 ms
for peak RSS and CPU time. It measures:

- wall time until the compiler returned (build);
- wall time until the app printed `READY`, i.e. socket bound (startup);
- total = build + startup ("time to application serving");
- peak RSS across the whole tree (LLVM threads are part of the process, but codegen-units
  can spawn helper processes);
- bytes added under the target dir and Cargo home (download size).

Because single samples are noisy, the intended workflow is 3+ repetitions and comparing
medians; the report format keeps every sample so that is possible. CI runs the two warm
scenarios on every push; the cold-download scenario is opt-in because it depends on
crates.io latency, not on this machine.

## Known limitations

1. **`cargo run` vs `cargo build` + exec.** In shared-target mode RustTurbo still invokes
   `cargo run` so Cargo's freshness check gates the execution. That costs a few
   milliseconds of Cargo startup — accepted in exchange for never running a stale binary.
2. **Incremental directories do not travel** in portable entries (they are keyed on
   absolute paths). Reuse is dependency-level, which is the expensive part anyway.
3. **Workspace-wide graphs**: all workspace members share a key. Two members with
   different feature sets can therefore thrash the shared dir; the LRU keeps this
   bounded but does not eliminate it.
4. **Windows**: path/link handling is implemented but only Linux and macOS are exercised
   in CI with real builds. Windows compiles and unit-tests, but shared-target hard-link
   behaviour on NTFS is unverified.
