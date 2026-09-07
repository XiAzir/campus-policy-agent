use crate::agent::TurnScope;
use crate::api::AppState;
use crate::api::error::{ApiError, ApiResult};
use crate::auth::{hash_password, verify_password};
use crate::chat::trim_history;
use crate::db::utcnow;
use crate::metrics::TurnMetrics;
use crate::retrieval::allowed_doc_ids;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::http::header::{CONTENT_DISPOSITION, CONTENT_TYPE, HeaderMap, HeaderValue};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;
use std::convert::Infallible;
use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use sysinfo::{Pid, ProcessesToUpdate, System};
use tokio_stream::StreamExt;
use tokio_stream::wrappers::ReceiverStream;

// ---------------- 鉴权提取辅助 ----------------

fn get_bearer_token(headers: &HeaderMap) -> Option<&str> {
    let auth = headers.get("Authorization")?.to_str().ok()?;
    auth.strip_prefix("Bearer ")
}

fn require_user(headers: &HeaderMap, state: &AppState) -> ApiResult<String> {
    let token = get_bearer_token(headers).ok_or_else(|| ApiError::unauthorized("未登录"))?;
    let tokens = state
        .tokens
        .try_read()
        .map_err(|_| ApiError::service_unavailable("令牌服务忙"))?;
    tokens
        .verify(token, "user")
        .ok_or_else(|| ApiError::unauthorized("登录已失效"))
}

fn require_admin(headers: &HeaderMap, state: &AppState) -> ApiResult<String> {
    let token =
        get_bearer_token(headers).ok_or_else(|| ApiError::unauthorized("未以管理员身份登录"))?;
    let tokens = state
        .tokens
        .try_read()
        .map_err(|_| ApiError::service_unavailable("令牌服务忙"))?;
    tokens
        .verify(token, "admin")
        .ok_or_else(|| ApiError::unauthorized("管理员登录已失效"))
}

fn get_client_ip(peer: Option<axum::Extension<axum::extract::ConnectInfo<std::net::SocketAddr>>>) -> String {
    peer.map(|p| p.0.0.ip().to_string()).unwrap_or_else(|| "unknown-peer".into())
}

// ---------------- 路由 Handlers ----------------

// GET /api/auth/state
pub async fn auth_state(State(state): State<AppState>) -> ApiResult<Json<serde_json::Value>> {
    let access_code_hash = state
        .db
        .setting_get("access_code_hash".to_string())
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;
    Ok(Json(
        json!({ "access_code_set": access_code_hash.is_some() }),
    ))
}

#[derive(Deserialize)]
pub struct LoginRequest {
    pub code: String,
}

// POST /api/auth/login
pub async fn login(
    State(state): State<AppState>,
    peer: Option<axum::Extension<axum::extract::ConnectInfo<std::net::SocketAddr>>>,
    Json(body): Json<LoginRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    let ip = get_client_ip(peer);
    let key = format!("login:{}", ip);
    if !state.limiter.hit(&key, 2.0, 10.0, 1.0) {
        return Err(ApiError::rate_limited("请求过于频繁，请稍后再试"));
    }

    let stored = state
        .db
        .setting_get("access_code_hash".to_string())
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;

    let Some(hash) = stored else {
        return Err(ApiError::bad_request(
            "访问码尚未设置，请联系管理员在管理端设置",
        ));
    };

    if !verify_password(&body.code, &hash) {
        let _ = state
            .db
            .audit("system".into(), "login_failed".into(), format!("ip={}", ip))
            .await;
        return Err(ApiError::unauthorized("访问码不正确"));
    }

    let client_id = hex::encode(rand::random::<[u8; 16]>());
    let token = {
        let tokens = state.tokens.read().await;
        tokens.issue("user", &client_id)
    };
    Ok(Json(json!({ "token": token, "client_id": client_id })))
}

#[derive(Deserialize)]
pub struct AdminLoginRequest {
    pub password: String,
}

// POST /api/auth/admin/login
pub async fn admin_login(
    State(state): State<AppState>,
    peer: Option<axum::Extension<axum::extract::ConnectInfo<std::net::SocketAddr>>>,
    Json(body): Json<AdminLoginRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    let ip = get_client_ip(peer);
    let key = format!("adminlogin:{}", ip);
    if !state.limiter.hit(&key, 2.0, 10.0, 1.0) {
        return Err(ApiError::rate_limited("请求过于频繁，请稍后再试"));
    }

    let stored = state
        .db
        .setting_get("admin_password_hash".to_string())
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;

    if !verify_password(&body.password, stored.as_deref().unwrap_or("")) {
        let _ = state
            .db
            .audit(
                "system".into(),
                "admin_login_failed".into(),
                format!("ip={}", ip),
            )
            .await;
        return Err(ApiError::unauthorized("管理员密码不正确"));
    }

    let client_id = hex::encode(rand::random::<[u8; 16]>());
    let token = {
        let tokens = state.tokens.read().await;
        tokens.issue("admin", &client_id)
    };
    Ok(Json(
        json!({ "admin_token": token, "client_id": client_id }),
    ))
}

