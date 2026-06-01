pub mod auth;
pub mod db;
pub mod handlers;
pub mod security;

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};
use serde::Serialize;
use utoipa::{OpenApi, ToSchema};

use auth::{AuthError, AuthProvider};
use db::ServerDb;

#[derive(Clone)]
pub struct AppState {
    pub db: Arc<tokio::sync::Mutex<ServerDb>>,
    /// Auth strategy — replaces the old `api_key: Option<String>` field.
    pub auth: Arc<dyn AuthProvider>,
    /// Cosine similarity threshold above which a new entry is flagged as conflicting (0.0–1.0).
    /// Default: 0.92. Set to 1.0 to disable conflict detection.
    pub conflict_threshold: f32,
    /// Optional server-side embedder. When set, the server embeds entries that arrive without
    /// a pre-computed vector. If absent, entries without a vector are stored without one
    /// (text search only).
    pub embedder: Option<Arc<dyn spelunk_core::embeddings::EmbeddingBackend>>,
    /// Optional LLM backend for `/explore`.
    pub llm: Option<Arc<dyn spelunk_core::llm::LlmBackend>>,
}

pub fn default_conflict_threshold() -> f32 {
    0.92
}

// ── OpenAPI spec ──────────────────────────────────────────────────────────────

#[derive(OpenApi)]
#[openapi(
    info(
        title = "spelunk-server",
        version = "0.1.0",
        description = "Shared memory server for spelunk. Stores decisions, requirements, \
                        and context for a team and serves them over HTTP.",
        contact(name = "spelunk", url = "https://github.com/spelunk-cloud/spelunk"),
        license(name = "MIT"),
    ),
    paths(
        handlers::health,
        handlers::list_projects,
        handlers::add_note,
        handlers::list_notes,
        handlers::get_note,
        handlers::search_notes,
        handlers::delete_note,
        handlers::archive_note,
        handlers::supersede_note,
        handlers::project_stats,
        handlers::harvested_shas,
        handlers::memory_since,
        handlers::memory_stream,
        handlers::index_embed,
        handlers::explore,
        handlers::llm_complete,
    ),
    components(schemas(
        handlers::AddNoteRequest,
        handlers::AddNoteResponse,
        handlers::ConflictEntry,
        handlers::ListQuery,
        handlers::SearchRequest,
        handlers::BoolResponse,
        handlers::CountResponse,
        handlers::SupersedeRequest,
        handlers::SinceQuery,
        handlers::StreamQuery,
        handlers::HealthResponse,
        handlers::EmbedRequest,
        handlers::EmbedChunkIn,
        handlers::EmbedResponse,
        handlers::EmbedChunkOut,
        handlers::ExploreRequest,
        handlers::ExploreContextChunk,
        handlers::LlmCompleteRequest,
        handlers::LlmMessage,
        ErrorBody,
        ErrorDetail,
        db::Project,
        db::ServerNote,
        db::ProjectStats,
    )),
    tags(
        (name = "health", description = "Liveness"),
        (name = "projects", description = "Project management"),
        (name = "memory", description = "Memory CRUD and semantic search"),
        (name = "index", description = "Code index / embedding"),
        (name = "inference", description = "LLM-powered code exploration"),
    ),
    security(
        ("bearer_auth" = [])
    ),
    modifiers(&SecurityAddon),
)]
pub struct ApiDoc;

struct SecurityAddon;

impl utoipa::Modify for SecurityAddon {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        let components = openapi.components.get_or_insert_with(Default::default);
        components.add_security_scheme(
            "bearer_auth",
            utoipa::openapi::security::SecurityScheme::Http(
                utoipa::openapi::security::HttpBuilder::new()
                    .scheme(utoipa::openapi::security::HttpAuthScheme::Bearer)
                    .bearer_format("API key")
                    .description(Some(
                        "Pass as `Authorization: Bearer <key>`. \
                         Not required when no key is configured on the server.",
                    ))
                    .build(),
            ),
        );
    }
}

// ── Router ────────────────────────────────────────────────────────────────────

