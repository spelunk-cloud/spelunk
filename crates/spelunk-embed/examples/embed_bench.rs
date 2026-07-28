// Sequence-length perf sweep for the native embedder: no `spelunk` CLI or
// `spelunk-server`/HTTP in the path, calls `NativeEmbedder` directly.
//
// Device is chosen at compile time via the crate's `metal` feature (see
// `select_device` in `embedder_native.rs`): build without it for CPU, with
// `--features metal` for GPU. Run both and diff the tables to compare devices.
//
// x-axis is tokenizer-exact (real `tokenizer.json` output), not the `chars/4`
// estimate `spelunk-core` uses at index time, which carries ~±25% per-chunk
// error and produced non-monotonic artifacts in an earlier profiling pass.
//
// Usage:
//   cargo run --release -p spelunk-embed --example embed_bench -- \
//       --gguf <path> --tokenizer <path> --config <path> \
//       [--sizes 128,256,512,1024] [--batches 1,8] [--repeat 5] [--csv out.csv]

use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use spelunk_embed::{EmbeddingBackend, NativeEmbedder};
use tokenizers::Tokenizer;

/// Sequence lengths (tokens) to sweep by default. Covers the spike's plateau
/// region (256-512) plus enough range either side to see the curve bend.
const DEFAULT_SIZES: &[usize] = &[
    32, 64, 128, 192, 256, 320, 384, 448, 512, 640, 768, 896, 1024, 1280, 1536, 1792, 2048, 3072,
    4096,
];

/// Batch sizes to sweep by default: 1 (sequential) vs 8 (the CPU batching
/// path's sub-batch size, `EMBED_BATCH_SIZE` in `embedder_native.rs`).
const DEFAULT_BATCHES: &[usize] = &[1, 8];

/// Repeats per (size, batch) point; the reported latency is the median.
const DEFAULT_REPEAT: usize = 5;

/// This crate's own source as the synthetic corpus (no network fetch
/// needed); cycled in `corpus_token_ids` if a run needs more tokens than one
/// copy provides.
const CORPUS_SOURCES: &[&str] = &[
    include_str!("../src/lib.rs"),
    include_str!("../src/embedder_native.rs"),
    include_str!("../src/backend.rs"),
    include_str!("../src/error.rs"),
];

struct Args {
    gguf: PathBuf,
    tokenizer: PathBuf,
    config: PathBuf,
    sizes: Vec<usize>,
    batches: Vec<usize>,
    repeat: usize,
    csv: Option<PathBuf>,
}

fn print_usage() {
    eprintln!(
        "embed_bench – sequence-length perf sweep for the native embedder\n\n\
         Required:\n\
         \x20 --gguf <path>       Q8_0 GGUF weights\n\
         \x20 --tokenizer <path>  tokenizer.json\n\
         \x20 --config <path>     Qwen3 config.json\n\
         Optional:\n\
         \x20 --sizes a,b,c       token-count sweep points (default: {:?})\n\
         \x20 --batches a,b       batch sizes to sweep (default: {:?})\n\
         \x20 --repeat N          repeats per point, median reported (default: {})\n\
         \x20 --csv <path>        also write results as CSV\n",
        DEFAULT_SIZES, DEFAULT_BATCHES, DEFAULT_REPEAT
    );
}

