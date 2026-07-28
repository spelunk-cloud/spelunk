//! `spelunk memory watch` — stream live memory events from spelunk-server.
//!
//! Behaviour:
//! - Opens `GET /v1/projects/{id}/memory/stream` as a persistent SSE connection.
//! - Supports `--kind` to filter by event kind (passed as `?kind=` query param).
//! - Tracks the `id:` field from each SSE frame as `Last-Event-ID`.
//! - On transient error or server-closed connection, reconnects automatically
//!   (up to `--reconnect-limit` times, default 10) using the last seen ID so
//!   the server can replay missed events.
//! - `--since-seq <N>` seeds the initial `Last-Event-ID` so callers can
//!   resume from a checkpoint.

use anyhow::{Context, Result};
use futures_util::StreamExt;

use super::super::color::cprintln;
use super::MemoryWatchArgs;
use crate::{capability, config::Config};

/// Reconnect back-off: 1 s, 2 s, 4 s, … capped at 30 s.
fn backoff_secs(attempt: u32) -> u64 {
    let base: u64 = 1 << attempt.min(5); // 1, 2, 4, 8, 16, 32 → capped
    base.min(30)
}

pub(super) async fn memory_watch(args: MemoryWatchArgs, cfg: &Config) -> Result<()> {
    let tier = capability::get_tier(cfg).await;
    capability::require_tier1("memory watch", tier, cfg.server_url.as_deref())?;
    // `require_tier1` also passes for an auto-discovered loopback server
    // (inference-only, ADR-004) whose `server_url` is unset; watching a team
    // stream needs the explicit team server, so check for it separately.
    let base_url = cfg.server_url.as_deref().ok_or_else(|| {
        anyhow::anyhow!("`spelunk memory watch` requires `server_url` to be configured.")
    })?;
    let project_id = cfg.project_id.as_deref().ok_or_else(|| {
        anyhow::anyhow!(
            "`project_id` is not configured. \
             Set it in `.spelunk/config.toml` or via `SPELUNK_PROJECT_ID`."
        )
    })?;

    let is_json = matches!(crate::utils::effective_format(&args.format), "json");
    let reconnect_limit = args.reconnect_limit;

    // Seed Last-Event-ID from --since-seq if provided.
    let mut last_event_id: Option<String> = args.since_seq.clone().map(|s| {
        // Accept both "seq-NNNNNNN" and plain integers.
        if s.starts_with("seq-") {
            s
        } else {
            format!("seq-{s:0>7}")
        }
    });

    let mut attempt: u32 = 0;

    loop {
        let url = build_url(base_url, project_id, args.kind.as_deref());

        eprintln!(
            "Watching {url}{}— press Ctrl-C to stop.",
            if let Some(ref id) = last_event_id {
                format!(" (resuming from {id}) ")
            } else {
                " ".to_string()
            }
        );

        let client = spelunk_core::config::apply_server_ca(
            reqwest::Client::builder(),
            cfg.server_ca.as_deref().map(std::path::Path::new),
        )?
        .build()
        .context("building HTTP client for memory watch")?;
        let mut req = client.get(&url);
        let bearer = cfg.bearer_for(base_url)?;
        if let Some(key) = bearer.as_deref() {
            req = req.header("Authorization", format!("Bearer {key}"));
        }
        if let Some(ref id) = last_event_id {
            req = req.header("Last-Event-ID", id.as_str());
        }

        let resp = match req.send().await.context("connecting to /memory/stream") {
            Ok(r) => r,
            Err(e) => {
                if !should_reconnect(attempt, reconnect_limit) {
                    return Err(e);
                }
                eprintln!(
                    "Connection error: {e}  Retrying in {}s…",
                    backoff_secs(attempt)
                );
                tokio::time::sleep(std::time::Duration::from_secs(backoff_secs(attempt))).await;
                attempt += 1;
                continue;
            }
        };

        // Non-2xx: surface the error and stop — this is a config/auth problem.
        if let Err(e) = resp.error_for_status_ref() {
            return Err(e).context("server returned error for GET /memory/stream");
        }

        let mut stream = resp.bytes_stream();
        let mut buf = String::new();
        // Per-event accumulator (SSE frames can span multiple lines).
        let mut current_data: Option<String> = None;
        let mut current_id: Option<String> = None;

        let result: Result<()> = 'read: loop {
            match stream.next().await {
                None => {
                    // Server closed the connection gracefully.
                    eprintln!("Stream closed by server.");
                    break 'read Ok(());
                }
                Some(Err(e)) => {
                    break 'read Err(anyhow::anyhow!("Stream read error: {e}"));
                }
                Some(Ok(chunk)) => {
                    let text = String::from_utf8_lossy(&chunk);
                    buf.push_str(&text);

                    // Process complete lines.
                    while let Some(pos) = buf.find('\n') {
                        let raw_line = buf[..pos].to_string();
                        buf = buf[pos + 1..].to_string();
                        let line = raw_line.trim_end_matches('\r');

                        if line.is_empty() {
                            // Blank line dispatches the accumulated event.
                            if let Some(data) = current_data.take() {
                                if !data.is_empty() {
                                    // Promote pending id to last_event_id.
                                    if let Some(id) = current_id.take() {
                                        last_event_id = Some(id);
                                    }
                                    if is_json {
                                        if let Ok(v) =
                                            serde_json::from_str::<serde_json::Value>(&data)
                                        {
                                            println!("{}", serde_json::to_string_pretty(&v)?);
                                        } else {
                                            println!("{data}");
                                        }
                                    } else {
                                        print_sse_note(&data);
                                    }
                                }
                            } else {
                                // No data but maybe an id-only frame (keepalive ping).
                                if let Some(id) = current_id.take() {
                                    last_event_id = Some(id);
                                }
                            }
                        } else if let Some(data) = line.strip_prefix("data: ") {
                            // Accumulate multi-line data fields (concatenate with '\n').
                            match current_data {
                                Some(ref mut existing) => {
                                    existing.push('\n');
                                    existing.push_str(data);
                                }
                                None => current_data = Some(data.to_string()),
                            }
                        } else if let Some(id) = line.strip_prefix("id: ") {
                            current_id = Some(id.to_string());
                        }
                        // Ignore `event:` and `:` (comment/keepalive) lines.
                    }
                }
            }
        };

        match result {
            Ok(()) => {
                // Graceful close.  If reconnect_limit > 0 still reconnect;
                // the server may have cycled.
                if !should_reconnect(attempt, reconnect_limit) {
                    return Ok(());
                }
                eprintln!(
                    "Reconnecting in {}s… (attempt {}/{})",
                    backoff_secs(attempt),
                    attempt + 1,
                    reconnect_limit
                );
                tokio::time::sleep(std::time::Duration::from_secs(backoff_secs(attempt))).await;
                attempt += 1;
            }
            Err(e) => {
                if !should_reconnect(attempt, reconnect_limit) {
                    return Err(e);
                }
                eprintln!(
                    "Stream error: {e}  Reconnecting in {}s… (attempt {}/{})",
                    backoff_secs(attempt),
                    attempt + 1,
                    reconnect_limit
                );
                tokio::time::sleep(std::time::Duration::from_secs(backoff_secs(attempt))).await;
                attempt += 1;
            }
        }
    }
}

