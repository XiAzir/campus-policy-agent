use campus_policy_backend::config::Config;
use campus_policy_backend::db::DbPool;
use campus_policy_backend::ingest;
use serde_json::json;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

fn setup_test_env(test_name: &str) -> (DbPool, Config, PathBuf) {
    let base_tmp = std::env::temp_dir().join(format!("cpa-rust-test-{}", test_name));
    let _ = fs::remove_dir_all(&base_tmp);
    fs::create_dir_all(&base_tmp).unwrap();

    let data_dir = base_tmp.join("data");
    fs::create_dir_all(&data_dir).unwrap();

    let db_path = data_dir.join("campus.db");
    let pool = DbPool::new(&db_path, 2, 64).expect("初始化测试 DbPool 失败");

    let mut config = Config::from_env(None);
    config.data_dir = data_dir;
    config.siliconflow_model = "test-embed".to_string();
    config.embed_dims = 1024;
    config.preprocessing_version = "v1".to_string();

    (pool, config, base_tmp)
}

#[tokio::test]
async fn test_ingest_duplicate_import_rejected() {
    let (db, config, _tmp) = setup_test_env("dup");
    let pkg_zip = Path::new("tests/fixtures/packages/small_v1.zip");
    assert!(pkg_zip.exists());

    let pid1 = ingest::import_package(&db, &config, pkg_zip, "small_v1.zip")
        .await
        .unwrap();
    assert_eq!(pid1, 1);

    // 重复导入应被拦截
    let res = ingest::import_package(&db, &config, pkg_zip, "small_v1.zip").await;
    assert!(res.is_err());
    let err = res.unwrap_err();
    assert!(err.to_string().contains("重复导入"));
}

#[tokio::test]
async fn test_ingest_preview_and_meta_override() {
    let (db, config, _tmp) = setup_test_env("prev_ov");
    let pkg_zip = Path::new("tests/fixtures/packages/small_v1.zip");

    let pid = ingest::import_package(&db, &config, pkg_zip, "small_v1.zip")
        .await
        .unwrap();

    let preview = ingest::preview_package(&db, &config, pid)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(preview["id"], pid);
    assert_eq!(preview["doc_count"], 6);
    assert_eq!(preview["status"], "draft");

    let docs = preview["documents"].as_array().unwrap();
    let doc_hash = docs[0]["doc_hash"].as_str().unwrap();

    // 尝试修改违禁字段 chunks 应被拒绝
    let mut bad_fields = HashMap::new();
    bad_fields.insert("chunks".to_string(), json!([]));
    let bad_res = ingest::apply_override(&db, &config, pid, doc_hash, bad_fields).await;
    assert!(bad_res.is_err());
    assert!(
        bad_res
            .unwrap_err()
            .to_string()
            .contains("会影响分块或向量")
    );

    // 允许修改 title 与 notes
    let mut ok_fields = HashMap::new();
    ok_fields.insert("title".to_string(), json!("改过的标题"));
    ok_fields.insert("notes".to_string(), json!("修订备注"));
    ingest::apply_override(&db, &config, pid, doc_hash, ok_fields)
        .await
        .unwrap();

    let preview2 = ingest::preview_package(&db, &config, pid)
        .await
        .unwrap()
        .unwrap();
    let docs2 = preview2["documents"].as_array().unwrap();
    let doc2 = docs2.iter().find(|d| d["doc_hash"] == doc_hash).unwrap();
    assert_eq!(doc2["title"], "改过的标题");
    assert_eq!(doc2["notes"], "修订备注");
    assert_eq!(doc2["edited"], true);
}