#[derive(Deserialize)]
pub struct AdminPasswordRequest {
    pub old_password: String,
    pub new_password: String,
}

// POST /api/auth/admin/password
pub async fn admin_password(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<AdminPasswordRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    require_admin(&headers, &state)?;

    if body.new_password.len() < 8 || body.new_password.len() > 128 {
        return Err(ApiError::bad_request("新密码长度必须在 8-128 字符之间"));
    }

    let stored = state
        .db
        .setting_get("admin_password_hash".to_string())
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;

    if !verify_password(&body.old_password, stored.as_deref().unwrap_or("")) {
        return Err(ApiError::unauthorized("当前密码不正确"));
    }

    if body.old_password == body.new_password {
        return Err(ApiError::bad_request("新密码不能与当前密码相同"));
    }

    let new_hash = hash_password(&body.new_password, None);
    state
        .db
        .setting_set("admin_password_hash".to_string(), new_hash)
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;

    let _ = state
        .db
        .audit("admin".into(), "admin_password_reset".into(), "".into())
        .await;

    Ok(Json(json!({ "ok": true })))
}

// GET /api/catalog
pub async fn catalog(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<serde_json::Value>> {
    require_user(&headers, &state)?;
    let docs = state
        .db
        .get_catalog()
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;
    Ok(Json(json!({ "documents": docs })))
}

#[derive(Deserialize)]
pub struct SourceTextQuery {
    #[serde(default = "default_frm")]
    pub frm: i64,
    #[serde(default = "default_to")]
    pub to: i64,
}

fn default_frm() -> i64 {
    1
}
fn default_to() -> i64 {
    80
}

// GET /api/source/{doc_uid}/text
pub async fn source_text(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(doc_uid): Path<String>,
    Query(query): Query<SourceTextQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    require_user(&headers, &state)?;

    let doc = state
        .db
        .get_document_public(doc_uid.clone())
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?
        .ok_or_else(|| ApiError::not_found("资料不存在"))?;

    let frm = query.frm.max(1);
    let max_allowed_to = frm + 200;
    let clamped_to = query.to.max(frm).min(max_allowed_to).min(doc.line_count);

    let text_path = state
        .config
        .data_dir
        .join("text")
        .join(format!("{}.txt", doc.doc_hash));

    let file = File::open(&text_path).map_err(|_| ApiError::not_found("标准化原文缺失"))?;
    let reader = BufReader::new(file);

    let mut lines = Vec::new();
    for (line_no, line_res) in reader.lines().enumerate() {
        let current_line = (line_no + 1) as i64;
        if current_line > clamped_to {
            break;
        }
        if current_line >= frm {
            let content = line_res.unwrap_or_default();
            lines.push(format!("L{}: {}", current_line, content));
        }
    }

    Ok(Json(json!({
        "doc_uid": doc_uid,
        "title": doc.title,
        "doc_type": doc.doc_type,
        "line_start": frm,
        "line_end": clamped_to,
        "line_count": doc.line_count,
        "page": null,
        "section": doc.title,
        "deactivated_kind": doc.deactivated_kind,
        "lines": lines
    })))
}

// GET /api/source/{doc_uid}/file
pub async fn source_file(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(doc_uid): Path<String>,
) -> ApiResult<Response> {
    require_user(&headers, &state)?;

    let doc = state
        .db
        .get_document_public(doc_uid)
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?
        .ok_or_else(|| ApiError::not_found("资料不存在"))?;

    let ext = PathBuf::from(&doc.title)
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| format!(".{}", s.to_lowercase()))
        .unwrap_or_else(|| format!(".{}", doc.doc_type));

    let file_path = state
        .config
        .data_dir
        .join("files")
        .join(format!("{}{}", doc.doc_hash, ext));

    if !file_path.exists() {
        return Err(ApiError::not_found("原文件缺失"));
    }

    let file_bytes = std::fs::read(&file_path).map_err(|_| ApiError::internal("读取原文件失败"))?;

    let filename = format!("{}.{}", doc.title, doc.doc_type);
    let disposition = format!(
        "attachment; filename*=UTF-8''{}",
        urlencoding::encode(&filename)
    );

    let mut response = (StatusCode::OK, file_bytes).into_response();
    let resp_headers = response.headers_mut();
    resp_headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    resp_headers.insert(
        "X-Content-Type-Options",
        HeaderValue::from_static("nosniff"),
    );
    if let Ok(val) = HeaderValue::from_str(&disposition) {
        resp_headers.insert(CONTENT_DISPOSITION, val);
    }

    Ok(response)
}

