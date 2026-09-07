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
mod common;
use std::sync::Arc;
use tokio::net::TcpListener;

async fn spawn_test_server() -> (String, String, String) {
    let fixture = common::fixture_copy();
    let legacy_db = fixture.path().join("campus.db");
    assert!(legacy_db.exists(), "必须存在 legacy_data/campus.db");

    let pool = DbPool::new(&legacy_db, 2, 64).expect("初始化测试 DbPool 失败");
    let mut config = Config::from_env(None);
    config.data_dir = fixture.path().to_path_buf();
    config.siliconflow_model = "test-embed".to_string();
    config.embed_dims = 1024;
    config.preprocessing_version = "v1".to_string();

    let secret = pool
        .setting_get("token_secret".into())
        .await
        .unwrap()
        .unwrap();
    let token_svc = TokenService::from_hex_secret(&secret, 30).unwrap();
    let user_token = token_svc.issue("user", "test-user");
    let admin_token = token_svc.issue("admin", "test-admin");
    let tokens = Arc::new(tokio::sync::RwLock::new(token_svc));
    let limiter = Arc::new(RateLimiter::new(100));
    let vectors = Arc::new(VectorIndex::new(config.data_dir.join("vectors")));
    let agent = Arc::new(Agent::new(
        pool.clone(),
        Arc::clone(&vectors),
        config.clone(),
    ));
    let chats = Arc::new(ChatManager::new(1, 10));
    let maintenance = Arc::new(campus_policy_backend::maintenance::MaintenanceState::new());

    let state = AppState {
        config,
        db: pool,
        tokens,
        limiter,
        vectors,
        agent,
        chats,
        maintenance,
    };

    let app = create_router(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    let base_url = format!("http://{}", addr);

    tokio::spawn(async move {
        let _fixture = fixture;
        axum::serve(listener, app).await.unwrap();
    });

    (base_url, user_token, admin_token)
}

#[tokio::test]
async fn forwarded_headers_cannot_reset_login_bucket() {
    let (url, _, _) = spawn_test_server().await;
    let client = reqwest::Client::new();
    for index in 0..12 {
        let response = client
            .post(format!("{url}/api/auth/admin/login"))
            .header("x-forwarded-for", format!("192.0.2.{index}"))
            .json(&serde_json::json!({"password":"definitely-wrong"}))
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status().as_u16(),
            if index < 10 { 401 } else { 429 }
        );
    }
}

#[tokio::test]
async fn chunked_body_limit_returns_json_413() {
    let (url, _, _) = spawn_test_server().await;
    let body =
        futures_util::stream::iter((0..5).map(|_| Ok::<_, std::io::Error>(vec![b' '; 64 * 1024])));
    let response = reqwest::Client::new()
        .post(format!("{url}/api/auth/login"))
        .header("content-type", "application/json")
        .body(reqwest::Body::wrap_stream(body))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 413);
    assert!(response.json::<Value>().await.unwrap()["detail"].is_string());
}