fn parse_args() -> Result<Args> {
    let mut gguf = None;
    let mut tokenizer = None;
    let mut config = None;
    let mut sizes = None;
    let mut batches = None;
    let mut repeat = DEFAULT_REPEAT;
    let mut csv = None;

    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut next_path = |flag: &str| -> Result<PathBuf> {
            it.next()
                .map(PathBuf::from)
                .with_context(|| format!("{flag} requires a value"))
        };
        match arg.as_str() {
            "--gguf" => gguf = Some(next_path("--gguf")?),
            "--tokenizer" => tokenizer = Some(next_path("--tokenizer")?),
            "--config" => config = Some(next_path("--config")?),
            "--csv" => csv = Some(next_path("--csv")?),
            "--sizes" => {
                let raw = it.next().context("--sizes requires a value")?;
                sizes = Some(parse_usize_list(&raw)?);
            }
            "--batches" => {
                let raw = it.next().context("--batches requires a value")?;
                batches = Some(parse_usize_list(&raw)?);
            }
            "--repeat" => {
                let raw = it.next().context("--repeat requires a value")?;
                repeat = raw.parse().context("--repeat must be a positive integer")?;
            }
            "-h" | "--help" => {
                print_usage();
                std::process::exit(0);
            }
            other => bail!("unrecognised argument: {other}"),
        }
    }

    Ok(Args {
        gguf: gguf.context("--gguf is required")?,
        tokenizer: tokenizer.context("--tokenizer is required")?,
        config: config.context("--config is required")?,
        sizes: sizes.unwrap_or_else(|| DEFAULT_SIZES.to_vec()),
        batches: batches.unwrap_or_else(|| DEFAULT_BATCHES.to_vec()),
        repeat: repeat.max(1),
        csv,
    })
}

fn parse_usize_list(raw: &str) -> Result<Vec<usize>> {
    raw.split(',')
        .map(|s| {
            s.trim()
                .parse::<usize>()
                .context("expected a comma-separated integer list")
        })
        .collect()
}

/// One sweep measurement: a (target size, batch) point.
struct Row {
    /// Requested sweep point (label only; `actual_n` is what was embedded).
    target: usize,
    batch: usize,
    /// Mean tokenizer-exact tokens actually embedded per batch item: the
    /// real x-axis.
    actual_n: f64,
    median_ms: f64,
    tokens_per_sec: f64,
}

fn main() -> Result<()> {
    // Surface the embedder's own "loading ... on Metal/GPU (...)" / fallback
    // warning log without requiring the caller to export RUST_LOG themselves.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = parse_args()?;

    println!(
        "device: compiled with `metal` feature = {} (see the loader log line above for whether \
         it actually got a GPU device or fell back to CPU)",
        cfg!(feature = "metal")
    );

    let embedder = NativeEmbedder::load_from_path(&args.gguf, &args.tokenizer, &args.config)
        .context("loading NativeEmbedder")?;
    let tokenizer = Tokenizer::from_file(&args.tokenizer)
        .map_err(|e| anyhow::anyhow!("loading measurement tokenizer copy: {e}"))?;

    let max_needed =
        *args.sizes.iter().max().unwrap_or(&0) * (*args.batches.iter().max().unwrap_or(&1));
    // Clamped to >= 128 so the fixed warmup point below (n=128) always has
    // enough corpus to slice from, even for a sweep of only smaller sizes.
    let corpus_ids = corpus_token_ids(&tokenizer, max_needed.max(128))?;

    let rt = tokio::runtime::Runtime::new().context("building tokio runtime")?;

    // Warm up the allocator/BLAS pool/page faults before timing, or whichever
    // point runs first absorbs that one-time cost (this made n=128 look
    // slower than n=256 in an untimed dry run).
    eprintln!("warming up...");
    rt.block_on(sweep_point(&embedder, &tokenizer, &corpus_ids, 128, 1, 1))?;

    let mut rows = Vec::new();

    for &target in &args.sizes {
        for &batch in &args.batches {
            eprintln!(
                "sweeping n≈{target} batch={batch} ({} repeats)...",
                args.repeat
            );
            let row = rt.block_on(sweep_point(
                &embedder,
                &tokenizer,
                &corpus_ids,
                target,
                batch,
                args.repeat,
            ))?;
            rows.push(row);
        }
    }

    print_table(&rows);
    print_fit(&rows, 1);

    if let Some(path) = &args.csv {
        write_csv(path, &rows)?;
        println!("\nwrote {}", path.display());
    }

    Ok(())
}

