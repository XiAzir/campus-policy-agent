use campus_policy_backend::{auth::TokenService, chat::trim_history, db::DbPool, tokenizer};
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;
mod common;

#[tokio::test(flavor = "current_thread")]
async fn full_database_queue_rejects_without_blocking_reactor() {
    let tmp = tempfile::tempdir().unwrap();
    let db = DbPool::new(&tmp.path().join("db.sqlite"), 1, 1).unwrap();
    let (started, ready) = tokio::sync::oneshot::channel();
    let (release, blocked) = std::sync::mpsc::channel();
    // A failsafe also guarantees this regression cannot hang a legacy build forever.
    let watchdog = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        let _ = release.send(());
    });
    let one = db.clone();
    let running = tokio::spawn(async move {
        one.read(move |_| {
            let _ = started.send(());
            blocked.recv().unwrap();
            Ok(())
        })
        .await
    });
    ready.await.unwrap();
    let two = db.clone();
    let queued = tokio::spawn(async move { two.read(|_| Ok(())).await });
    tokio::task::yield_now().await;
    let before = std::time::Instant::now();
    assert!(db.read(|_| Ok(())).await.is_err());
    assert!(before.elapsed() < Duration::from_millis(150));
    running.await.unwrap().unwrap();
    queued.await.unwrap().unwrap();
    watchdog.join().unwrap();
}

#[tokio::test]
async fn unicode_audit_detail_and_worker_panic_are_contained() {
    let tmp = tempfile::tempdir().unwrap();
    let db = DbPool::new(&tmp.path().join("db.sqlite"), 1, 8).unwrap();
    db.audit("admin".into(), "test".into(), "中文边界".repeat(100))
        .await
        .unwrap();
    let detail: String = db
        .read(|conn| conn.query_row("SELECT detail FROM audit_log", [], |row| row.get(0)))
        .await
        .unwrap();
    assert!(detail.len() <= 500);
    assert!(detail.starts_with("中文边界"));
    assert!(
        db.read::<(), _>(|_| panic!("intentional worker regression"))
            .await
            .is_err()
    );
    assert_eq!(db.read(|_| Ok(42)).await.unwrap(), 42);
    assert!(
        db.write::<(), _>(|conn| {
            conn.execute_batch(
                "BEGIN IMMEDIATE; INSERT INTO settings VALUES('partial', 'never commit');",
            )?;
            panic!("intentional manual transaction regression")
        })
        .await
        .is_err()
    );
    assert!(db.write(|conn| Ok(conn.is_autocommit())).await.unwrap());
    assert!(db.setting_get("partial".into()).await.unwrap().is_none());
}

#[tokio::test]
async fn cancelled_long_read_stops_cooperatively() {
    let tmp = tempfile::tempdir().unwrap();
    let db = DbPool::new(&tmp.path().join("db.sqlite"), 1, 8).unwrap();
    let (started, ready) = tokio::sync::oneshot::channel();
    let stopped = Arc::new(AtomicBool::new(false));
    let mark = stopped.clone();
    let worker = db.clone();
    let task = tokio::spawn(async move {
        worker
            .read_cancellable(move |_, cancelled| {
                let _ = started.send(());
                for _ in 0..500 {
                    if cancelled.load(Ordering::Relaxed) {
                        mark.store(true, Ordering::Relaxed);
                        return Ok(());
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
                Ok(())
            })
            .await
    });
    ready.await.unwrap();
    task.abort();
    let _ = task.await;
    tokio::time::timeout(Duration::from_secs(1), db.read(|_| Ok(())))
        .await
        .unwrap()
        .unwrap();
    assert!(stopped.load(Ordering::Relaxed));
}

#[test]
fn invalid_pool_token_and_history_parameters_are_bounded() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("db.sqlite");
    assert!(DbPool::new(&path, 0, 1).is_err());
    assert!(DbPool::new(&path, 1, 0).is_err());
    assert!(TokenService::from_hex_secret("", 30).is_err());
    assert!(TokenService::from_hex_secret(&"ab".repeat(32), i64::MAX).is_err());
    let history = vec![
        json!({"parts":[{"text":"旧消息"}]}),
        json!({"parts":[{"text":"新消息"}]}),
    ];
    assert_eq!(trim_history(history.clone(), usize::MAX, 3), history[1..]);
    assert!(trim_history(history, 0, usize::MAX).is_empty());
    assert_eq!(tokenizer::fts_match_query("不会初始化大词典", 0), "");
}

#[tokio::test]
async fn source_window_rejects_giant_line_and_bad_utf8() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("text.txt");
    std::fs::write(&path, "第一行\r\n第二行\n").unwrap();
    assert_eq!(
        campus_policy_backend::text::read_lines(path.clone(), 1, 2)
            .await
            .unwrap(),
        ["第一行", "第二行"]
    );
    assert!(
        campus_policy_backend::text::read_lines(path.clone(), i64::MAX, 2)
            .await
            .unwrap()
            .is_empty()
    );
    std::fs::write(&path, vec![b'x'; 256 * 1024 + 1]).unwrap();
    assert!(
        campus_policy_backend::text::read_lines(path.clone(), 1, 2)
            .await
            .is_err()
    );
    std::fs::write(&path, [255, b'\n']).unwrap();
    assert!(
        campus_policy_backend::text::read_lines(path, 1, 2)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn missing_vector_file_is_not_a_successful_empty_search() {
    let fixture = common::fixture_copy();
    let db = DbPool::new(&fixture.path().join("campus.db"), 1, 8).unwrap();
    let scope = campus_policy_backend::retrieval::allowed_doc_ids(&db, "current", None, None, None)
        .await
        .unwrap();
    let index =
        campus_policy_backend::vectors::VectorIndex::new(fixture.path().join("missing-vectors"));
    assert!(
        campus_policy_backend::retrieval::search(
            &db,
            &index,
            "学校政策",
            Some(&vec![0.1; 1024]),
            &scope,
            8,
            None
        )
        .await
        .is_err()
    );
    assert!(
        campus_policy_backend::retrieval::search(
            &db,
            &index,
            "学校政策",
            None,
            &scope,
            usize::MAX,
            None
        )
        .await
        .is_err()
    );
}

#[test]
fn malformed_manifest_sections_scope_and_page_map_are_rejected() {
    let original: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/packages/manifest_snapshot.json")).unwrap();
    let mut valid = original.clone();
    valid["documents"][3]["page_map"] = json!([[1, 1]]);
    assert!(campus_policy_backend::pkgfmt::validate_manifest(&valid).is_ok());
    for (field, bad) in [
        ("sections", json!({})),
        ("audience_scope", json!([])),
        ("page_map", json!([[i64::MAX, 1]])),
    ] {
        let mut manifest = valid.clone();
        manifest["documents"][0][field] = bad;
        assert!(
            campus_policy_backend::pkgfmt::validate_manifest(&manifest).is_err(),
            "{field}"
        );
    }
}