fn build_url(base_url: &str, project_id: &str, kind: Option<&str>) -> String {
    let base = format!(
        "{}/v1/projects/{}/memory/stream",
        base_url.trim_end_matches('/'),
        project_id,
    );
    match kind {
        Some(k) if !k.is_empty() => format!("{base}?kind={k}"),
        _ => base,
    }
}

fn should_reconnect(attempt: u32, limit: u32) -> bool {
    limit > 0 && attempt < limit
}

fn print_sse_note(data: &str) {
    // Try to deserialize the cloud-api MemoryEvent shape first, then fall back
    // to the legacy OSS server shape.
    if print_cloud_event(data) {
        return;
    }
    print_legacy_note(data);
}

fn print_cloud_event(data: &str) -> bool {
    #[derive(serde::Deserialize)]
    struct CloudEvent {
        event: String,
        entry_id: Option<String>,
        kind: Option<String>,
        title: Option<String>,
        seq: Option<i64>,
        created_at: Option<String>,
        last_seq: Option<i64>,
    }

    let Ok(e) = serde_json::from_str::<CloudEvent>(data) else {
        return false;
    };

    match e.event.as_str() {
        "ping" => {
            if let Some(seq) = e.last_seq {
                eprintln!("[ ping  last_seq={seq} ]");
            }
            true
        }
        "memory.created" => {
            let kind = e.kind.as_deref().unwrap_or("?");
            let title = e.title.as_deref().unwrap_or("(untitled)");
            let seq = e.seq.unwrap_or(0);
            cprintln!("\x1b[1mseq-{seq:07}\x1b[0m  \x1b[33m[{kind}]\x1b[0m  {title}",);
            if let Some(ts) = &e.created_at {
                cprintln!("     \x1b[2m{ts}\x1b[0m");
            }
            println!();
            true
        }
        "memory.archived" => {
            let id = e.entry_id.as_deref().unwrap_or("?");
            let seq = e.seq.unwrap_or(0);
            cprintln!("\x1b[2mseq-{seq:07}  archived {id}\x1b[0m");
            println!();
            true
        }
        "memory.conflict_detected" | "memory.conflict_resolved" => {
            cprintln!("\x1b[2m{data}\x1b[0m");
            true
        }
        "auth_error" => {
            eprintln!("\x1b[31mAuth error from server — key may have been revoked.\x1b[0m");
            true
        }
        _ => false,
    }
}

fn print_legacy_note(data: &str) {
    // Legacy spelunk-server (OSS) shape.
    #[derive(serde::Deserialize)]
    struct Slim {
        id: i64,
        kind: String,
        title: String,
        created_at: i64,
        #[serde(default)]
        tags: Vec<String>,
    }
    if let Ok(n) = serde_json::from_str::<Slim>(data) {
        cprintln!(
            "\x1b[1m#{id}\x1b[0m  \x1b[33m[{kind}]\x1b[0m  {title}",
            id = n.id,
            kind = n.kind,
            title = n.title,
        );
        cprintln!(
            "     \x1b[2m{}\x1b[0m",
            super::super::status::format_age(n.created_at)
        );
        if !n.tags.is_empty() {
            println!("     tags: {}", n.tags.join(", "));
        }
        println!();
    } else {
        println!("{data}");
    }
}
