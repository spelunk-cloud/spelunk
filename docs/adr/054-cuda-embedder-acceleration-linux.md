# ADR-054: CUDA GPU acceleration for the native embedder on Linux

**Date:** 2026-07-01  
**Deciders:** Architect  
**Trigger:** PR #439 added Metal GPU acceleration for the bundled F2LLM-v2-330M
embedder on macOS, making on-device embedding fast on Apple Silicon. Linux and
Windows release builds remain **CPU-only** (`docs/getting-started.md`:
"GPU-accelerated on macOS via candle; CPU-only on Linux/Windows"). The embedder is
~99.8% GPU-bound in its forward pass (ADR-052), so NVIDIA users on Linux are leaving
a large speedup on the table. Which other Candle-supported accelerators can we add,
can we detect them at runtime, and can we fall back to CPU when they are absent?

---

## Context

### What Candle actually supports

The native embedder runs on `candle` 0.11 (`candle-core`, `candle-nn`,
`candle-transformers`, all optional deps behind the `embed-native` feature). Candle's
device backends are a fixed set:

- **CPU** — always available; optionally BLAS-accelerated via Intel **MKL** (x86) or
  Apple **Accelerate** (macOS). These accelerate the *CPU* path only; they are not a
  GPU. Already covered as deferred post-v1.0 work in ADR-052 — **out of scope here.**
- **Metal** — Apple GPUs. Shipped in PR #439 behind the `metal` feature.
- **CUDA** — NVIDIA GPUs, on both Linux and Windows. Optional `cudnn` on top for
  extra speed. This is the **only additional GPU backend** Candle offers for
  Linux/Windows.

