use campus_policy_backend::api::{AppState, create_router};
use campus_policy_backend::auth::{RateLimiter, TokenService};
use campus_policy_backend::backup;
use campus_policy_backend::chat::ChatManager;
use campus_policy_backend::config::Config;
use campus_policy_backend::db::DbPool;
use campus_policy_backend::maintenance::MaintenanceState;
use campus_policy_backend::vectors::VectorIndex;
use reqwest::header::AUTHORIZATION;
use serde_json::Value;
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::net::TcpListener;

fn setup_test_data_dir(name: &str) -> (PathBuf, PathBuf) {
    let tmp = std::env::temp_dir().join(format!("cpa-rust-backup-test-{}", name));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp).unwrap();

    let data_dir = tmp.join("data");
    fs::create_dir_all(&data_dir).unwrap();

    // 拷贝 legacy 数据到测试目录
    let legacy_src = Path::new("tests/fixtures/legacy_data");
    for entry in fs::read_dir(legacy_src).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        let target = data_dir.join(entry.file_name());
        if path.is_dir() {
            copy_dir_all(&path, &target);
        } else {
            fs::copy(&path, &target).unwrap();
        }
    }

    (tmp, data_dir)
}

fn copy_dir_all(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).unwrap();
    for entry in fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        let target = dst.join(entry.file_name());
        if path.is_dir() {
            copy_dir_all(&path, &target);
        } else {
            fs::copy(&path, &target).unwrap();
        }
    }
}

#[tokio::test]
async fn test_backup_create_and_validate() {
    let (_tmp, data_dir) = setup_test_data_dir("create_val");
    let db_path = data_dir.join("campus.db");
    let pool = DbPool::new(&db_path, 2, 64).unwrap();

    let mut config = Config::from_env(None);
    config.data_dir = data_dir.clone();
    config.siliconflow_model = "test-embed".to_string();
    config.embed_dims = 1024;
    config.preprocessing_version = "v1".to_string();

    let out_zip = data_dir.parent().unwrap().join("test_backup.zip");

    // 1. 导出备份
    backup::create_backup(&pool, &config, &out_zip)
        .await
        .unwrap();
    assert!(out_zip.exists());

    // 2. 检查备份压缩包结构
    let f = fs::File::open(&out_zip).unwrap();
    let (meta, uncompressed_size) = backup::inspect_backup_archive(f).unwrap();
    assert_eq!(meta.format, backup::BACKUP_FORMAT);
    assert_eq!(meta.version, backup::BACKUP_VERSION);
    assert_eq!(meta.counts.documents, 6);
    assert_eq!(meta.counts.chunks, 6);
    assert!(uncompressed_size > 0);

    // 3. 校验解压后快照完整性
    let unpack_dir = data_dir.parent().unwrap().join("unpacked");
    fs::create_dir_all(&unpack_dir).unwrap();

    let mut zip = zip::ZipArchive::new(fs::File::open(&out_zip).unwrap()).unwrap();
    for rel_name in meta.files.keys() {
        let mut zf = zip.by_name(rel_name).unwrap();
        let dst = unpack_dir.join(rel_name);
        if let Some(p) = dst.parent() {
            fs::create_dir_all(p).unwrap();
        }
        let mut out = fs::File::create(&dst).unwrap();
        std::io::copy(&mut zf, &mut out).unwrap();
    }

    backup::check_unpacked_snapshot(&unpack_dir, &config, &meta).unwrap();
}