// GET /api/source/{doc_uid}/versions
pub async fn source_versions(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(doc_uid): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    require_user(&headers, &state)?;

    let doc = state
        .db
        .get_document_public(doc_uid.clone())
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;
    if doc.is_none() {
        return Err(ApiError::not_found("资料不存在"));
    }

    let versions = state
        .db
        .get_version_history(doc_uid)
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;

    Ok(Json(json!({ "versions": versions })))
}

// ---------- 聊天（SSE 事件流） ----------

#[derive(Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub text: String,
}

#[derive(Deserialize, Default)]
pub struct ScopeSpec {
    #[serde(default = "default_scope_mode")]
    pub mode: String,
    #[serde(default)]
    pub domains: Vec<String>,
    #[serde(default)]
    pub doc_uids: Vec<String>,
    #[serde(default = "default_year_mode")]
    pub year_mode: String,
    #[serde(default)]
    pub expand_confirmed: bool,
}

fn default_scope_mode() -> String {
    "auto".to_string()
}
fn default_year_mode() -> String {
    "current".to_string()
}

#[derive(Deserialize)]
pub struct ChatBody {
    #[serde(default)]
    pub messages: Vec<ChatMessage>,
    pub question: String,
    #[serde(default)]
    pub scope: ScopeSpec,
    #[serde(default)]
    pub profile: HashMap<String, serde_json::Value>,
}

async fn build_turn_scope(
    db: &crate::db::DbPool,
    scope: &ScopeSpec,
) -> Result<(TurnScope, String), ApiError> {
    let mut note = String::new();
    if scope.mode == "files" && !scope.expand_confirmed {
        if scope.doc_uids.is_empty() {
            return Err(ApiError::bad_request("请至少选择一份文件"));
        }
        let allowed = allowed_doc_ids(db, &scope.year_mode, None, Some(&scope.doc_uids), None)
            .await
            .map_err(|e| ApiError::internal(e.to_string()))?;

        if allowed.is_empty() {
            return Err(ApiError::bad_request("指定的资料不存在或已停用"));
        }
        note = format!(
            "用户明确指定了 {} 份文件，严格限定在此范围内",
            allowed.len()
        );
        return Ok((
            TurnScope {
                strict_files: true,
                strict_doc_uids: scope.doc_uids.clone(),
                year_mode: scope.year_mode.clone(),
                ..Default::default()
            },
            note,
        ));
    }

    if scope.mode == "domains" && !scope.domains.is_empty() {
        note = format!("用户选择领域：{}", scope.domains.join("、"));
        if scope.expand_confirmed {
            note.push_str("（用户已确认可扩展到全部资料）");
        }
        return Ok((
            TurnScope {
                domains: if scope.expand_confirmed {
                    Vec::new()
                } else {
                    scope.domains.clone()
                },
                year_mode: scope.year_mode.clone(),
                ..Default::default()
            },
            note,
        ));
    }

    if scope.expand_confirmed {
        note = "用户已确认可扩展到全部资料".to_string();
    }

    Ok((
        TurnScope {
            year_mode: scope.year_mode.clone(),
            ..Default::default()
        },
        note,
    ))
}

