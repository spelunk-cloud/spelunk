# Third-party models

`spelunk-server` bundles a native embedding model, so semantic search works
with no external endpoint and no way to relocate it: the embedding model and
its compute path are both pinned product-wide. LLM-backed features are
different: the server has no LLM of its own, and proxies those calls to an
external OpenAI-compatible chat-completions endpoint that you configure. This
page covers wiring that up.

Looking for the bundled embedder's license and provenance instead? See
[Model attribution](model-attribution.md).

## Configuring an external LLM endpoint

Flags and environment variables (verified against `spelunk-server --help`,
v0.9.4):

| Flag | Env | Purpose |
|---|---|---|
| `--llm-url` | `SPELUNK_LLM_URL` | Base URL of an OpenAI-compatible chat-completions server (e.g. LM Studio, Ollama, vLLM). |
| `--llm-model` | `SPELUNK_LLM_MODEL` | Model name to send to that endpoint (e.g. `google/gemma-3n-e4b`). |

These are flags to **`spelunk-server`**, not CLI `config.toml` keys: the CLI
never talks to an LLM directly, only through the server. There is no
authentication option for the endpoint itself: the server sends unauthenticated
requests, so point it at a trusted local or internal endpoint, not a public one.

### What this unlocks

- **`spelunk explore`**: the interactive LLM reasoning loop (`/explore`).
- **`spelunk memory harvest`**: LLM-based decision extraction from commits and
  agent sessions.
- **`spelunk index` chunk summaries**: see the caveat below; this one needs an
  explicitly configured `server_url` in addition to the server having an LLM.

### Absence behavior

With no LLM configured on the server:

- `spelunk explore` and `spelunk memory harvest` fail with an actionable error
  naming `server_url` (or, if no server is reachable at all, pointing at
  `spelunk server start`).
- The server's own `/explore` and `/llm/complete` routes return `503` with
  `"This server has no LLM configured. Set SPELUNK_LLM_URL and SPELUNK_LLM_MODEL."`
- `spelunk index` prints `Skipping summaries (no server_url configured)` to
  stderr and continues; a missing LLM never fails an index run.

### Loopback (local dev) setup

Export the variables, then restart the auto-managed local daemon so the
new process inherits them (a daemon already running keeps its old
configuration until restarted):

```bash
export SPELUNK_LLM_URL="http://127.0.0.1:1234"   # your LM Studio / Ollama / vLLM endpoint
export SPELUNK_LLM_MODEL="your-chat-model-id"

spelunk server stop      # if one is already running
spelunk server start     # the new daemon inherits the variables above
```

`spelunk explore` and `spelunk memory harvest` now work against the
auto-discovered loopback server, no `config.toml` change needed, since both
commands fill in the loopback URL for you when no explicit `server_url` is set.

**Index-time summaries are the exception.** They are gated on an *explicitly
configured* `server_url`, not merely on a reachable server, so they stay off
even against an LLM-configured loopback daemon unless you also set:

```toml
# .spelunk/config.toml
server_url = "http://127.0.0.1:7777"
```

(A loopback `http://` value is allowed here; see
[Server setup → Client configuration](server-setup.md#client-configuration).)

### Team server setup

Pass the same two flags when you start the deployed `spelunk-server` (see
[Server setup](server-setup.md)):

```bash
spelunk-server --host 0.0.0.0 --port 7777 \
  --tls-cert /etc/spelunk/tls-cert --tls-key /etc/spelunk/tls-key \
  --llm-url http://llm-host:1234 --llm-model your-chat-model-id
```

Every client already sets an explicit `server_url` to reach a team server, so
`explore`, `memory harvest`, and index-time summaries are all unlocked with no
extra client-side configuration.

## Native embedder artifact source

Embeddings have no external endpoint or config: the model is pinned
product-wide to F2LLM-v2-330M at 896 dimensions, computed only by the bundled
native embedder. `SPELUNK_EMBEDDER_GGUF_REPO` points that *bundled native*
embedder at an alternate source for the same F2LLM-v2-330M GGUF and tokenizer
artifacts, not a different model. See [Model attribution](model-attribution.md).

## Running with no network access at all

`SPELUNK_EMBEDDER_GGUF_REPO` above still calls out over the network, just to a
different (self-hosted) source. For a host with no route out at all, not
even to an alternate source, `--model-dir` / `SPELUNK_MODEL_DIR` loads the
bundled native embedder from a directory you provision ahead of time instead
of fetching it from Hugging Face Hub. See [Server setup → Air-gapped /
no-egress install](server-setup.md#air-gapped--no-egress-install) for the
directory layout and the fetch-and-transfer procedure.

## Related

- [Server setup](server-setup.md): deploying `spelunk-server`, TLS, client configuration
- [Model attribution](model-attribution.md): license and provenance for the bundled embedder
- [Getting started](getting-started.md): the zero-setup local path
