# Model attribution

spelunk-server bundles a native embedding model rather than calling an external
embedding endpoint. `cargo-about` (see `about.toml`) covers Rust dependency
licenses, but it does not cover the model weights downloaded at runtime, so they
are attributed here.

Looking for how to configure an external LLM or embedding endpoint instead?
See [Third-party models](third-party-models.md).

## F2LLM-v2-330M (embedder)

- **Model:** `codefuse-ai/F2LLM-v2-330M`
- **Upstream:** https://huggingface.co/codefuse-ai/F2LLM-v2-330M
- **Pinned source revision:** `1239cdd544b24c247ed75df2ae22e5a401ac4659`, the
  provenance anchor for the weights, tokenizer, and config redistributed in
  `spelunk-cloud/F2LLM-v2-330M-Q8_0-GGUF` (see below). Not used at runtime;
  update it (and regenerate/re-upload the artifacts) if the pin ever moves.
- **License:** Apache License 2.0 (declared via the upstream Hugging Face
  model-card license tag). Full text:
  https://www.apache.org/licenses/LICENSE-2.0
- **Use in spelunk:** loaded by `spelunk-server` as the 896-dim semantic
  embedding backend (Qwen3 decoder architecture, candle runtime).

### Modification notice (Apache-2.0 §4)

spelunk redistributes a **modified** copy of these weights: the original BF16
safetensors are **quantized to Q8_0** (projection matmuls and the token-embedding
table are stored Q8_0; RMSNorm weights are kept F32) and packaged as a single
GGUF file. No other changes are made to the weights.

spelunk fetches this pre-quantized Q8_0 GGUF, plus the unmodified upstream
`tokenizer.json`, from a Hugging Face repository it owns
(`spelunk-cloud/F2LLM-v2-330M-Q8_0-GGUF`); that artifact carries its own
`LICENSE`, `NOTICE`, and model card reproducing this attribution. `config.json`
(unmodified, ~1 KB) is vendored directly into the `spelunk-server` binary
(`crates/spelunk-server/assets/f2llm-v2-330m-config.json`) rather than fetched.
**None of the three artifacts (GGUF, tokenizer, config) are fetched from the
third-party upstream repo at runtime** (everything comes from our own
first-party repo or is embedded in the binary). Set `SPELUNK_EMBEDDER_GGUF_REPO`
to a different repo to fetch the GGUF and tokenizer from there instead (it must
host both files). See `docs/embedder-artifact/` for the text that accompanies
the distributed artifact.

### Other bundled inference dependencies

The candle runtime (`candle-core`, `candle-nn`, `candle-transformers`), the
Hugging Face hub client (`hf-hub`), and `tokenizers` are Rust crates and are
covered by `cargo-about` / `about.toml`; they are not re-listed here.
