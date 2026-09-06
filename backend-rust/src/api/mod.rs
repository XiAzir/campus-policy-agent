pub mod error;
pub mod handlers;

use crate::auth::{RateLimiter, TokenService};
use crate::config::Config;
use crate::db::DbPool;
use axum::Router;
use axum::routing::{get, post, put};
use std::sync::Arc;
use tower_http::services::ServeDir;

#[derive(Clone)]
pub struct AppState {
    pub config: Config,
    pub db: DbPool,
    pub tokens: TokenService,
    pub limiter: Arc<RateLimiter>,
}

pub fn create_router(state: AppState) -> Router {
    let api_routes = Router::new()
        .route("/auth/state", get(handlers::auth_state))
        .route("/auth/login", post(handlers::login))
        .route("/auth/admin/login", post(handlers::admin_login))
        .route("/auth/admin/password", post(handlers::admin_password))
        .route("/catalog", get(handlers::catalog))
        .route("/source/{doc_uid}/text", get(handlers::source_text))
        .route("/source/{doc_uid}/file", get(handlers::source_file))
        .route("/source/{doc_uid}/versions", get(handlers::source_versions))
        .route("/admin/documents", get(handlers::admin_documents))
        .route(
            "/admin/documents/{doc_uid}/versions",
            get(handlers::admin_versions),
        )
        .route("/admin/access-code", put(handlers::admin_set_access_code))
        .route("/admin/status", get(handlers::admin_status))
        .route("/admin/metrics", get(handlers::admin_metrics));

    let mut router = Router::new().nest("/api", api_routes);

    if let Some(dist) = &state.config.frontend_dist {
        if dist.exists() {
            router = router.fallback_service(ServeDir::new(dist));
        } else {
            router = router.fallback(handlers::fallback_handler);
        }
    } else {
        router = router.fallback(handlers::fallback_handler);
    }

    router.with_state(state)
}