// POST /api/chat
pub async fn chat(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<ChatBody>,
) -> ApiResult<Response> {
    let client_id = require_user(&headers, &state)?;

    if body.question.trim().is_empty() || body.question.chars().count() > 4000 {
        return Err(ApiError::bad_request("问题长度必须在 1-4000 字符之间"));
    }

    let key = format!("chat:{}", client_id);
    if !state.limiter.hit(&key, 30.0, 20.0, 1.0) {
        return Err(ApiError::rate_limited("请求过于频繁，请稍后再试"));
    }

    let (turn_scope, note) = build_turn_scope(&state.db, &body.scope).await?;

    let raw_history: Vec<serde_json::Value> = body
        .messages
        .into_iter()
        .map(|m| {
            json!({
                "role": m.role,
                "parts": [{ "text": m.text }]
            })
        })
        .collect();

    let history = trim_history(
        raw_history,
        state.config.chat_history_max_rounds,
        state.config.chat_history_max_chars,
    );

    let mut profile_map = HashMap::new();
    if let Some(c) = body.profile.get("college").and_then(|v| v.as_str()) {
        profile_map.insert("college".to_string(), c.chars().take(60).collect());
    }
    if let Some(y) = body.profile.get("entry_year").and_then(|v| v.as_str()) {
        profile_map.insert("entry_year".to_string(), y.chars().take(20).collect());
    }
    profile_map.insert("scope_note".to_string(), note);

    let agent_clone = Arc::clone(&state.agent);
    let question = body.question;

    let runner = move |_job: Arc<crate::chat::ChatJob>,
                       tx: tokio::sync::mpsc::Sender<serde_json::Value>| {
        let agent = agent_clone;
        async move {
            let metrics = TurnMetrics::new();
            let emit_tx = tx.clone();

            let run_res = agent
                .run(
                    history,
                    &question,
                    turn_scope,
                    profile_map,
                    &metrics,
                    emit_tx,
                )
                .await;

            match run_res {
                Ok(result) => {
                    if result.expand_request.is_none() {
                        let _ = tx
                            .send(json!({
                                "event": "citations",
                                "citations": result.citations
                            }))
                            .await;
                        let _ = tx
                            .send(json!({
                                "event": "done",
                                "text": result.text,
                                "interrupted": false
                            }))
                            .await;
                    }
                }
                Err(e) => {
                    let _ = tx.send(metrics.to_public_json()).await;
                    let _ = tx
                        .send(json!({
                            "event": "error",
                            "message": format!("服务内部错误：{}", e)
                        }))
                        .await;
                }
            }
        }
    };

    let (_job, rx) = state
        .chats
        .submit(
            client_id.clone(),
            state.config.chat_request_timeout_s,
            runner,
        )
        .await
        .map_err(|e| ApiError::rate_limited(e.to_string()))?;

    let stream = ReceiverStream::new(rx).take_while(|val| val.get("event").and_then(|v| v.as_str()) != Some("__end__")).filter_map(|val| {
        if val.get("event").and_then(|v| v.as_str()) == Some("__end__") {
            None
        } else {
            let json_str = serde_json::to_string(&val).unwrap_or_default();
            Some(Ok::<_, Infallible>(Event::default().data(json_str)))
        }
    });

    let sse = Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text(": keepalive"),
    );

    let mut response = sse.into_response();
    let resp_headers = response.headers_mut();
    resp_headers.insert("Cache-Control", HeaderValue::from_static("no-cache"));
    resp_headers.insert("X-Accel-Buffering", HeaderValue::from_static("no"));

    Ok(response)
}

#[derive(Deserialize)]
pub struct CancelBody {
    pub request_id: String,
}

// POST /api/chat/cancel
pub async fn chat_cancel(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<CancelBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let client_id = require_user(&headers, &state)?;
    let ok = state.chats.cancel(&body.request_id, &client_id).await;
    Ok(Json(json!({ "ok": ok })))
}