There is **no generic cross-vendor GPU backend in Candle.** WebGPU/Vulkan
(huggingface/candle#344) and AMD ROCm (huggingface/candle#346) have been open,
unmerged feature requests since 2023. A truly vendor-neutral accelerator (AMD, Intel,
Vulkan) would require leaving Candle for a different framework (e.g. Burn / CubeCL) —
a runtime rewrite, not a feature flag. It is **explicitly deferred** as a future
investigation, not part of this decision.

**Conclusion: CUDA is the only accelerator we can add without leaving Candle.** This
ADR scopes it to **Linux x86_64 / NVIDIA** for the first cut; Windows CUDA is a
follow-up once the Linux pattern is proven.

### How device selection works today

`select_device()` in `crates/spelunk-server/src/embedder_native.rs` is compile-time
feature-gated with a runtime fallback:

```rust
fn select_device() -> Device {
    #[cfg(feature = "metal")]
    {
        match Device::new_metal(0) {
            Ok(d) => return d,
            Err(e) => tracing::warn!("Metal GPU unavailable ({e}); falling back to CPU"),
        }
    }
    Device::Cpu
}
```

Two independent layers matter:

1. **Compile-time.** A backend is only reachable if its Cargo feature was enabled at
   build time. `metal` is enabled only on the macOS release job
   (`.github/workflows/release.yml`: `features: rich-formats,metal`); Linux and Windows
   jobs build `rich-formats` only. A CUDA backend requires a new `cuda` feature and a
   build that enables it. Crucially, building Candle with `cuda` requires the **CUDA
   toolkit present at build time** — Candle's `candle-kernels` `build.rs` invokes `nvcc`
   to compile GPU kernels. This is a build-host requirement, not just a runtime one.

2. **Runtime.** Within a build that has the feature compiled in, `Device::new_*(0)`
   returns `Err` when no device is present and the code falls back to `Device::Cpu`.
   This is the graceful "no GPU → CPU" path that already works for Metal.

### The distribution question (why a spike is needed)

Metal's fallback is clean because a Metal-linked binary only ever runs on macOS, where
the Metal framework is always present. CUDA is different: a CUDA-compiled binary may be
run on a Linux host with **no NVIDIA driver at all**. Candle 0.11's `cudarc` defaults to
`dynamic-loading` (so the CUDA *driver* is not needed at build time), but historically
`cudarc` could **panic when `libcuda` is missing at runtime** rather than returning a
recoverable error. That single behaviour decides the distribution model:

- If `Device::new_cuda(0)` returns a recoverable `Err` on a driver-less host → we can
  ship **one Linux binary** built with `cuda` that transparently falls back to CPU.
- If it panics / aborts on load → the default CPU binary must **not** carry CUDA; we
  ship a **separate opt-in `-cuda` artifact** (PyTorch `cpu` vs `cu121` model), and the
  installer picks it only when an NVIDIA GPU is detected.

We will not guess. A small spike (below) settles it before any release-matrix change.

---

## Decision

Add an **optional CUDA backend for the native embedder, Linux x86_64 / NVIDIA only**,
mirroring the existing Metal pattern. Ship it behind a non-default `cuda` Cargo feature
so the default CPU build is completely unaffected. Determine the distribution model
(single fall-back binary vs. separate `-cuda` artifact) from the spike result.

### Step 0 — Spike (gates the distribution model)

Throwaway spike, results recorded on the implementation task (no committed spec doc):

1. On a Linux + NVIDIA host, build `spelunk-server` with the `cuda` feature; confirm
   `Device::new_cuda(0)` is selected and embeddings run on-GPU (`nvidia-smi` shows
   utilisation).
2. Run the **same CUDA-compiled binary** on a host with **no** NVIDIA driver. Record
   whether `Device::new_cuda(0)` returns a recoverable `Err` (→ Outcome A, single
   binary) or panics/aborts on load (→ Outcome B, separate artifact), against the exact
   `cudarc` version Candle 0.11 pins.

### Step 1 — Cargo feature

In `crates/spelunk-server/Cargo.toml`, alongside `metal`:

```toml
# cuda: enable CUDA GPU acceleration on Linux (NVIDIA). Requires the CUDA toolkit
# (nvcc) at build time. Non-default; enabled only by the CUDA release build.
cuda = ["candle-core/cuda", "candle-nn/cuda", "candle-transformers/cuda"]
```

An optional `cudnn = ["candle-core/cudnn", ...]` add-on is deferred — `cuda` alone is
the first target. Candle deps stay at `0.11.0`; no version bump.

### Step 2 — Runtime device selection

Extend `select_device()` with a CUDA arm after the Metal arm and before the CPU
default:

```rust
#[cfg(feature = "cuda")]
{
    match Device::new_cuda(0) {
        Ok(d) => return d,
        Err(e) => tracing::warn!("CUDA GPU unavailable ({e}); falling back to CPU"),
    }
}
```

- The batching guard in `embed_batch` already routes **any** non-CPU device
  (`!matches!(self.device, Device::Cpu)`) down the Metal-safe sequential path, so CUDA
  reuses it with no change. A CUDA-specific batched path is a later optimisation, not
  required for correctness.
- The startup status log (currently `on_gpu ? "Metal/GPU" : "CPU"`) is generalised to
  name the actual backend: CUDA / Metal / CPU.
- If the spike shows `new_cuda` can panic on a driver-less host but we still want a
  single binary, guard the probe so a missing driver degrades to CPU rather than
  aborting the process.

### Step 3 — Release CI + distribution (branch on spike outcome)

CUDA builds need the CUDA toolkit installed on the runner (`nvcc` for
`candle-kernels`), e.g. via a `Jimver/cuda-toolkit` action step or a CUDA build
container. spelunk-oss GitHub Actions are free (public repo), so CI minutes are not a
constraint.

- **Outcome A (single binary):** switch the Linux x86_64 job to `rich-formats,cuda`
  and rely on runtime fallback.
- **Outcome B (separate artifact — safe default):** leave the existing CPU
  `x86_64-unknown-linux-gnu` artifact unchanged and **add** a second job producing an
  `x86_64-unknown-linux-gnu-cuda` artifact built with `rich-formats,cuda`. `install.sh`
  detects an NVIDIA GPU (e.g. `nvidia-smi` present) and selects the `-cuda` artifact,
  defaulting to CPU otherwise. `install.ps1` is untouched (Windows deferred).

### Step 4 — Docs

- `docs/building.md`: add the `cuda` feature row and its build requirement (CUDA
  toolkit / `nvcc`) next to `metal`.
- `docs/getting-started.md`: update "CPU-only on Linux/Windows" to note optional CUDA
  acceleration on Linux/NVIDIA.

---

## Consequences

- **Default builds unchanged.** `cuda` is non-default and feature-gated; the CPU Linux
  and Windows artifacts and the macOS/Metal build are unaffected. This is purely
  additive.
- **No API, model, or format change.** The `EmbeddingBackend` trait, the `/v1/embed`
  endpoint, the Q8_0 GGUF format, and the bundled model all stay the same. Vectors from
  the CUDA path must match the CPU path within numerical tolerance (same weights).
- **Build-host requirement.** The CUDA release job needs the CUDA toolkit; this adds a
  toolchain install step (and build time for `candle-kernels`) to that one job only.
- **Distribution grows by one artifact under Outcome B.** An `-cuda` Linux artifact and
  installer detection logic. Under Outcome A, no new artifact — just a feature change on
  the existing Linux job.
- **Windows CUDA deferred.** Windows + MSVC + CUDA toolchain is more fragile in CI; it
  is a follow-up once the Linux pattern lands.
- **Generic cross-vendor GPU (AMD/Intel/Vulkan) explicitly deferred.** Not available in
  Candle; would require a framework change and is out of scope for this ADR.

## Verification

1. **Spike gate:** on Linux + NVIDIA, `cargo build --release -p spelunk-server
   --features rich-formats,cuda` succeeds; the server logs the CUDA device on startup;
   `nvidia-smi` shows utilisation during indexing. On a driver-less host, record
   fallback-vs-panic to select Outcome A or B.
2. **CPU regression:** default `cargo build -p spelunk-server` (no `cuda`) and the CPU
   Linux release artifact still build and pass `cargo test` (native embedder tests run
   on the Linux CI job).
3. **Parity:** CUDA-path embeddings match the CPU path within numerical tolerance on a
   small fixture set (same model, same Q8_0 weights).
4. **Release dry run:** the new/changed release job produces a downloadable Linux
   artifact; under Outcome B, `install.sh` selects the correct binary for a GPU vs
   non-GPU host.