#[tokio::test]
async fn valid_package_over_two_mib_uploads_and_whole_doc_tag_publishes() {
    use std::io::{Read, Write};
    let (url, _, token) = spawn_test_server().await;
    let file = std::fs::File::open("tests/fixtures/packages/small_v1.zip").unwrap();
    let mut source = zip::ZipArchive::new(file).unwrap();
    let mut output = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for index in 0..source.len() {
        let mut entry = source.by_index(index).unwrap();
        let mut bytes = vec![];
        entry.read_to_end(&mut bytes).unwrap();
        if entry.name() == "manifest.json" {
            let mut manifest: Value = serde_json::from_slice(&bytes).unwrap();
            manifest["documents"][0]["domains"] =
                serde_json::json!([{"tag":"whole-doc-regression", "section_ids":[]}]);
            bytes = serde_json::to_vec(&manifest).unwrap();
            bytes.extend(std::iter::repeat_n(b' ', 2400000));
        }
        output
            .start_file(
                entry.name(),
                zip::write::SimpleFileOptions::default()
                    .compression_method(zip::CompressionMethod::Stored),
            )
            .unwrap();
        output.write_all(&bytes).unwrap();
    }
    let bytes = output.finish().unwrap().into_inner();
    assert!(bytes.len() > 2 * 1024 * 1024);
    let form = reqwest::multipart::Form::new().part(
        "file",
        reqwest::multipart::Part::bytes(bytes).file_name("large.zip"),
    );
    let client = reqwest::Client::new();
    let response = client
        .post(format!("{url}/api/admin/packages"))
        .bearer_auth(&token)
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
    let id = response.json::<Value>().await.unwrap()["id"]
        .as_i64()
        .unwrap();
    let response = client
        .post(format!("{url}/api/admin/packages/{id}/publish"))
        .bearer_auth(&token)
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
    let response = client
        .get(format!("{url}/api/admin/backup"))
        .bearer_auth(token)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let bytes = response.bytes().await.unwrap();
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("db.sqlite");
    std::io::copy(
        &mut archive.by_name("db.sqlite").unwrap(),
        &mut std::fs::File::create(&path).unwrap(),
    )
    .unwrap();
    let db = rusqlite::Connection::open(path).unwrap();
    let count: i64 = db
        .query_row(
            "SELECT COUNT(*) FROM doc_tags WHERE tag='whole-doc-regression' AND section_id IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
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

#[tokio::test]
async fn test_api_admin_packages_http_endpoints() {
    let (base_url, user_token, admin_token) = spawn_test_server().await;
    let client = reqwest::Client::new();

    // 1. 获取包列表应返回现有 1 个已发布包
    let res = client
        .get(format!("{}/api/admin/packages", base_url))
        .header(AUTHORIZATION, format!("Bearer {}", admin_token))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let body: Value = res.json().await.unwrap();
    let pkgs = body["packages"].as_array().unwrap();
    assert_eq!(pkgs.len(), 1);
    assert_eq!(pkgs[0]["id"], 1);

    // 2. 预览不存在的包返回 404
    let res_404 = client
        .get(format!("{}/api/admin/packages/9999", base_url))
        .header(AUTHORIZATION, format!("Bearer {}", admin_token))
        .send()
        .await
        .unwrap();
    assert_eq!(res_404.status(), 404);

    // 3. 上传重复资料包 (small_v1.zip 已发布过) 应返回 400 拦截
    let pkg_bytes = std::fs::read("tests/fixtures/packages/small_v1.zip").unwrap();
    let part = reqwest::multipart::Part::bytes(pkg_bytes)
        .file_name("small_v1.zip")
        .mime_str("application/zip")
        .unwrap();
    let form = reqwest::multipart::Form::new().part("file", part);

    let res_upload = client
        .post(format!("{}/api/admin/packages", base_url))
        .header(AUTHORIZATION, format!("Bearer {}", admin_token))
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(res_upload.status(), 400);
    let upload_err: Value = res_upload.json().await.unwrap();
    assert!(upload_err["detail"].as_str().unwrap().contains("重复导入"));

    // 4. 停用与启用文档
    let cat_res = client
        .get(format!("{}/api/catalog", base_url))
        .header(AUTHORIZATION, format!("Bearer {}", user_token))
        .send()
        .await
        .unwrap();
    let cat_body: Value = cat_res.json().await.unwrap();
    let target_uid = cat_body["documents"][0]["doc_uid"].as_str().unwrap();

    // 手动停用
    let deact_res = client
        .post(format!(
            "{}/api/admin/documents/{}/deactivate",
            base_url, target_uid
        ))
        .header(AUTHORIZATION, format!("Bearer {}", admin_token))
        .send()
        .await
        .unwrap();
    assert_eq!(deact_res.status(), 200);

    // 停用后 catalog 现行文档减少 1 篇
    let cat_res2 = client
        .get(format!("{}/api/catalog", base_url))
        .header(AUTHORIZATION, format!("Bearer {}", user_token))
        .send()
        .await
        .unwrap();
    assert_eq!(
        cat_res2.json::<Value>().await.unwrap()["documents"]
            .as_array()
            .unwrap()
            .len(),
        5
    );

    // 重新启用
    let enable_res = client
        .post(format!(
            "{}/api/admin/documents/{}/enable",
            base_url, target_uid
        ))
        .header(AUTHORIZATION, format!("Bearer {}", admin_token))
        .send()
        .await
        .unwrap();
    assert_eq!(enable_res.status(), 200);

    // 启用后恢复为 6 篇
    let cat_res3 = client
        .get(format!("{}/api/catalog", base_url))
        .header(AUTHORIZATION, format!("Bearer {}", user_token))
        .send()
        .await
        .unwrap();
    assert_eq!(
        cat_res3.json::<Value>().await.unwrap()["documents"]
            .as_array()
            .unwrap()
            .len(),
        6
    );

    // 5. 无替代关系时调用 unlink 返回 400
    let unlink_res = client
        .post(format!(
            "{}/api/admin/documents/{}/unlink",
            base_url, target_uid
        ))
        .header(AUTHORIZATION, format!("Bearer {}", admin_token))
        .send()
        .await
        .unwrap();
    assert_eq!(unlink_res.status(), 400);
}
