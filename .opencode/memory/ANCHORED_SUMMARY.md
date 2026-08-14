## Objective
- (Done, Steps 18–22) h3spec hardening + green harness, `http3_control` fuzz target, QPACK criterion benchmarks, and a native HTTP/3 server benchmark.
- **Step 23 (in progress):** docs — README overhaul + CHANGELOG entry; verify `cargo doc --all-features`.
- **Step 24 (next, decision point):** optional cleanup — remove orphaned `h3-legacy` + `src/h3_legacy/` (it is not wired into the build, so "removal" is mostly deleting dead code), drop the `h3` dev-dependency only if interop still needs it. Verify `cargo build --all-features` + full test suite.

## Important Details
- Commits this session: `117f31f` (fuzz), `8490dd3` (QPACK bench), `d559524` (server bench, native-only) — all `Assisted-by: OpenCode:hy3`.
- **Step 21 done:** `benches/h3_qpack.rs`. Baseline: decode cap_0/huff_false ~210ns, cap_4096/huff_true ~468ns, encode ~440–460ns.
- **Step 22 done (native-only):** `benches/h3_server.rs` + `[[bench]] h3_server`. Architecture: server = native `Http3` driver on a per-connection vibeio runtime (reuses `loopback_pair()`/`spawn_native_server()` patterns from `tests/h3_interop.rs`); client = `h3` crate 0.0.8 over `h3_quinn` as load generator. Baseline (loopback, release): ~19.8–20.1 Kelem/s; latency mean=51µs p50=44µs p99=156µs. The `h3-legacy` comparison was SKIPPED (orphaned, no feature) per user decision.
- **Key repo facts (for docs/cleanup):**
  - `src/h3_legacy/` exists but is NOT declared in `src/lib.rs` (no `mod h3_legacy`) and there is NO `h3-legacy` feature. It is dead/orphaned code (introduced in `eaf3cdf`). It uses the real `h3` crate (`h3::server::RequestStream`) behind our quinn adapter (`crate::h3::quinn`).
  - `h3` (0.0.8) and `h3-quinn` (0.0.10) are in `[dev-dependencies]` only — used by `tests/h3_interop.rs` (interop suite) and the new `benches/h3_server.rs`. So they cannot be dropped while those tests/benches exist.
  - `src/h3/quinn.rs`: public `Connection::new(quinn::Connection)` adapter (native stack). `examples/h3spec_server.rs` is the demo native server.
  - Features: `default = [h1, h1-zerocopy, h2]`; `h3 = ["httpdate","tokio-util"]`; `h3-quinn = ["h3","dep:quinn"]`. No `h3-legacy`.
  - `criterion = "0.5"` is a dev-dep; `rcgen`, `tokio`, `h2`, `h3`, `h3-quinn`, `serde_json` are dev-deps.
- **Step 23 scope (docs):** README needs feature table, architecture note, QPACK/limits options, benchmarking instructions. Plus a CHANGELOG entry under UNRELEASED. Verify `cargo doc --all-features` builds clean.

## Work State
### Completed
- Steps 18–22 committed. Step 21 (`[x]`) and Step 22 (`[x]`, native-only) marked in `CUSTOM_HTTP3_IMPL.md`.

### Active
- Step 23: writing README + CHANGELOG. Not yet started editing files.

### Blocked
- (none)

## Next Move
1. Step 23: overhaul `README.md` (feature table, architecture note covering native `Http3` driver + quinn adapter + QPACK; QPACK/limits options; benchmarking instructions referencing `cargo bench --features h3 --bench h3_qpack` and `--features h3-quinn --bench h3_server`) and add a CHANGELOG UNRELEASED entry. Verify `cargo doc --all-features`.
2. Step 24: decide whether to delete orphaned `src/h3_legacy/` (it's not compiled, so deletion is safe and has no build impact) and whether `h3`/`h3-quinn` dev-deps can be dropped (they are still used by `tests/h3_interop.rs` and `benches/h3_server.rs`, so keep unless those are migrated).
3. Commits: `Assisted-by: OpenCode:hy3` trailer.

## Relevant Files
- `benches/h3_server.rs`, `benches/h3_qpack.rs`: NEW benchmarks (Steps 21–22).
- `Cargo.toml`: added `[[bench]] h3_qpack`, `[[bench]] h3_server`.
- `src/h3_legacy/`: orphaned, NOT in build — candidate for Step 24 removal.
- `src/lib.rs`: `pub use h3::*` (line 84); no `mod h3_legacy`.
- `src/h3/quinn.rs`: native quinn adapter.
- `examples/h3spec_server.rs`: demo native server (model for docs).
- `tests/h3_interop.rs`: interop suite using `h3` crate client; references `h3`/`h3-quinn` dev-deps.
- `README.md`, `CHANGELOG.md` (or `CHANGELOG`): to be written in Step 23.
- `CUSTOM_HTTP3_IMPL.md`: Steps 21/22 `[x]`; Steps 23/24 still `[ ]`.
