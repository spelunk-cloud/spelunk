// Integration-style test suite for the `handlers` module: builds a full
// `axum::Router` via `AppState`/`router(...)` and drives it with
// `tower::ServiceExt::oneshot` (or a real bound TCP listener for the
// timeout/concurrency themes). Split by theme; shared setup lives in
// `support`.

mod support;

mod batch_dedupe_tests;
mod batch_tests;
mod concurrency_tests;
mod embed_tests;
mod health_tests;
mod liveness_tests;
mod notes_tests;
mod search_explore_tests;
mod sync_tests;
mod timeout_tests;