/// Tokenizes the concatenated corpus (cycled to >= `min_tokens` ids,
/// `add_special_tokens=false`): the raw pool windows are sliced from.
fn corpus_token_ids(tokenizer: &Tokenizer, min_tokens: usize) -> Result<Vec<u32>> {
    let one_copy = CORPUS_SOURCES.concat();
    anyhow::ensure!(!one_copy.is_empty(), "embed_bench corpus sources are empty");

    let mut ids = Vec::new();
    while ids.len() < min_tokens.max(1) {
        let encoding = tokenizer
            .encode(one_copy.as_str(), false)
            .map_err(|e| anyhow::anyhow!("tokenizing embed_bench corpus: {e}"))?;
        ids.extend_from_slice(encoding.get_ids());
    }
    Ok(ids)
}

/// Slices `batch` circularly-wrapped windows from `corpus_ids`, decodes to
/// text, then re-measures the actual token count via the same
/// `add_special_tokens=true` encode `NativeEmbedder` uses internally:
/// decode/encode isn't perfectly bijective, so slice length isn't what
/// actually gets embedded.
fn build_batch_texts(
    tokenizer: &Tokenizer,
    corpus_ids: &[u32],
    target: usize,
    batch: usize,
) -> Result<(Vec<String>, Vec<usize>)> {
    anyhow::ensure!(!corpus_ids.is_empty(), "empty corpus token pool");
    let len = corpus_ids.len();
    // Spread windows across the pool so they're not all the same text: avoids
    // flattering any per-text cache, and represents a real mixed sub-batch.
    let spacing = (len / batch.max(1)).max(1);

    let mut texts = Vec::with_capacity(batch);
    let mut actual_ns = Vec::with_capacity(batch);
    for i in 0..batch {
        let start = (i * spacing) % len;
        let take = target.min(len);
        let slice: Vec<u32> = (0..take).map(|k| corpus_ids[(start + k) % len]).collect();
        let text = tokenizer
            .decode(&slice, true)
            .map_err(|e| anyhow::anyhow!("decoding corpus slice: {e}"))?;
        let actual_n = tokenizer
            .encode(text.as_str(), true)
            .map_err(|e| anyhow::anyhow!("measuring actual token count: {e}"))?
            .get_ids()
            .len();
        texts.push(text);
        actual_ns.push(actual_n);
    }
    Ok((texts, actual_ns))
}

async fn sweep_point(
    embedder: &NativeEmbedder,
    tokenizer: &Tokenizer,
    corpus_ids: &[u32],
    target: usize,
    batch: usize,
    repeat: usize,
) -> Result<Row> {
    let (texts, actual_ns) = build_batch_texts(tokenizer, corpus_ids, target, batch)?;
    let text_refs: Vec<&str> = texts.iter().map(String::as_str).collect();
    let total_actual_n: usize = actual_ns.iter().sum();
    let mean_actual_n = total_actual_n as f64 / batch as f64;

    let mut samples_ms = Vec::with_capacity(repeat);
    for _ in 0..repeat {
        let start = Instant::now();
        let out = embedder
            .embed(&text_refs)
            .await
            .with_context(|| format!("embed at n≈{target} batch={batch}"))?;
        let elapsed = start.elapsed();
        anyhow::ensure!(
            out.len() == batch,
            "embedder returned {} vectors for a batch of {batch}",
            out.len()
        );
        samples_ms.push(elapsed.as_secs_f64() * 1000.0);
    }

    let median_ms = median(&mut samples_ms);
    let tokens_per_sec = if median_ms > 0.0 {
        (total_actual_n as f64) / (median_ms / 1000.0)
    } else {
        f64::INFINITY
    };

    Ok(Row {
        target,
        batch,
        actual_n: mean_actual_n,
        median_ms,
        tokens_per_sec,
    })
}

