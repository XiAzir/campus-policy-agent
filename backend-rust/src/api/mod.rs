pub mod error;
pub mod handlers;

use crate::agent::Agent;
use crate::auth::{RateLimiter, TokenService};
use crate::chat::ChatManager;
use crate::config::Config;
use crate::db::DbPool;
use crate::maintenance::MaintenanceState;
use crate::vectors::VectorIndex;
use axum::Router;
use axum::extract::Request;
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post, put};
use serde_json::json;
use std::sync::Arc;
use tokio::sync::RwLock;
use tower_http::services::ServeDir;

#[derive(Clone)]
pub struct AppState {
    pub config: Config,
    pub db: DbPool,
    pub tokens: Arc<RwLock<TokenService>>,
    pub limiter: Arc<RateLimiter>,
    pub vectors: Arc<VectorIndex>,
    pub agent: Arc<Agent>,
    pub chats: Arc<ChatManager>,
    pub maintenance: Arc<MaintenanceState>,
}

async fn maintenance_middleware(
    axum::extract::State(state): axum::extract::State<AppState>,
    req: Request,
    next: Next,
) -> Response {
    if let Err(msg) = state.maintenance.enter_request() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(json!({ "detail": msg })),
        )
            .into_response();
    }

    let res = next.run(req).await;
    state.maintenance.exit_request();
    res
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
        .route("/chat", post(handlers::chat))
        .route("/chat/cancel", post(handlers::chat_cancel))
        .route(
            "/admin/packages",
            get(handlers::admin_packages_list).post(handlers::admin_package_upload),
        )
        .route(
            "/admin/packages/{package_id}",
            get(handlers::admin_package_preview).delete(handlers::admin_package_discard),
        )
        .route(
            "/admin/packages/{package_id}/documents/{doc_hash}",
            patch(handlers::admin_package_patch_meta),
        )
        .route(
            "/admin/packages/{package_id}/publish",
            post(handlers::admin_package_publish),
        )
        .route("/admin/documents", get(handlers::admin_documents))
        .route(
            "/admin/documents/{doc_uid}/versions",
            get(handlers::admin_versions),
        )
        .route(
            "/admin/documents/{doc_uid}/deactivate",
            post(handlers::admin_document_deactivate),
        )
        .route(
            "/admin/documents/{doc_uid}/enable",
            post(handlers::admin_document_enable),
        )
        .route(
            "/admin/documents/{doc_uid}/unlink",
            post(handlers::admin_document_unlink),
        )
        .route("/admin/access-code", put(handlers::admin_set_access_code))
        .route("/admin/status", get(handlers::admin_status))
        .route("/admin/metrics", get(handlers::admin_metrics))
        .route("/admin/backup", get(handlers::admin_backup))
        .route("/admin/restore", post(handlers::admin_restore))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            maintenance_middleware,
        ));

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
