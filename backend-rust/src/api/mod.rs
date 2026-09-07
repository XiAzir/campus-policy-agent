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
    mut req: Request,
    next: Next,
) -> Response {
    if let Err(msg) = state.maintenance.enter_request() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(json!({ "detail": msg })),
        )
            .into_response();
    }

    let guard = crate::maintenance::RequestGuard(state.maintenance.clone());
    let path = req.uri().path().strip_prefix("/api").unwrap_or(req.uri().path());
    let storage_operation = path.starts_with("/admin/packages")
        || path.starts_with("/admin/documents")
        || path == "/admin/backup" || path == "/admin/restore";
    let limit = if path == "/admin/restore" { 10u64 * 1024 * 1024 * 1024 }
        else if path == "/admin/packages" && req.method() == axum::http::Method::POST { 210 * 1024 * 1024 }
        else { 256 * 1024 };
    if req.headers().get("content-length").and_then(|h| h.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok()).is_some_and(|n| n > limit) {
        return error::ApiError::payload_too_large("请求体超过上限").into_response();
    }
    use futures_util::StreamExt;
    let exceeded = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let over_limit = exceeded.clone();
    let body = std::mem::replace(req.body_mut(), axum::body::Body::empty());
    let stream = futures_util::stream::try_unfold((body.into_data_stream(), 0u64, over_limit), move |(mut stream, used, exceeded)| async move {
        match stream.next().await {
            Some(chunk) => {
                let chunk = chunk.map_err(std::io::Error::other)?;
                let used = used.saturating_add(chunk.len() as u64);
                if used > limit {
                    exceeded.store(true, std::sync::atomic::Ordering::Relaxed);
                    return Err(std::io::Error::other("请求体超过上限"));
                }
                Ok(Some((chunk, (stream, used, exceeded))))
            }
            None => Ok(None),
        }
    });
    *req.body_mut() = axum::body::Body::from_stream(stream);
    let response = if storage_operation {
        let Ok(permit) = state.maintenance.storage.clone().try_lock_owned() else {
            return error::ApiError::conflict("已有存储操作正在进行").into_response();
        };
        let runtime = tokio::runtime::Handle::current();
        // The worker owns both guards until work completes, even if the HTTP future is dropped.
        return match tokio::task::spawn_blocking(move || runtime.block_on(async move {
            let response = next.run(req).await;
            guarded_response(limit_response(response, &exceeded), (guard, permit))
        })).await {
            Ok(response) => response,
            Err(_) => error::ApiError::internal("存储工作任务异常退出").into_response(),
        };
    } else { next.run(req).await };
    guarded_response(limit_response(response, &exceeded), guard)
}

fn limit_response(response: Response, exceeded: &std::sync::atomic::AtomicBool) -> Response {
    if exceeded.load(std::sync::atomic::Ordering::Relaxed) {
        error::ApiError::payload_too_large("请求体超过上限").into_response()
    } else { response }
}

fn guarded_response<G: Send + 'static>(response: Response, guard: G) -> Response {
    use futures_util::StreamExt;
    let (parts, body) = response.into_parts();
    let stream = futures_util::stream::unfold((body.into_data_stream(), guard), |(mut stream, guard)| async move {
        stream.next().await.map(|chunk| (chunk, (stream, guard)))
    });
    Response::from_parts(parts, axum::body::Body::from_stream(stream))
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

    router.layer(axum::extract::DefaultBodyLimit::disable()).with_state(state)
}
