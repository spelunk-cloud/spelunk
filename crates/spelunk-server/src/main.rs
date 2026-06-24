use std::{net::SocketAddr, path::PathBuf, sync::Arc};

use anyhow::{Context, Result};
use clap::Parser;
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

use spelunk_server::auth::ApiKeyAuth;
use spelunk_server::db::ServerDb;
use spelunk_server::rate_limiter::RateLimiter;
use spelunk_server::{ApiDoc, AppState, default_conflict_threshold, router};
use utoipa::OpenApi;

#[cfg(feature = "embed-native")]
mod embedder_native;

#[derive(Parser, Debug)]
#[command(name = "spelunk-server", about = "Shared memory server for spelunk")]
struct Args {
    /// Port to listen on
    #[arg(long, default_value = "7777")]
    port: u16,

    /// Host/address to bind
    #[arg(long, default_value = "0.0.0.0")]
    host: String,

    /// Path to the server SQLite database
    #[arg(long, default_value = "spelunk.db")]
    db: PathBuf,

    /// Shared API key (Bearer token). Leave unset to disable auth (dev only).
    #[arg(long, env = "SPELUNK_SERVER_KEY")]
    key: Option<String>,

    /// Embedding dimension expected from clients (must match the team's model).
    /// Default: 896 (F2LLM-v2-330M).
    #[arg(long, default_value = "896")]
    embedding_dim: usize,

    /// Cosine similarity threshold for conflict detection (0.0–1.0). New entries with
    /// similarity ≥ this value to an existing active entry trigger a 409 response.
    /// Set to 1.0 to disable conflict detection.
    #[arg(long, default_value_t = default_conflict_threshold())]
    conflict_threshold: f32,

    /// Base URL of an OpenAI-compatible embedding server for server-side embedding
    /// (e.g. `http://127.0.0.1:1234`). Overrides `SPELUNK_EMBEDDING_URL`.
    /// When set, entries posted without a pre-computed `embedding` field are embedded
    /// by the server before storage.
    #[arg(long, env = "SPELUNK_EMBEDDING_URL")]
    embedding_url: Option<String>,

    /// Embedding model name to pass to the embedding server (e.g.
    /// `text-embedding-embeddinggemma-300m-qat`). Overrides `SPELUNK_EMBEDDING_MODEL`.
    #[arg(long, env = "SPELUNK_EMBEDDING_MODEL", default_value = "")]
    embedding_model: String,

    /// Base URL of an OpenAI-compatible chat completions server for LLM features
    /// (`/explore`). Overrides `SPELUNK_LLM_URL`.
    #[arg(long, env = "SPELUNK_LLM_URL")]
    llm_url: Option<String>,

    /// LLM model name (e.g. `google/gemma-3n-e4b`). Overrides `SPELUNK_LLM_MODEL`.
    #[arg(long, env = "SPELUNK_LLM_MODEL", default_value = "")]
    llm_model: String,

    /// Print the OpenAPI spec as JSON and exit (for Postman / Newman import).
    #[arg(long)]
    print_openapi: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Register sqlite-vec extension for every connection in this process.
    #[allow(clippy::missing_transmute_annotations)]
    unsafe {
        rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute(
            sqlite_vec::sqlite3_vec_init as *const (),
        )));
    }

    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with(fmt::layer())
        .init();

    let args = Args::parse();

    if args.print_openapi {
        println!("{}", ApiDoc::openapi().to_pretty_json()?);
        return Ok(());
    }

    let db = ServerDb::open(&args.db, args.embedding_dim)
        .with_context(|| format!("opening server db at {}", args.db.display()))?;

    let instance_id = db
        .get_or_create_instance_id()
        .context("initialising instance_id")?;
    tracing::debug!("instance_id: {instance_id}");

    let started_by = effective_uid();

    if args.key.is_none() {
        tracing::warn!(
            "No API key configured — server is running without authentication. \
             Set --key or SPELUNK_SERVER_KEY for production use."
        );
    }

    // Build the auth provider from the configured key.
    let auth: Arc<dyn spelunk_server::auth::AuthProvider> =
        Arc::new(ApiKeyAuth::new(args.key.clone()));

    // Build the optional server-side embedder.
    let embedder: Option<Arc<dyn spelunk_core::embeddings::EmbeddingBackend>> = if let Some(
        base_url,
    ) =
        args.embedding_url
    {
        let model = if args.embedding_model.is_empty() {
            "default".to_string()
        } else {
            args.embedding_model.clone()
        };
        tracing::info!("server-side embedding enabled: {base_url} model={model}");
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .context("building HTTP client for server-side embedder")?;
        Some(Arc::new(ServerEmbedder {
            client,
            base_url,
            model,
        }))
    } else {
        // No --embedding-url: try the bundled native embedder (embed-native feature).
        #[cfg(feature = "embed-native")]
        {
            match embedder_native::NativeEmbedder::load() {
                Ok(native) => {
                    tracing::info!(
                        "native embedding model loaded (dim={})",
                        embedder_native::DIM
                    );
                    Some(Arc::new(native) as Arc<dyn spelunk_core::embeddings::EmbeddingBackend>)
                }
                Err(e) => {
                    tracing::warn!(
                        "native embedding model failed to load: {e}; \
                             running without embedder (set --embedding-url to override)"
                    );
                    None
                }
            }
        }
        #[cfg(not(feature = "embed-native"))]
        {
            None
        }
    };

    // Build the optional LLM backend.
    let llm: Option<Arc<dyn spelunk_core::llm::LlmBackend>> = if let Some(base_url) = args.llm_url {
        let model = if args.llm_model.is_empty() {
            "default".to_string()
        } else {
            args.llm_model.clone()
        };
        tracing::info!("server-side LLM enabled: {base_url} model={model}");
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(300))
            .build()
            .context("building HTTP client for server-side LLM")?;
        Some(Arc::new(ServerLlm {
            client,
            base_url,
            model,
        }))
    } else {
        None
    };

    // Server-side max_tokens ceiling: env var or 8192 default.
    let max_tokens_ceiling: usize = std::env::var("SPELUNK_MAX_TOKENS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8192);

    // Per-principal rate limiter: 60 requests per minute by default.
    let rate_limiter = Arc::new(RateLimiter::new(60, 60));

    let state = AppState {
        db: Arc::new(tokio::sync::Mutex::new(db)),
        auth,
        conflict_threshold: args.conflict_threshold,
        embedder,
        llm,
        max_tokens_ceiling,
        rate_limiter,
        instance_id,
        started_by,
    };

    let app = router(state);
    let addr: SocketAddr = format!("{}:{}", args.host, args.port)
        .parse()
        .context("parsing bind address")?;

    tracing::info!("spelunk-server listening on http://{addr}");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

// ── Effective UID helper ──────────────────────────────────────────────────────

/// Return the effective user ID of the current process (Unix), or `None` on Windows.
fn effective_uid() -> Option<u32> {
    #[cfg(unix)]
    {
        unsafe extern "C" {
            fn geteuid() -> u32;
        }
        Some(unsafe { geteuid() })
    }
    #[cfg(not(unix))]
    {
        None
    }
}

// ── Inline embedder for the server binary ─────────────────────────────────────

struct ServerEmbedder {
    client: reqwest::Client,
    base_url: String,
    model: String,
}

#[async_trait::async_trait]
impl spelunk_core::embeddings::EmbeddingBackend for ServerEmbedder {
    async fn embed(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
        #[derive(serde::Serialize)]
        struct Req<'a> {
            model: &'a str,
            input: &'a [&'a str],
        }
        #[derive(serde::Deserialize)]
        struct Resp {
            data: Vec<Data>,
        }
        #[derive(serde::Deserialize)]
        struct Data {
            embedding: Vec<f32>,
        }

        let resp: Resp = self
            .client
            .post(format!("{}/v1/embeddings", self.base_url))
            .json(&Req {
                model: &self.model,
                input: texts,
            })
            .send()
            .await
            .context("calling embedding server")?
            .error_for_status()
            .context("embedding server returned an error")?
            .json()
            .await
            .context("parsing embedding response")?;

        anyhow::ensure!(!resp.data.is_empty(), "embedding server returned 0 vectors");
        Ok(resp.data.into_iter().map(|d| d.embedding).collect())
    }

    fn dimension(&self) -> usize {
        0 // dimension is model-dependent; not used server-side
    }
}

