use campus_policy_backend::agent::Agent;
use campus_policy_backend::api::{AppState, create_router};
use campus_policy_backend::auth::{RateLimiter, TokenService};
use campus_policy_backend::chat::ChatManager;
use campus_policy_backend::config::Config;
use campus_policy_backend::db::DbPool;
use campus_policy_backend::vectors::VectorIndex;
use reqwest::header::AUTHORIZATION;
use serde_json::Value;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use tokio::net::TcpListener;

async fn spawn_test_server() -> (String, String, String) {
    let legacy_db = Path::new("tests/fixtures/legacy_data/campus.db");
    assert!(legacy_db.exists(), "必须存在 legacy_data/campus.db");

    let pool = DbPool::new(legacy_db, 2, 64).expect("初始化测试 DbPool 失败");
    let mut config = Config::from_env(None);
    config.data_dir = Path::new("tests/fixtures/legacy_data").to_path_buf();

    let secret = pool
        .setting_get("token_secret".into())
        .await
        .unwrap()
        .unwrap();
    let tokens = TokenService::from_hex_secret(&secret, 30).unwrap();
    let limiter = Arc::new(RateLimiter::new(100));
    let vectors = Arc::new(VectorIndex::new(config.data_dir.join("vectors")));
    let agent = Arc::new(Agent::new(
        pool.clone(),
        Arc::clone(&vectors),
        config.clone(),
    ));
    let chats = Arc::new(ChatManager::new(1, 10));

    let user_token = tokens.issue("user", "test-user");
    let admin_token = tokens.issue("admin", "test-admin");

    let state = AppState {
        config,
        db: pool,
        tokens,
        limiter,
        vectors,
        agent,
        chats,
    };

    let app = create_router(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    let base_url = format!("http://{}", addr);

    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    (base_url, user_token, admin_token)
}

#[tokio::test]
async fn test_api_auth_state() {
    let (base_url, _, _) = spawn_test_server().await;
    let client = reqwest::Client::new();

    let res = client
        .get(format!("{}/api/auth/state", base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["access_code_set"], true);
}

#[tokio::test]
async fn test_api_catalog_requires_auth() {
    let (base_url, user_token, _) = spawn_test_server().await;
    let client = reqwest::Client::new();

    // 1. 无 token 访问应 401
    let res_no_auth = client
        .get(format!("{}/api/catalog", base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(res_no_auth.status(), 401);

    // 2. 带有效 token 访问应 200 并返回 6 篇文档
    let res_ok = client
        .get(format!("{}/api/catalog", base_url))
        .header(AUTHORIZATION, format!("Bearer {}", user_token))
        .send()
        .await
        .unwrap();

    assert_eq!(res_ok.status(), 200);
    let body: Value = res_ok.json().await.unwrap();
    let docs = body["documents"].as_array().unwrap();
    assert_eq!(docs.len(), 6);
}

#[tokio::test]
async fn test_api_source_text_window_and_clamp() {
    let (base_url, user_token, _) = spawn_test_server().await;
    let client = reqwest::Client::new();

    // 先查出长文资料 doc_uid
    let res_cat = client
        .get(format!("{}/api/catalog", base_url))
        .header(AUTHORIZATION, format!("Bearer {}", user_token))
        .send()
        .await
        .unwrap();
    let body: Value = res_cat.json().await.unwrap();
    let docs = body["documents"].as_array().unwrap();
    let long_doc = docs
        .iter()
        .find(|d| d["line_count"].as_i64().unwrap_or(0) > 100)
        .unwrap();
    let uid = long_doc["doc_uid"].as_str().unwrap();

    // 1. 默认窗口 frm=1, to=80
    let res_def = client
        .get(format!("{}/api/source/{}/text", base_url, uid))
        .header(AUTHORIZATION, format!("Bearer {}", user_token))
        .send()
        .await
        .unwrap();
    assert_eq!(res_def.status(), 200);
    let val_def: Value = res_def.json().await.unwrap();
    assert_eq!(val_def["line_start"], 1);
    assert_eq!(val_def["line_end"], 80);
    assert_eq!(val_def["lines"].as_array().unwrap().len(), 80);
    assert!(val_def["lines"][0].as_str().unwrap().starts_with("L1:"));

    // 2. 窗口 clamp 测试：请求 to=500，应被限制到 frm + 200 = 201 行 (闭区间 1..=201)
    let res_clamped = client
        .get(format!("{}/api/source/{}/text?frm=1&to=500", base_url, uid))
        .header(AUTHORIZATION, format!("Bearer {}", user_token))
        .send()
        .await
        .unwrap();
    assert_eq!(res_clamped.status(), 200);
    let val_clamped: Value = res_clamped.json().await.unwrap();
    assert_eq!(val_clamped["line_start"], 1);
    assert_eq!(val_clamped["line_end"], 201);
    assert_eq!(val_clamped["lines"].as_array().unwrap().len(), 201);
}

#[tokio::test]
async fn test_api_source_file_download() {
    let (base_url, user_token, _) = spawn_test_server().await;
    let client = reqwest::Client::new();

    let res_cat = client
        .get(format!("{}/api/catalog", base_url))
        .header(AUTHORIZATION, format!("Bearer {}", user_token))
        .send()
        .await
        .unwrap();
    let body: Value = res_cat.json().await.unwrap();
    let docs = body["documents"].as_array().unwrap();
    let uid = docs[0]["doc_uid"].as_str().unwrap();

    // 1. 无 token 401
    let res_no_auth = client
        .get(format!("{}/api/source/{}/file", base_url, uid))
        .send()
        .await
        .unwrap();
    assert_eq!(res_no_auth.status(), 401);

    // 2. 有 token 200 并检查 nosniff
    let res_ok = client
        .get(format!("{}/api/source/{}/file", base_url, uid))
        .header(AUTHORIZATION, format!("Bearer {}", user_token))
        .send()
        .await
        .unwrap();
    assert_eq!(res_ok.status(), 200);
    assert_eq!(
        res_ok.headers().get("X-Content-Type-Options").unwrap(),
        "nosniff"
    );
    let bytes = res_ok.bytes().await.unwrap();
    assert!(!bytes.is_empty());
}

#[tokio::test]
async fn test_api_admin_metrics() {
    let (base_url, _, admin_token) = spawn_test_server().await;
    let client = reqwest::Client::new();

    let res = client
        .get(format!("{}/api/admin/metrics", base_url))
        .header(AUTHORIZATION, format!("Bearer {}", admin_token))
        .send()
        .await
        .unwrap();

    assert_eq!(res.status(), 200);
    let body: Value = res.json().await.unwrap();
    assert!(body["rss_bytes"].as_u64().unwrap() > 0);
    assert!(body["at"].is_string());
}
