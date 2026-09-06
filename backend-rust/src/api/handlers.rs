use crate::api::AppState;
use crate::api::error::{ApiError, ApiResult};
use crate::auth::{hash_password, verify_password};
use crate::db::utcnow;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::http::header::{CONTENT_DISPOSITION, CONTENT_TYPE, HeaderMap, HeaderValue};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use sysinfo::{Pid, ProcessesToUpdate, System};

// ---------------- 鉴权提取辅助 ----------------

fn get_bearer_token(headers: &HeaderMap) -> Option<&str> {
    let auth = headers.get("Authorization")?.to_str().ok()?;
    auth.strip_prefix("Bearer ")
}

fn require_user(headers: &HeaderMap, state: &AppState) -> ApiResult<String> {
    let token = get_bearer_token(headers).ok_or_else(|| ApiError::unauthorized("未登录"))?;
    state
        .tokens
        .verify(token, "user")
        .ok_or_else(|| ApiError::unauthorized("登录已失效"))
}

fn require_admin(headers: &HeaderMap, state: &AppState) -> ApiResult<String> {
    let token =
        get_bearer_token(headers).ok_or_else(|| ApiError::unauthorized("未以管理员身份登录"))?;
    state
        .tokens
        .verify(token, "admin")
        .ok_or_else(|| ApiError::unauthorized("管理员登录已失效"))
}

fn get_client_ip(headers: &HeaderMap) -> String {
    headers
        .get("x-forwarded-for")
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.split(',').next())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "127.0.0.1".to_string())
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
    headers: HeaderMap,
    Json(body): Json<LoginRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    let ip = get_client_ip(&headers);
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
    let token = state.tokens.issue("user", &client_id);
    Ok(Json(json!({ "token": token, "client_id": client_id })))
}

#[derive(Deserialize)]
pub struct AdminLoginRequest {
    pub password: String,
}

// POST /api/auth/admin/login
pub async fn admin_login(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<AdminLoginRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    let ip = get_client_ip(&headers);
    let key = format!("adminlogin:{}", ip);
    if !state.limiter.hit(&key, 2.0, 10.0, 1.0) {
        return Err(ApiError::rate_limited("请求过于频繁，请稍后再试"));
    }

    let stored = state
        .db
        .setting_get("admin_password_hash".to_string())
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;

    let hash = stored.unwrap_or_default();
    if !verify_password(&body.password, &hash) {
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
    let token = state.tokens.issue("admin", &client_id);
    Ok(Json(
        json!({ "admin_token": token, "client_id": client_id }),
    ))
}

#[derive(Deserialize)]
pub struct PasswordChangeRequest {
    pub old_password: String,
    pub new_password: String,
}

// POST /api/auth/admin/password
pub async fn admin_password(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<PasswordChangeRequest>,
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

    let hash = stored.unwrap_or_default();
    if !verify_password(&body.old_password, &hash) {
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
    // Spec 6.1: 区间为闭区间，to 最多 frm+200，可能返回 201 行，受 doc.line_count 保护
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
        "page": null, // PDF page 映射将在后续检索/解析中精确填充
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
pub struct AccessCodeRequest {
    pub code: String,
}

// PUT /api/admin/access-code
pub async fn admin_set_access_code(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<AccessCodeRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    require_admin(&headers, &state)?;

    if body.code.len() < 4 || body.code.len() > 64 {
        return Err(ApiError::bad_request("访问码长度必须在 4-64 字符之间"));
    }

    let hash = hash_password(&body.code, None);
    state
        .db
        .setting_set("access_code_hash".to_string(), hash)
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;

    let _ = state
        .db
        .audit(
            "admin".into(),
            "access_code_changed".into(),
            "旧访问码立即失效；已登录用户不受影响".into(),
        )
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

    // 计算 data_dir 字节大小
    let mut data_bytes = 0u64;
    if let Ok(entries) = std::fs::read_dir(&state.config.data_dir) {
        for entry in entries.flatten() {
            if let Ok(meta) = entry.metadata() {
                data_bytes += meta.len();
            }
        }
    }

    Ok(Json(json!({
        "disk": {
            "total_gb": 100.0,
            "used_gb": 20.0,
            "free_gb": 80.0,
            "warn_gb": state.config.disk_warn_gb,
        },
        "counts": {
            "documents": counts.documents,
            "current": counts.current,
            "chunks": counts.chunks,
            "packages": counts.packages,
        },
        "data_dir_mb": (data_bytes as f64) / 1_000_000.0,
        "warnings": []
    })))
}

// GET /api/admin/metrics
pub async fn admin_metrics(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<serde_json::Value>> {
    require_admin(&headers, &state)?;

    let mut sys = System::new();
    let pid = Pid::from_u32(std::process::id());
    sys.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);

    let (rss_bytes, cpu_s) = if let Some(proc) = sys.process(pid) {
        (proc.memory(), proc.cpu_usage() as f64)
    } else {
        (0, 0.0)
    };

    Ok(Json(json!({
        "at": utcnow(),
        "rss_bytes": rss_bytes,
        "cpu_s": cpu_s,
        "disk_free_bytes": 100_000_000_000u64,
        "running": 0,
        "waiting": 0
    })))
}

pub async fn fallback_handler() -> Json<serde_json::Value> {
    Json(json!({
        "service": "campus-policy-agent-rust",
        "frontend": "尚未构建或处于开发模式"
    }))
}
