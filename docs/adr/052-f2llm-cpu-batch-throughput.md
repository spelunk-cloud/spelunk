# ADR-052: F2LLM embedder — CPU batch throughput improvements

**Date:** 2026-06-30  
**Deciders:** Architect  
**Trigger:** On GPU-less machines (Linux CI runners, low-cost
cloud VMs, developer laptops with no Apple Silicon Metal) the native
F2LLM-v2-330M embedder drives 100% of one CPU core and produces roughly
~400 ms per chunk at the current `EMBED_BATCH_SIZE = 8`. Indexing a large
codebase on CPU-only hardware is noticeably slow and the question is: which
specific levers are worth pulling?

---

## Context

### What we know about the workload

The F2LLM-v2-330M embedder (Qwen3 decoder, Q8_0 GGUF, 896-dim, candle runtime)
is the v0.9 default, shipped in `spelunk-server`'s `embedder_native.rs`. Key
structural facts that constrain the design space:

- **GPU-bound when a GPU is present.** Prior measurement (spelunk decision
  recorded in project memory: "Embedder is GPU-bound; no disaggregation")
  showed ~99.8% of wall-clock in the GPU forward pass on Metal. No CPU
  optimisation is relevant there.
- **CPU-only path is the target.** `Device::Cpu` is selected when the `metal`
  feature is absent or Metal init fails. This is the path for Linux CI runners,
  Linux cloud VMs, and Windows/Linux developer machines.
- **Tokenisation was already ruled out as a lever.** Disaggregating tokenise
  from embed was measured as a 1.000× no-op (the GPU forward pass dominates).
  On CPU the forward pass still dominates, so tokenisation disaggregation
  remains a no-op — do not repeat that investigation.
- **Batching is already implemented for CPU.** `embed_batch` runs a single
  padded forward pass when `Device::Cpu` and `max_seq <= BATCH_MAX_SEQ (512)`,
  amortising per-call overhead. Sequences are sorted by length before batching
  to minimise padding waste.
- **The inner loop is a Q8_0 matmul executed by candle's CPU kernel.** Each
  transformer layer runs: Q/K/V projections (3× `QMatMul`), attention scores
  (GEMM), softmax, V aggregation (GEMM), O projection, and gate/up/down MLP
  projections (3× `QMatMul`). These are the dominant FLOP contributors.
- **No BLAS or Accelerate feature is currently enabled.** The `Cargo.toml`
  declares `candle-core/metal` as the only optional candle feature; no
  `accelerate`, `mkl`, or `openblas` feature is wired in. Candle's Q8_0 CPU
  matmul therefore runs its own hand-written kernel, not a vendor BLAS routine.
- **Thread pool is at candle defaults.** No `RAYON_NUM_THREADS`,
  `candle_core::set_num_threads`, or any intra-op parallelism configuration
  exists in the codebase. Candle uses rayon for some CPU ops; thread count
  inherits from rayon's default (number of logical CPUs).

### The forward-pass anatomy (relevant to threading)

The batched path builds a `[B, max_seq, hidden]` activation tensor and runs it
through 28 transformer layers (F2LLM-v2-330M config). Each layer performs:

1. RMSNorm (element-wise, cheap)
2. Three `QMatMul` projections: `[B, seq, hidden] × [hidden, proj_dim]` — this
   is where most CPU time goes
3. RoPE rotation, GQA expand, scaled dot-product attention GEMM
4. Output projection + residual
5. Post-norm + MLP gate/up/down projections (3 more `QMatMul` calls)

With `B = 8` and `seq ≤ 512`, the matmul is `[8 × 512, 896] × [896, 896]` —
a batch-mode GEMM that candle should be able to parallelise across cores.

---

## Options considered

### Option A: Increase `EMBED_BATCH_SIZE` beyond 8

**What it does.** More sequences per forward pass → larger batch GEMM → better
CPU BLAS utilisation. At B=8 the GEMM is `[4096, 896] × [896, 896]`; at B=16
it doubles the M dimension.

**Expected gain.** Moderate. Batch GEMM efficiency typically plateaus once M
is large enough to fill the SIMD pipeline; doubling from B=8 to B=16 may give
10–30% throughput improvement. Diminishing returns after B=16–32 on typical
developer hardware (4–8 physical cores).

**Constraint.** The attention tensor is `[B × n_head × max_seq²]`. At B=8,
seq=512: `8 × 16 × 512² × 4 B ≈ 134 MB`. At B=16: ~268 MB. At B=32: ~536 MB.
On machines with ≥4 GB RAM this is fine; on 1 GB VMs it may OOM for long
sequences. The `BATCH_MAX_SEQ = 512` guard already handles the length dimension;
a larger `EMBED_BATCH_SIZE` only raises the B dimension.

**Verdict.** Worth doing, low risk, measurable. The right ceiling is likely
B=16 or B=32; benchmark decides.

### Option B: Enable `candle-core/accelerate` on macOS or `candle-core/mkl` on Linux

