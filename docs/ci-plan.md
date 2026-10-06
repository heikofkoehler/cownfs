# CI Plan: Tests, Fuzz, Loom

## Substrate Decision (needed from Heiko)

Choose where CI runs:
- **Option A: GitHub Actions** (ubuntu-latest runners)
  - Pros: Zero setup, free for public repos
  - Cons: 6-hour job limit (fuzz needs 6×1h = 6h, fits barely); no 16-core runners for P4 gate
- **Option B: Self-hosted runner** (your Mac mini or Linux box)
  - Pros: No time limits, can use 16-core for P4, persistent fuzz corpus
  - Cons: You maintain it

**Blocker**: GitHub OAuth on this box lacks `workflow` scope, so I cannot push `.github/workflows/` files. You'll need to add them manually or grant the scope.

## PR Checks (fast, every PR)

Already in `.github/workflows/pr.yml`:
- `cargo fmt --check`
- `cargo build --workspace --all-targets` (warning-free)
- `cargo test --workspace` (excludes `p4_parallel_reads_scale` which needs 16 cores; excludes `--ignored` nightly tests)

**To add**: clippy with `-D warnings` for new code.

## Nightly Jobs (slow, 1-hour runs)

### 1. Fuzz (6 targets × 1 hour each)

**Targets** (in `fuzz/fuzz_targets/`):
- `fuzz_rpc_frame` — RPC framing parser
- `fuzz_xdr_compound` — XDR/COMPOUND decoder
- `fuzz_decode_node` — B-tree node decoder
- `fuzz_superblock` — Superblock parser
- `fuzz_referral` — Referral table parser
- `fuzz_backup` — Backup stream parser

**Requirements**:
- Nightly Rust toolchain (`rustup toolchain install nightly`)
- `cargo fuzz` (`cargo install cargo-fuzz`)
- Each target: `cargo fuzz run <target> -- -max_total_time=3600`
- Corpus persists in `fuzz/corpus/<target>/` (commit the corpus)

**Resources**: 1 CPU per target; can run in parallel (6 CPUs) or serial (6 hours).

### 2. Loom (concurrency model checking)

**Tests** (in `crates/cownfs-core/tests/t4_loom.rs`):
- `loom_txg_wait_wakes_on_sync`
- `loom_txg_wait_gets_error`
- `loom_txg_multiple_waiters`

**Command**: `cargo test --release --test t4_loom` (loom requires release mode)

**Resources**: CPU-intensive; each test explores thread interleavings. On 2-core box, takes hours. Needs 8+ cores for practical nightly runs.

**Note**: Full T4 model (P1 commit-split) not yet written — current loom covers TxgCoord only.

### 3. T7 Chaos (1-hour SIGKILL loop)

**Test**: `cargo test --release --test t7_chaos t7_chaos_nightly -- --ignored --nocapture`

**What it does**: Spawns real `cownfs-server`, does FILE_SYNC writes with ledger, random SIGKILL, restart, verifies ledger + fsck. Loops for 1 hour.

**Resources**: Needs to bind to 127.0.0.1 ports; uses temp files. 1 CPU.

### 4. P4 Scaling Gate (16-core only)

**Test**: `cargo test --release --test p4_parallel_reads`

**Requirement**: 16-core machine to verify 8× scaling. Fails RED by design on smaller hosts.

## Workflow Files to Create

`.github/workflows/nightly.yml`:
```yaml
name: Nightly (fuzz + loom + chaos)
on:
  schedule:
    - cron: '0 2 * * *'  # 2am UTC
  workflow_dispatch:      # manual trigger

jobs:
  fuzz:
    runs-on: ubuntu-latest
    strategy:
      matrix:
        target: [fuzz_rpc_frame, fuzz_xdr_compound, fuzz_decode_node, fuzz_superblock, fuzz_referral, fuzz_backup]
    steps:
      - uses: actions/checkout@v4
      - uses: actions-rust-lang/rustup@v1
        with:
          toolchain: nightly
      - run: cargo install cargo-fuzz
      - run: cargo fuzz run ${{ matrix.target }} -- -max_total_time=3600
        working-directory: fuzz

  loom:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: actions-rust-lang/rustup@v1
        with:
          toolchain: stable
      - run: cargo test --release --test t4_loom

  chaos:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: actions-rust-lang/rustup@v1
        with:
          toolchain: stable
      - run: cargo test --release --test t7_chaos t7_chaos_nightly -- --ignored --nocapture
```

## Steps to Enable

1. **You decide**: GitHub Actions vs self-hosted (see above).
2. **If GitHub Actions**: You manually add `.github/workflows/nightly.yml` (I can't push workflow files due to OAuth scope). Or grant `workflow` scope and I'll push it.
3. **Commit fuzz corpus**: `git add fuzz/corpus/` (seeds the fuzzers).
4. **16-core for P4**: Either a self-hosted 16-core runner, or accept P4 as manual-only.

## Current Status

- ✅ PR workflow exists (fmt, build, test)
- ✅ Fuzz targets written (6/7, delta N/A)
- ✅ Loom tests written (TxgCoord only)
- ✅ T7 chaos harness built (short + nightly)
- ❌ Nightly workflow file not created (blocked on substrate + OAuth scope)
- ❌ Fuzz corpus not committed
- ❌ 16-core runner not available