#[tokio::test]
async fn test_ingest_publish_and_versions_lifecycle() {
    let (db, config, _tmp) = setup_test_env("pub_lifecycle");
    let pkg_zip = Path::new("tests/fixtures/packages/small_v1.zip");

    let pid = ingest::import_package(&db, &config, pkg_zip, "small_v1.zip")
        .await
        .unwrap();

    // 发布草稿包
    ingest::publish_package(&db, &config, pid, HashMap::new())
        .await
        .unwrap();

    // 验证数据库发布状态与关联表生成
    let counts = db.get_counts().await.unwrap();
    assert_eq!(counts.documents, 6);
    assert_eq!(counts.current, 6);
    assert_eq!(counts.packages, 1);
    assert_eq!(counts.chunks, 6);

    // 验证正式向量文件与不可变原文件、标准化文本
    let final_vec = config
        .data_dir
        .join("vectors")
        .join(format!("pkg-{}.npy", pid));
    assert!(final_vec.exists());

    // 查出一份文档 doc_uid
    let docs = db.get_catalog().await.unwrap();
    let uid = &docs[0].doc_uid;

    // 1. 手动停用
    ingest::deactivate_document(&db, uid).await.unwrap();
    let counts_after = db.get_counts().await.unwrap();
    assert_eq!(counts_after.current, 5);

    // 再次手动停用应报错
    let deact_twice = ingest::deactivate_document(&db, uid).await;
    assert!(deact_twice.is_err());

    // 2. 重新启用
    ingest::enable_document(&db, uid).await.unwrap();
    let counts_reenable = db.get_counts().await.unwrap();
    assert_eq!(counts_reenable.current, 6);

    // 3. 验证版本链查询
    let history = db.get_version_history(uid.to_string()).await.unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(&history[0].doc_uid, uid);
    assert!(history[0].is_current);
}

#[tokio::test]
async fn test_ingest_discard_draft() {
    let (db, config, _tmp) = setup_test_env("discard");
    let pkg_zip = Path::new("tests/fixtures/packages/small_v1.zip");

    let pid = ingest::import_package(&db, &config, pkg_zip, "small_v1.zip")
        .await
        .unwrap();

    ingest::discard_package(&db, &config, pid).await.unwrap();

    // 丢弃后不可再次丢弃
    let discard_again = ingest::discard_package(&db, &config, pid).await;
    assert!(discard_again.is_err());

    let preview = ingest::preview_package(&db, &config, pid).await.unwrap();
    assert!(preview.is_none());
}

#[tokio::test]
async fn test_ingest_path_traversal_zip_rejected() {
    let (db, config, tmp) = setup_test_env("traversal");
    let bad_zip = tmp.join("bad_traversal.zip");

    {
        let file = fs::File::create(&bad_zip).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file("../evil.txt", options).unwrap();
        use std::io::Write;
        zip.write_all(b"malicious payload").unwrap();
        zip.finish().unwrap();
    }

    let res = ingest::import_package(&db, &config, &bad_zip, "bad.zip").await;
    assert!(res.is_err());
    let err_msg = res.unwrap_err().to_string();
    assert!(
        err_msg.contains("穿越") || err_msg.contains("不合法") || err_msg.contains("包含不允许")
    );
}

#[tokio::test]
async fn test_ingest_bad_target_publish_rejected() {
    let (db, config, _tmp) = setup_test_env("bad_target");
    let pkg_zip = Path::new("tests/fixtures/packages/small_v1.zip");

    let pid = ingest::import_package(&db, &config, pkg_zip, "small_v1.zip")
        .await
        .unwrap();

    let mut replacements = HashMap::new();
    let preview = ingest::preview_package(&db, &config, pid)
        .await
        .unwrap()
        .unwrap();
    let doc_hash = preview["documents"][0]["doc_hash"]
        .as_str()
        .unwrap()
        .to_string();
    replacements.insert(doc_hash, Some("nonexistent-target-uid".to_string()));

    let res = ingest::publish_package(&db, &config, pid, replacements).await;
    assert!(res.is_err());
    let err_msg = res.unwrap_err().to_string();
    assert!(err_msg.contains("替代目标不存在或不唯一"));
}