#[tokio::test]
async fn test_maintenance_gate_and_restore_cycle() {
    let (_tmp, data_dir) = setup_test_data_dir("maint_restore");
    let db_path = data_dir.join("campus.db");
    let pool = DbPool::new(&db_path, 2, 64).unwrap();

    let mut config = Config::from_env(None);
    config.data_dir = data_dir.clone();
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
    let agent = Arc::new(campus_policy_backend::agent::Agent::new(
        pool.clone(),
        Arc::clone(&vectors),
        config.clone(),
    ));
    let chats = Arc::new(ChatManager::new(1, 10));
    let maintenance = Arc::new(MaintenanceState::new());

    let state = AppState {
        config: config.clone(),
        db: pool.clone(),
        tokens: Arc::clone(&tokens),
        limiter,
        vectors: Arc::clone(&vectors),
        agent,
        chats: Arc::clone(&chats),
        maintenance: Arc::clone(&maintenance),
    };

    let app = create_router(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    let base_url = format!("http://{}", addr);

    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let client = reqwest::Client::new();

    // 1. 先下载备份
    let res_backup = client
        .get(format!("{}/api/admin/backup", base_url))
        .header(AUTHORIZATION, format!("Bearer {}", admin_token))
        .send()
        .await
        .unwrap();
    assert_eq!(res_backup.status(), 200);
    let backup_bytes = res_backup.bytes().await.unwrap();
    assert!(!backup_bytes.is_empty());

    // 2. 测试维护门模式：手动触发 restoring
    maintenance
        .restoring
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let res_maint = client
        .get(format!("{}/api/catalog", base_url))
        .header(AUTHORIZATION, format!("Bearer {}", user_token))
        .send()
        .await
        .unwrap();
    assert_eq!(res_maint.status(), 503);
    let maint_body: Value = res_maint.json().await.unwrap();
    assert!(maint_body["detail"].as_str().unwrap().contains("正在恢复"));

    // 退出维护门
    maintenance
        .restoring
        .store(false, std::sync::atomic::Ordering::SeqCst);
    let res_ok = client
        .get(format!("{}/api/catalog", base_url))
        .header(AUTHORIZATION, format!("Bearer {}", user_token))
        .send()
        .await
        .unwrap();
    assert_eq!(res_ok.status(), 200);

    // 3. HTTP 恢复接口 POST /api/admin/restore
    let part = reqwest::multipart::Part::bytes(backup_bytes.to_vec())
        .file_name("backup.zip")
        .mime_str("application/zip")
        .unwrap();
    let form = reqwest::multipart::Form::new().part("file", part);

    let res_restore = client
        .post(format!("{}/api/admin/restore", base_url))
        .header(AUTHORIZATION, format!("Bearer {}", admin_token))
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(res_restore.status(), 200);
    let restore_body: Value = res_restore.json().await.unwrap();
    assert_eq!(restore_body["ok"], true);
    assert_eq!(restore_body["summary"]["documents"], 6);

    // 恢复后正常访问 catalog
    let res_after = client
        .get(format!("{}/api/catalog", base_url))
        .header(AUTHORIZATION, format!("Bearer {}", user_token))
        .send()
        .await
        .unwrap();
    assert_eq!(res_after.status(), 200);
}

#[tokio::test]
async fn test_restore_rejected_cases() {
    let (_tmp, data_dir) = setup_test_data_dir("rejected_cases");
    let out_zip = data_dir.parent().unwrap().join("bad_backup.zip");

    // 1. 生成缺少 db.sqlite 文件但 zip 里有其它条目的坏备份
    {
        let file = fs::File::create(&out_zip).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file("backup.json", options).unwrap();
        use std::io::Write;
        zip.write_all(b"{\"format\":\"campus-policy-backup\",\"version\":2,\"created_at\":\"now\",\"counts\":{\"documents\":0,\"chunks\":0},\"files\":{}}").unwrap();
        zip.start_file("files/extra.txt", options).unwrap();
        zip.write_all(b"extra").unwrap();
        zip.finish().unwrap();
    }

    let f = fs::File::open(&out_zip).unwrap();
    let res = backup::inspect_backup_archive(f);
    assert!(res.is_err());
    let err_msg = res.unwrap_err().to_string();
    assert!(err_msg.contains("清单与实际文件条目不一致") || err_msg.contains("格式错误"));

    // 2. 生成带路径穿越条目的坏备份
    let out_traversal = data_dir.parent().unwrap().join("traversal_backup.zip");
    {
        let file = fs::File::create(&out_traversal).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file("../evil.txt", options).unwrap();
        use std::io::Write;
        zip.write_all(b"malicious").unwrap();
        zip.finish().unwrap();
    }

    let f2 = fs::File::open(&out_traversal).unwrap();
    let res2 = backup::inspect_backup_archive(f2);
    assert!(res2.is_err());
    assert!(res2.unwrap_err().to_string().contains("不合法路径"));
}