/// Build the axum router with all routes.
pub fn router(state: AppState) -> Router {
    let protected = Router::new()
        .route("/v1/projects", get(handlers::list_projects))
        .route("/v1/projects/{project_id}/memory", post(handlers::add_note))
        .route(
            "/v1/projects/{project_id}/memory",
            get(handlers::list_notes),
        )
        .route(
            "/v1/projects/{project_id}/memory/search",
            post(handlers::search_notes),
        )
        .route(
            "/v1/projects/{project_id}/memory/harvested-shas",
            get(handlers::harvested_shas),
        )
        .route(
            "/v1/projects/{project_id}/memory/since",
            get(handlers::memory_since),
        )
        .route(
            "/v1/projects/{project_id}/memory/stream",
            get(handlers::memory_stream),
        )
        .route(
            "/v1/projects/{project_id}/memory/{note_id}",
            get(handlers::get_note),
        )
        .route(
            "/v1/projects/{project_id}/memory/{note_id}",
            delete(handlers::delete_note),
        )
        .route(
            "/v1/projects/{project_id}/memory/{note_id}/archive",
            post(handlers::archive_note),
        )
        .route(
            "/v1/projects/{project_id}/memory/{note_id}/supersede",
            post(handlers::supersede_note),
        )
        .route(
            "/v1/projects/{project_id}/stats",
            get(handlers::project_stats),
        )
        .route(
            "/v1/projects/{project_id}/index/embed",
            post(handlers::index_embed),
        )
        .route("/v1/projects/{project_id}/explore", post(handlers::explore))
        .route(
            "/v1/projects/{project_id}/llm/complete",
            post(handlers::llm_complete),
        )
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ));

    Router::new()
        .route("/v1/health", get(handlers::health))
        .route("/api-docs/openapi.json", get(openapi_spec))
        .merge(protected)
        .with_state(state)
}

// ── OpenAPI spec endpoint ─────────────────────────────────────────────────────

/// Serve the OpenAPI spec as JSON. Import into Postman via
/// `File → Import → Link` using the server URL + `/api-docs/openapi.json`.
async fn openapi_spec() -> impl IntoResponse {
    Json(ApiDoc::openapi())
}

// ── Auth middleware ───────────────────────────────────────────────────────────

/// Trait-driven auth middleware. Delegates to `AppState.auth`.
async fn auth_middleware(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Response {
    match state.auth.authenticate(request.headers()).await {
        Ok(ctx) => {
            request.extensions_mut().insert(ctx);
            next.run(request).await
        }
        Err(AuthError(msg)) => (
            StatusCode::UNAUTHORIZED,
            Json(ErrorBody::new("unauthorized", &msg)),
        )
            .into_response(),
    }
}

// ── Shared error body ─────────────────────────────────────────────────────────

/// Consistent JSON error body: `{"error": {"code": "...", "message": "..."}}`.
#[derive(Serialize, ToSchema)]
pub struct ErrorBody {
    pub error: ErrorDetail,
}

#[derive(Serialize, ToSchema)]
pub struct ErrorDetail {
    pub code: String,
    pub message: String,
}

impl ErrorBody {
    pub fn new(code: &str, message: &str) -> Self {
        Self {
            error: ErrorDetail {
                code: code.to_string(),
                message: message.to_string(),
            },
        }
    }
}

// ── Error type ────────────────────────────────────────────────────────────────

/// Map anyhow errors to HTTP responses using the standard error body format.
pub enum AppError {
    NotFound,
    BadRequest(String),
    ServiceUnavailable(String),
    Internal(anyhow::Error),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        match self {
            AppError::NotFound => (
                StatusCode::NOT_FOUND,
                Json(ErrorBody::new("not_found", "Not found")),
            )
                .into_response(),
            AppError::BadRequest(msg) => (
                StatusCode::BAD_REQUEST,
                Json(ErrorBody::new("bad_request", &msg)),
            )
                .into_response(),
            AppError::ServiceUnavailable(msg) => (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ErrorBody::new("service_unavailable", &msg)),
            )
                .into_response(),
            AppError::Internal(e) => {
                let msg = e.to_string();
                // Surface dimension-mismatch and other user-facing errors as 400.
                if msg.contains("mismatch") || msg.contains("required") {
                    (
                        StatusCode::BAD_REQUEST,
                        Json(ErrorBody::new("bad_request", &msg)),
                    )
                        .into_response()
                } else {
                    tracing::error!("internal error: {e:#}");
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(ErrorBody::new("internal_error", "Internal server error")),
                    )
                        .into_response()
                }
            }
        }
    }
}

impl<E: Into<anyhow::Error>> From<E> for AppError {
    fn from(e: E) -> Self {
        AppError::Internal(e.into())
    }
}

// ── OpenAPI snapshot test ─────────────────────────────────────────────────────

#[cfg(test)]
mod openapi_tests {
    use utoipa::OpenApi;

    /// Write the generated OpenAPI spec to `docs/openapi.json` so it can be
    /// committed as the reference snapshot.  Run with:
    ///   cargo test -p spelunk-server write_openapi_snapshot -- --nocapture
    #[test]
    fn write_openapi_snapshot() {
        let spec = super::ApiDoc::openapi()
            .to_pretty_json()
            .expect("serialise openapi");
        // Resolve path relative to the workspace root (two levels up from src/).
        let out = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(|p| p.parent())
            .expect("workspace root")
            .join("docs/openapi.json");
        std::fs::create_dir_all(out.parent().unwrap()).ok();
        std::fs::write(&out, &spec).expect("write docs/openapi.json");
        println!("Written: {}", out.display());
    }
}