**What it does.** Routes candle's dense matmul through Apple Accelerate (BLAS)
or Intel MKL instead of candle's own kernel. Accelerate in particular is a
first-class system framework on macOS — no download, ships with the OS.

**Expected gain.** Potentially large (2–4×) for the dense F32 matmuls in the
attention GEMM. Q8_0 projection weights use candle's own dequantise-then-multiply
path; Accelerate accelerates only the final F32 multiply, so the gain on
projection matmuls is smaller but still real on well-vectorised hardware.

**Constraint.** `accelerate` is a macOS-only feature. `mkl` requires Intel
hardware and a redistributable license. Neither is cross-platform. The build
matrix must gate these features by target — `[target.'cfg(target_os = "macos")'.dependencies]`
for Accelerate. This adds build complexity. For Metal-enabled macOS builds the
GPU already wins, so Accelerate benefits CPU-fallback macOS only.

**Verdict.** High value on macOS CPU-fallback (e.g. M-chip Mac where Metal
unavailable or failed). Moderate on Linux/Intel if MKL is acceptable. Requires
careful feature-gating in `Cargo.toml`. Should be measured before shipping to
verify it does not regress Metal builds.

### Option C: Configure the rayon intra-op thread pool explicitly

**What it does.** Set candle's intra-op thread count to match the available
physical cores, or to a fraction thereof to leave headroom for the tokio
`spawn_blocking` pool. Controlled via `RAYON_NUM_THREADS` env var or
`rayon::ThreadPoolBuilder` at startup.

**Expected gain.** Small to none. Rayon's default already discovers the logical
CPU count. The `spawn_blocking` call already takes the embedder off the async
executor. The only scenario where explicit tuning helps is a container with CPU
limits (e.g. cgroups v2 quotas) where rayon's default overestimates available
parallelism and causes thrashing. Correctness of candle's rayon usage varies by
op; not all matmuls use rayon.

**Verdict.** Low-priority. Useful as an operator knob (`SPELUNK_EMBED_THREADS`
env var → `RAYON_NUM_THREADS` pass-through) but unlikely to yield a measurable
throughput improvement on default hardware. Do not block on this.

### Option D: Switch Q8_0 weights to Q4_K or Q4_0 quantisation

**What it does.** A 4-bit quantisation halves the weight memory bandwidth at the
cost of slightly lower precision. On bandwidth-bound CPU matmul, this can yield
up to 2× throughput. Candle supports `GgmlDType::Q4_0` and `GgmlDType::Q4_K`.

**Expected gain.** Depends on whether the workload is compute-bound or
memory-bandwidth-bound. At B=8 with seq=128, the weights are loaded repeatedly
— if the working set spills L3 cache, bandwidth is the bottleneck and Q4 wins.
At B=16–32 with well-warmed BLAS, compute may dominate and Q4 buys less.

**Risk.** Embedding quality impact must be measured. F2LLM-v2-330M was validated
at Q8_0 (R@10 = 0.60 on our Rust benchmark); Q4 has not been evaluated. Quality
regression at 4-bit is possible for decoder-based embedders because the last-token
hidden state is sensitive to accumulated quantisation error across 28 layers.

**Verdict.** Investigate after Option A + B are benchmarked. Run the R@10
quality evaluation at Q4_0 and Q4_K before committing. Do not ship without a
quality gate.

### Option E: Avoid redundant data copies between tokenisation and forward pass

**What it does.** Currently `embed()` collects token IDs into `Vec<Vec<u32>>`,
sorts them, then builds a flat `Vec<u32>` padded buffer before constructing the
`Tensor`. This is two heap allocations per batch. It could be collapsed to one.

**Expected gain.** Negligible. The tokenisation + copy is O(B × seq) integer
operations; the forward pass is O(B × seq² × hidden) floating-point operations.
At B=8, seq=256: tokeniser produces ~2000 u32 values; the forward pass processes
~256M float ops. The copy is sub-millisecond and is not on the critical path.
This is exactly the class of "disaggregation" no-op ruled out by prior
measurement — do not pursue it.

**Verdict.** Reject. Not worth implementing. Cited here so we do not revisit it.

### Option F: Parallel sub-batch forward passes using multiple threads

**What it does.** Instead of one sequential batch loop on the tokio
`spawn_blocking` thread, spawn multiple threads each running a sub-batch.
Multiple concurrent forward passes could utilise more CPU cores.

**Expected gain.** Likely negative or neutral. Candle's rayon matmuls already
parallelise internally across all cores; adding an outer level of parallelism
causes thread over-subscription, cache thrashing, and lock contention on the
`Mutex<EmbedderInner>`. The mutex already serialises concurrent HTTP requests
to the embedder, making this moot at the API layer too.

**Verdict.** Reject. The existing architecture (single shared `Mutex` +
rayon intra-op parallelism) is correct for CPU inference. Multi-process sharding
(e.g. multiple server instances) is the right scale-out primitive if needed and
is out of scope.