// GET /api/admin/documents
pub async fn admin_documents(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<serde_json::Value>> {
    require_admin(&headers, &state)?;
    let docs = state
        .db
        .get_admin_documents()
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;
    Ok(Json(json!({ "documents": docs })))
}

// GET /api/admin/documents/{doc_uid}/versions
pub async fn admin_versions(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(doc_uid): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    require_admin(&headers, &state)?;

    let doc = state
        .db
        .get_document_public(doc_uid.clone())
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;
    if doc.is_none() {
        return Err(ApiError::not_found("资料不存在"));
    }

    let versions = state
        .db
        .get_version_history(doc_uid)
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;

    Ok(Json(json!({ "versions": versions })))
}

#[derive(Deserialize)]
pub struct SetAccessCodeRequest {
    pub code: String,
}

// PUT /api/admin/access-code
pub async fn admin_set_access_code(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<SetAccessCodeRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    require_admin(&headers, &state)?;

    if body.code.len() < 4 || body.code.len() > 128 {
        return Err(ApiError::bad_request("访问码长度必须在 4-128 字符之间"));
    }

    let hash = hash_password(&body.code, None);
    state
        .db
        .setting_set("access_code_hash".to_string(), hash)
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;

    let _ = state
        .db
        .audit("admin".into(), "access_code_set".into(), "".into())
        .await;

    Ok(Json(json!({ "ok": true })))
}

// GET /api/admin/status
pub async fn admin_status(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<serde_json::Value>> {
    require_admin(&headers, &state)?;

    let counts = state
        .db
        .get_counts()
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;

    let data_dir = state.config.data_dir.clone();
    let (data_bytes, (free_bytes, total_bytes)) = diagnostic_job(move || {
        Ok((crate::storage::directory_bytes(&data_dir)?, crate::storage::disk_space(&data_dir)?))
    }).await?;
    let free_gb = (free_bytes as f64) / 1_000_000_000.0;
    let total_gb = (total_bytes as f64) / 1_000_000_000.0;
    let used_gb = (total_gb - free_gb).max(0.0);

    let mut warnings = Vec::new();
    if (data_bytes as f64) >= state.config.disk_warn_gb * 1e9 {
        warnings.push(format!(
            "项目数据已达到 {}GB 告警阈值，请检查容量并清理不再需要的草稿",
            state.config.disk_warn_gb
        ));
    }
    if free_bytes < 128 * 1024 * 1024 {
        warnings.push("磁盘可用空间不足安全余量，资料处理将被拒绝".to_string());
    }

    Ok(Json(json!({
        "disk": {
            "total_gb": (total_gb * 100.0).round() / 100.0,
            "used_gb": (used_gb * 100.0).round() / 100.0,
            "free_gb": (free_gb * 100.0).round() / 100.0,
            "warn_gb": state.config.disk_warn_gb,
        },
        "counts": {
            "documents": counts.documents,
            "current": counts.current,
            "chunks": counts.chunks,
            "packages": counts.packages,
        },
        "data_dir_mb": ((data_bytes as f64) / 1_000_000.0 * 10.0).round() / 10.0,
        "warnings": warnings
    })))
}

// GET /api/admin/metrics
pub async fn admin_metrics(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<serde_json::Value>> {
    require_admin(&headers, &state)?;

    let data_dir = state.config.data_dir.clone();
    let (rss_bytes, cpu_s, disk_free) = diagnostic_job(move || {
        let mut sys = System::new();
        let pid = Pid::from_u32(std::process::id());
        sys.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
        let rss = sys.process(pid).map(|p| p.memory());
        Ok((rss, crate::storage::cpu_seconds(), crate::storage::disk_space(&data_dir)?.0))
    }).await?;

    let (running, waiting) = state.chats.counts().await;

    Ok(Json(json!({
        "at": utcnow(),
        "rss_bytes": rss_bytes,
        "cpu_s": cpu_s,
        "disk_free_bytes": disk_free,
        "running": running,
        "waiting": waiting
    })))
}

// ---------- 管理端：资料包与文件管理 ----------

// GET /api/admin/packages
pub async fn admin_packages_list(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<serde_json::Value>> {
    require_admin(&headers, &state)?;

    let pkgs = state
        .db
        .read(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, sha256, original_filename, size, imported_at, status, doc_count, chunk_count, embed_model, embed_dim \
                 FROM packages ORDER BY id DESC",
            )?;
            let mut rows = stmt.query([])?;
            let mut list = Vec::new();
            while let Some(r) = rows.next()? {
                list.push(json!({
                    "id": r.get::<_, i64>(0)?,
                    "sha256": r.get::<_, String>(1)?,
                    "original_filename": r.get::<_, String>(2)?,
                    "size": r.get::<_, i64>(3)?,
                    "imported_at": r.get::<_, String>(4)?,
                    "status": r.get::<_, String>(5)?,
                    "doc_count": r.get::<_, i64>(6)?,
                    "chunk_count": r.get::<_, i64>(7)?,
                    "embed_model": r.get::<_, String>(8)?,
                    "embed_dim": r.get::<_, i64>(9)?
                }));
            }
            Ok(list)
        })
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;

    Ok(Json(json!({ "packages": pkgs })))
}

// GET /api/admin/packages/{package_id}
pub async fn admin_package_preview(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(package_id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    require_admin(&headers, &state)?;

    let preview = crate::ingest::preview_package(&state.db, &state.config, package_id)
        .await
        .map_err(|e| ApiError::bad_request(e.to_string()))?;

    let Some(val) = preview else {
        return Err(ApiError::not_found("资料包不存在"));
    };

    Ok(Json(val))
}

// POST /api/admin/packages
pub async fn admin_package_upload(
    State(state): State<AppState>,
    headers: HeaderMap,
    mut multipart: axum::extract::Multipart,
) -> ApiResult<Json<serde_json::Value>> {
    require_admin(&headers, &state)?;

    let key = "pkgupload".to_string();
    if !state.limiter.hit(&key, 6.0, 5.0, 1.0) {
        return Err(ApiError::rate_limited("请求过于频繁，请稍后再试"));
    }

    let mut original_filename = "package.zip".to_string();
    let parent = state
        .config
        .data_dir
        .parent()
        .unwrap_or(&state.config.data_dir);
    let tmp_upload = tempfile::Builder::new().prefix("cpb-upload-").suffix(".zip")
        .tempfile_in(parent).map_err(|e| ApiError::internal(e.to_string()))?;
    let tmp_zip = tmp_upload.path().to_path_buf();

    let mut total_bytes = 0u64;
    let max_bytes = (state.config.max_package_mb as u64) * 1024 * 1024;
    let mut file_saved = false;

    while let Some(mut field) = multipart.next_field().await.map_err(multipart_error)? {
        let name = field.name().unwrap_or("").to_string();
        if name == "file" {
            if file_saved { return Err(ApiError::bad_request("只能上传一个文件")); }
            if let Some(fname) = field.file_name() {
                original_filename = fname.to_string();
            }

            let mut out_file =
                File::create(&tmp_zip).map_err(|e| ApiError::internal(e.to_string()))?;
            while let Some(chunk) = field.chunk().await.map_err(multipart_error)? {
                total_bytes += chunk.len() as u64;
                if total_bytes > max_bytes {
                    let _ = std::fs::remove_file(&tmp_zip);
                    return Err(ApiError::new(
                        StatusCode::PAYLOAD_TOO_LARGE,
                        format!(
                            "资料包超过单包上限 {}MB，请分包导入",
                            state.config.max_package_mb
                        ),
                    ));
                }
                crate::storage::require_space(parent, chunk.len() as u64).map_err(ApiError::bad_request)?;
                std::io::Write::write_all(&mut out_file, &chunk)
                    .map_err(|e| ApiError::internal(e.to_string()))?;
            }
            file_saved = true;
        } else {
            while field.chunk().await.map_err(multipart_error)?.is_some() {}
        }
    }

    if !file_saved {
        let _ = std::fs::remove_file(&tmp_zip);
        return Err(ApiError::bad_request("缺少上传文件"));
    }

    let import_res =
        crate::ingest::import_package(&state.db, &state.config, &tmp_zip, &original_filename).await;
    let _ = std::fs::remove_file(&tmp_zip);

    match import_res {
        Ok(id) => Ok(Json(json!({ "id": id }))),
        Err(e) => Err(ApiError::bad_request(e.to_string())),
    }
}

#[derive(Deserialize)]
pub struct PatchMetaRequest {
    pub fields: HashMap<String, serde_json::Value>,
}

// PATCH /api/admin/packages/{package_id}/documents/{doc_hash}
pub async fn admin_package_patch_meta(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((package_id, doc_hash)): Path<(i64, String)>,
    Json(body): Json<PatchMetaRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    require_admin(&headers, &state)?;

    crate::ingest::apply_override(&state.db, &state.config, package_id, &doc_hash, body.fields)
        .await
        .map_err(|e| ApiError::bad_request(e.to_string()))?;

    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize, Default)]
pub struct PublishPackageRequest {
    #[serde(default)]
    pub replacements: HashMap<String, Option<String>>,
}

// POST /api/admin/packages/{package_id}/publish
pub async fn admin_package_publish(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(package_id): Path<i64>,
    Json(body): Json<PublishPackageRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    require_admin(&headers, &state)?;

    crate::ingest::publish_package(&state.db, &state.config, package_id, body.replacements)
        .await
        .map_err(|e| ApiError::bad_request(e.to_string()))?;

    Ok(Json(json!({ "ok": true })))
}

// DELETE /api/admin/packages/{package_id}
pub async fn admin_package_discard(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(package_id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    require_admin(&headers, &state)?;

    crate::ingest::discard_package(&state.db, &state.config, package_id)
        .await
        .map_err(|e| ApiError::bad_request(e.to_string()))?;

    Ok(Json(json!({ "ok": true })))
}

// POST /api/admin/documents/{doc_uid}/deactivate
pub async fn admin_document_deactivate(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(doc_uid): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    require_admin(&headers, &state)?;

    crate::ingest::deactivate_document(&state.db, &doc_uid)
        .await
        .map_err(|e| ApiError::bad_request(e.to_string()))?;

    Ok(Json(json!({ "ok": true })))
}

// POST /api/admin/documents/{doc_uid}/enable
pub async fn admin_document_enable(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(doc_uid): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    require_admin(&headers, &state)?;

    crate::ingest::enable_document(&state.db, &doc_uid)
        .await
        .map_err(|e| ApiError::bad_request(e.to_string()))?;

    Ok(Json(json!({ "ok": true })))
}

// POST /api/admin/documents/{doc_uid}/unlink
pub async fn admin_document_unlink(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(doc_uid): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    require_admin(&headers, &state)?;

    crate::ingest::unlink_replacement(&state.db, &doc_uid)
        .await
        .map_err(|e| ApiError::bad_request(e.to_string()))?;

    Ok(Json(json!({ "ok": true })))
}

// GET /api/admin/backup
pub async fn admin_backup(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    require_admin(&headers, &state)?;

    let parent = state
        .config
        .data_dir
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."));
    let tmp_zip = tempfile::Builder::new()
        .prefix("cpb-dl-")
        .suffix(".zip")
        .tempfile_in(parent)
        .map_err(|e| ApiError::internal(format!("创建备份临时文件失败: {}", e)))?;
    let out_path = tmp_zip.path().to_path_buf();

    crate::backup::create_backup(&state.db, &state.config, &out_path)
        .await
        .map_err(|e| ApiError::bad_request(format!("备份失败：{}", e)))?;

    let _ = state
        .db
        .audit(
            "admin".to_string(),
            "backup_download".to_string(),
            "".to_string(),
        )
        .await;

    let file = tokio::fs::File::open(&out_path).await
        .map_err(|e| ApiError::internal(format!("读取备份结果失败: {}", e)))?;
    let stream = futures_util::stream::try_unfold((file, tmp_zip), |(mut file, temp)| async move {
        use tokio::io::AsyncReadExt;
        let mut bytes = vec![0u8; 64 * 1024];
        let count = file.read(&mut bytes).await?;
        if count == 0 { return Ok::<_, std::io::Error>(None); }
        bytes.truncate(count);
        Ok(Some((bytes, (file, temp))))
    });

    let ts = utcnow().replace(':', "");
    let filename = format!("backup-{}.zip", ts);
    let disposition = format!("attachment; filename=\"{}\"", filename);

    let mut response = axum::body::Body::from_stream(stream).into_response();
    let resp_headers = response.headers_mut();
    resp_headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/zip"));
    if let Ok(val) = HeaderValue::from_str(&disposition) {
        resp_headers.insert(CONTENT_DISPOSITION, val);
    }

    Ok(response)
}

// POST /api/admin/restore
pub async fn admin_restore(
    State(state): State<AppState>,
    headers: HeaderMap,
    mut multipart: axum::extract::Multipart,
) -> ApiResult<Json<serde_json::Value>> {
    require_admin(&headers, &state)?;

    let parent = state
        .config
        .data_dir
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."));
    let tmp_upload = tempfile::Builder::new()
        .prefix("cpb-restore-upload-")
        .suffix(".zip")
        .tempfile_in(parent)
        .map_err(|e| ApiError::internal(format!("创建恢复临时文件失败: {}", e)))?;
    let upload_path = tmp_upload.path().to_path_buf();

    let mut found_file = false;
    let mut total_bytes = 0u64;

    while let Some(mut field) = multipart
        .next_field()
        .await
        .map_err(multipart_error)?
    {
        if field.name() == Some("file") {
            if found_file { return Err(ApiError::bad_request("只能上传一个文件")); }
            let mut out = File::create(&upload_path)
                .map_err(|e| ApiError::internal(format!("写入上传文件失败: {}", e)))?;
            while let Some(chunk) = field
                .chunk()
                .await
                .map_err(multipart_error)?
            {
                total_bytes += chunk.len() as u64;
                if total_bytes > crate::backup::MAX_EXPANDED {
                    return Err(ApiError::payload_too_large("备份上传大小超限"));
                }
                crate::storage::require_space(parent, chunk.len() as u64).map_err(ApiError::bad_request)?;
                out.write_all(&chunk)
                    .map_err(|e| ApiError::internal(format!("写入分块失败: {}", e)))?;
            }
            out.flush()
                .map_err(|e| ApiError::internal(format!("刷新上传文件失败: {}", e)))?;
            found_file = true;
        } else {
            while field.chunk().await.map_err(multipart_error)?.is_some() {}
        }
    }

    if !found_file {
        return Err(ApiError::bad_request("缺少名为 file 的备份文件字段"));
    }

    // 1. 验证 ZIP 与解压到私有暂存目录
    let unpack_dir = tempfile::Builder::new()
        .prefix("cpb-restore-")
        .tempdir_in(parent)
        .map_err(|e| ApiError::internal(format!("创建恢复暂存目录失败: {}", e)))?;
    let root = unpack_dir.path();

    let f = File::open(&upload_path)
        .map_err(|e| ApiError::internal(format!("打开上传文件失败: {}", e)))?;
    let (meta, total_uncompressed) = crate::backup::inspect_backup_archive(f)
        .map_err(|e| ApiError::bad_request(e.to_string()))?;

    crate::storage::require_space(parent, total_uncompressed)
        .map_err(|e| ApiError::bad_request(format!("磁盘空间不足: {}", e)))?;

    // 解压各文件并校验哈希
    {
        let f = File::open(&upload_path)
            .map_err(|e| ApiError::internal(format!("打开上传文件失败: {}", e)))?;
        let mut zip = zip::ZipArchive::new(f)
            .map_err(|e| ApiError::bad_request(format!("读取 ZIP 失败: {}", e)))?;
        for (rel_name, record) in &meta.files {
            let mut zfile = zip
                .by_name(rel_name)
                .map_err(|e| ApiError::bad_request(format!("ZIP 缺少条目 {}: {}", rel_name, e)))?;
            let dest = root.join(rel_name);
            if let Some(p) = dest.parent() {
                std::fs::create_dir_all(p)
                    .map_err(|e| ApiError::internal(format!("创建目录失败: {}", e)))?;
            }
            let mut out = File::create(&dest)
                .map_err(|e| ApiError::internal(format!("创建目标文件失败: {}", e)))?;
            std::io::copy(&mut zfile, &mut out)
                .map_err(|e| ApiError::internal(format!("解压文件失败: {}", e)))?;
            out.flush()
                .map_err(|e| ApiError::internal(format!("刷新目标文件失败: {}", e)))?;
            let actual_hash = crate::storage::hash_file(&dest)
                .map_err(|e| ApiError::internal(format!("校验哈希失败: {}", e)))?;
            if actual_hash != record.sha256 {
                return Err(ApiError::bad_request(format!(
                    "备份文件哈希校验失败: {}",
                    rel_name
                )));
            }
        }
    }

    // 2. 深入校验已解压快照
    crate::backup::check_unpacked_snapshot(root, &state.config, &meta)
        .map_err(|e| ApiError::bad_request(e.to_string()))?;

    // 3. 进入维护模式
    if state.maintenance.restoring.compare_exchange(false, true, std::sync::atomic::Ordering::SeqCst,
        std::sync::atomic::Ordering::SeqCst).is_err() {
        return Err(ApiError::conflict("已有恢复任务正在进行"));
    }
    let _restore_guard = crate::maintenance::RestoreGuard(state.maintenance.clone());

    // 取消所有进行中问答
    state.chats.cancel_all().await;

    // 最多等待活动请求 30 秒
    if let Err(e) = state.maintenance.drain(30).await {
        state
            .maintenance
            .restoring
            .store(false, std::sync::atomic::Ordering::SeqCst);
        return Err(ApiError::conflict(e));
    }

    // 4. 原子安装与回滚保护
    let vectors_clone = Arc::clone(&state.vectors);
    let install_res =
        crate::backup::install_backup(&state.db, &state.config, root, &meta, move || {
            vectors_clone.close_all();
        })
        .await;

    let summary = match install_res {
        Ok(counts) => {
            // 刷新 token_secret
            if let Ok(Some(sec)) = state.db.setting_get("token_secret".to_string()).await
                && let Ok(new_tokens) = crate::auth::TokenService::from_hex_secret(&sec, 30)
            {
                let mut lock = state.tokens.write().await;
                *lock = new_tokens;
            }
            let _ = state
                .db
                .audit(
                    "admin".to_string(),
                    "backup_restore".to_string(),
                    format!("docs={}", counts.documents),
                )
                .await;
            state
                .maintenance
                .restoring
                .store(false, std::sync::atomic::Ordering::SeqCst);
            counts
        }
        Err(e) => {
            if !crate::backup::restore_marker_path(&state.config.data_dir).exists() {
                return Err(ApiError::bad_request(format!("恢复失败，原资料库已保留: {}", e)));
            }
            state
                .maintenance
                .failed
                .store(true, std::sync::atomic::Ordering::SeqCst);
            return Err(ApiError::service_unavailable(format!(
                "恢复回滚失败，服务已锁定；请停止服务后按恢复故障步骤处理: {}",
                e
            )));
        }
    };

    let _ = std::fs::remove_file(&upload_path);

    Ok(Json(json!({
        "ok": true,
        "summary": {
            "documents": summary.documents,
            "chunks": summary.chunks
        }
    })))
}

pub async fn fallback_handler() -> Json<serde_json::Value> {
    Json(json!({
        "service": "campus-policy-agent-rust",
        "frontend": "尚未构建或处于开发模式"
    }))
}

fn multipart_error(error: axum::extract::multipart::MultipartError) -> ApiError {
    let status = if error.status() == StatusCode::PAYLOAD_TOO_LARGE
        || error.to_string().contains("超过上限") { StatusCode::PAYLOAD_TOO_LARGE }
        else { StatusCode::BAD_REQUEST };
    ApiError::new(status, "上传内容不完整、格式错误或超过请求体上限")
}

async fn diagnostic_job<T: Send + 'static>(work: impl FnOnce() -> std::io::Result<T> + Send + 'static) -> ApiResult<T> {
    static SLOTS: std::sync::OnceLock<Arc<tokio::sync::Semaphore>> = std::sync::OnceLock::new();
    let permit = SLOTS.get_or_init(|| Arc::new(tokio::sync::Semaphore::new(1))).clone()
        .try_acquire_owned().map_err(|_| ApiError::service_unavailable("指标采样忙，请稍后重试"))?;
    tokio::task::spawn_blocking(move || { let _permit = permit; work() }).await
        .map_err(|_| ApiError::internal("指标采样任务失败"))?
        .map_err(|_| ApiError::service_unavailable("无法读取系统或数据盘指标"))
}