fn median(samples: &mut [f64]) -> f64 {
    samples.sort_by(|a, b| a.partial_cmp(b).expect("no NaNs in timing samples"));
    let mid = samples.len() / 2;
    if samples.len().is_multiple_of(2) {
        (samples[mid - 1] + samples[mid]) / 2.0
    } else {
        samples[mid]
    }
}

fn print_table(rows: &[Row]) {
    println!(
        "\n{:>10} {:>7} {:>12} {:>12} {:>14}",
        "target_n", "batch", "actual_n", "median_ms", "tok/s"
    );
    for r in rows {
        println!(
            "{:>10} {:>7} {:>12.1} {:>12.3} {:>14.1}",
            r.target, r.batch, r.actual_n, r.median_ms, r.tokens_per_sec
        );
    }
}

/// Least-squares fit of `ms = a + b*n + c*n^2`, via 3x3 normal equations
/// solved by Cramer's rule (no linalg dependency needed for 3 unknowns).
fn print_fit(rows: &[Row], batch: usize) {
    let pts: Vec<(f64, f64)> = rows
        .iter()
        .filter(|r| r.batch == batch)
        .map(|r| (r.actual_n, r.median_ms))
        .collect();
    if pts.len() < 3 {
        println!("\n(need >= 3 batch={batch} points to fit a quadratic; skipping)");
        return;
    }

    // Normal equations for y = a + b*x + c*x^2: minimise sum((y - fit)^2).
    let n = pts.len() as f64;
    let (mut sx, mut sx2, mut sx3, mut sx4) = (0.0, 0.0, 0.0, 0.0);
    let (mut sy, mut sxy, mut sx2y) = (0.0, 0.0, 0.0);
    for &(x, y) in &pts {
        let x2 = x * x;
        sx += x;
        sx2 += x2;
        sx3 += x2 * x;
        sx4 += x2 * x2;
        sy += y;
        sxy += x * y;
        sx2y += x2 * y;
    }

    // | n   sx  sx2 | |a|   | sy   |
    // | sx  sx2 sx3 | |b| = | sxy  |
    // | sx2 sx3 sx4 | |c|   | sx2y |
    let m = [[n, sx, sx2], [sx, sx2, sx3], [sx2, sx3, sx4]];
    let v = [sy, sxy, sx2y];
    match solve_3x3(m, v) {
        Some([a, b, c]) => {
            println!(
                "\nfitted (batch={batch}, n={} points): ms = {a:.4} + {b:.6}*n + {c:.8}*n^2",
                pts.len()
            );
        }
        None => println!("\n(batch={batch} points are degenerate; quadratic fit skipped)"),
    }
}

/// Solve `m * x = v` for a 3x3 system via Cramer's rule. `None` if `m` is
/// (near-)singular (all sweep points collinear/degenerate in x).
fn solve_3x3(m: [[f64; 3]; 3], v: [f64; 3]) -> Option<[f64; 3]> {
    let det3 = |r: [[f64; 3]; 3]| -> f64 {
        r[0][0] * (r[1][1] * r[2][2] - r[1][2] * r[2][1])
            - r[0][1] * (r[1][0] * r[2][2] - r[1][2] * r[2][0])
            + r[0][2] * (r[1][0] * r[2][1] - r[1][1] * r[2][0])
    };
    let d = det3(m);
    if d.abs() < 1e-9 {
        return None;
    }
    let mut result = [0.0; 3];
    for col in 0..3 {
        let mut mc = m;
        for row in 0..3 {
            mc[row][col] = v[row];
        }
        result[col] = det3(mc) / d;
    }
    Some(result)
}

fn write_csv(path: &Path, rows: &[Row]) -> Result<()> {
    let mut out = String::from("target_n,batch,actual_n,median_ms,tokens_per_sec\n");
    for r in rows {
        out.push_str(&format!(
            "{},{},{:.2},{:.4},{:.2}\n",
            r.target, r.batch, r.actual_n, r.median_ms, r.tokens_per_sec
        ));
    }
    std::fs::write(path, out).with_context(|| format!("writing {}", path.display()))
}