---

## Decision

### Recommended path (two-step, post-v1.0)

**Step 1 (high confidence, implement first):** Benchmark batch size B ∈ {8, 16,
32, 64} on a CPU-only machine with the existing Q8_0 GGUF. If throughput
improvement is ≥15% without RAM regression, raise `EMBED_BATCH_SIZE` to the
optimal value. This is a one-constant change in `embedder_native.rs` with no
build or API changes.

**Step 2 (medium confidence, implement if Step 1 plateau is unsatisfying):**
Enable `candle-core/accelerate` on macOS (CPU-fallback path only) and optionally
`candle-core/mkl` on Linux. Gate behind Cargo features:
`accelerate = ["candle-core/accelerate", "candle-nn/accelerate"]` for macOS,
`mkl = ["candle-core/mkl", "candle-nn/mkl"]` for Linux/Intel. Verify that
enabling Accelerate does not regress Metal GPU latency (it should not, as Metal
and Accelerate are orthogonal dispatch paths). CI must not enable these features
by default (the CI runner may be ARM or lack MKL); the distribution Makefile
gates them by `uname`.

**Deferred:** Q4 quantisation investigation (Option D) — requires a quality
benchmark run (R@10 on the Rust eval set) before a decision can be made.
Thread-pool tuning (Option C) — expose as `SPELUNK_EMBED_THREADS` env var
pass-through only, no code-level tuning.

**Explicitly rejected:** redundant-copy elimination (Option E) and
multi-threaded sub-batch dispatch (Option F) — confirmed no-ops by analysis.

This is **post-v1.0 performance work**. It is not a v1.0 gate. v1.0 ships
with `EMBED_BATCH_SIZE = 8` and no BLAS feature.

---

## Measurement plan

All benchmarks run on a CPU-only machine (no Metal, no GPU). Suggested
reference target: an `x86_64` Linux VM with 4 vCPUs and 8 GB RAM, the same
tier as a typical CI runner or low-cost cloud instance (e.g., AWS `t3.xlarge`).

### Benchmark harness

Add a Criterion benchmark (or a standalone binary in `bench/`) that:

1. Loads the Q8_0 GGUF from the local cache (`NativeEmbedder::load()`).
2. Prepares a synthetic corpus of N=512 chunks drawn from a real-world
   distribution: ~30% short (≤64 tokens), ~50% medium (65–256 tokens), ~20%
   long (257–512 tokens). Use the actual tokeniser.
3. Embeds all 512 chunks through `embed_batch` in sub-batches of size B.
4. Records: total wall time, throughput (chunks/s), peak RSS during the run.

### Measurements required

| Variable | Values to test |
|---|---|
| `EMBED_BATCH_SIZE` | 8 (baseline), 16, 32, 64 |
| Candle features | default (baseline), `+accelerate` (macOS only), `+mkl` (Linux/Intel only) |
| Q dtype (deferred) | Q8_0 (baseline), Q4_0, Q4_K — only after Options A+B are validated |

For each cell: 3 warm-up runs discarded, 5 timed runs, report median ± p95.

### Acceptance threshold

A change ships if it satisfies **both**:
- Throughput improvement ≥ 20% over baseline on the reference hardware.
- Peak RSS increase ≤ 50% over baseline (guards against OOM on 1 GB VMs at
  larger batch sizes).
- For Q4 variants: R@10 on the internal Rust retrieval eval set ≥ 0.55 (the
  Q8_0 baseline is 0.60; a 5-point drop is the maximum tolerable regression).

### Baseline to record before any change

Run the harness with B=8 and default candle features; record the result in
`bench/results/cpu-embed-baseline-YYYYMMDD.json`. This anchors all comparisons.

---

## Consequences

- **No v1.0 impact.** The existing `EMBED_BATCH_SIZE = 8` and no-BLAS
  configuration ships in v1.0 unchanged. This ADR authorises post-v1.0 work only.
- **Build matrix grows slightly.** Enabling Accelerate/MKL behind Cargo features
  adds two non-default feature flags to the distribution build. CI must not
  activate them by default; the macOS release build should enable `accelerate`.
- **Quality gating for Q4.** Any Q4 quantisation experiment must run R@10 on
  the internal Rust eval before merging. The quality gate is not optional.
- **No API changes.** The `/v1/projects/{id}/index/embed` endpoint signature and
  response format are unchanged. `EMBED_BATCH_SIZE` is an internal constant;
  the HTTP batch size (`MAX_BATCH = 64` in `embed_phase.rs`) is independent and
  stays unchanged.
- **Thread-pool knob.** If Step 1 measurement reveals that default rayon thread
  counts are suboptimal in constrained containers, a `SPELUNK_EMBED_THREADS` env
  var (forwarded to `RAYON_NUM_THREADS` before the server starts) is the
  operator escape hatch. No code changes required beyond documentation.