// ── Inline LLM for the server binary ─────────────────────────────────────────

struct ServerLlm {
    client: reqwest::Client,
    base_url: String,
    model: String,
}

#[async_trait::async_trait]
impl spelunk_core::llm::LlmBackend for ServerLlm {
    async fn generate(
        &self,
        messages: &[spelunk_core::llm::Message],
        max_tokens: usize,
        tx: tokio::sync::mpsc::Sender<spelunk_core::llm::Token>,
        json_schema: Option<serde_json::Value>,
    ) -> anyhow::Result<()> {
        use futures_util::StreamExt;

        #[derive(serde::Serialize)]
        struct ChatReq<'a> {
            model: &'a str,
            messages: Vec<ChatMsg<'a>>,
            stream: bool,
            max_tokens: usize,
            temperature: f32,
            #[serde(skip_serializing_if = "Option::is_none")]
            response_format: Option<serde_json::Value>,
        }
        #[derive(serde::Serialize)]
        struct ChatMsg<'a> {
            role: &'a str,
            content: &'a str,
        }
        #[derive(serde::Deserialize)]
        struct StreamChunk {
            choices: Vec<StreamChoice>,
        }
        #[derive(serde::Deserialize)]
        struct StreamChoice {
            delta: Delta,
        }
        #[derive(serde::Deserialize)]
        struct Delta {
            content: Option<String>,
        }

        let chat_messages: Vec<ChatMsg> = messages
            .iter()
            .map(|m| ChatMsg {
                role: &m.role,
                content: &m.content,
            })
            .collect();

        let response_format =
            json_schema.map(|s| serde_json::json!({ "type": "json_schema", "json_schema": s }));

        let req = ChatReq {
            model: &self.model,
            messages: chat_messages,
            stream: true,
            max_tokens,
            temperature: 0.7,
            response_format,
        };

        let mut stream = self
            .client
            .post(format!("{}/v1/chat/completions", self.base_url))
            .json(&req)
            .send()
            .await
            .context("calling LLM server")?
            .error_for_status()
            .context("LLM server returned an error")?
            .bytes_stream();

        let mut buffer = String::new();
        while let Some(chunk) = stream.next().await {
            let bytes = chunk.context("reading SSE byte chunk")?;
            buffer.push_str(&String::from_utf8_lossy(&bytes));

            while let Some(pos) = buffer.find("\n\n") {
                let event = buffer[..pos].to_string();
                buffer.drain(..pos + 2);

                for line in event.lines() {
                    let data = match line.strip_prefix("data: ") {
                        Some(d) => d,
                        None => continue,
                    };
                    if data == "[DONE]" {
                        return Ok(());
                    }
                    if data.is_empty() {
                        continue;
                    }
                    if let Ok(chunk) = serde_json::from_str::<StreamChunk>(data) {
                        for choice in chunk.choices {
                            if let Some(content) = choice.delta.content
                                && !content.is_empty()
                                && tx.send(content).await.is_err()
                            {
                                return Ok(());
                            }
                        }
                    }
                }
            }
        }

        Ok(())
    }
}
