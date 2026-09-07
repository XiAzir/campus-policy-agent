use campus_policy_backend::{
    chat::ChatManager,
    llm::{GeminiClient, StreamEvent},
};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

async fn collect(chunks: Vec<Vec<u8>>) -> Result<String, String> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = axum::Router::new().route(
        "/models/{action}",
        axum::routing::post(move || {
            let chunks = chunks.clone();
            async move {
                let stream =
                    futures_util::stream::unfold(chunks.into_iter(), |mut iter| async move {
                        tokio::time::sleep(Duration::from_millis(1)).await;
                        iter.next().map(|b| (Ok::<_, std::io::Error>(b), iter))
                    });
                axum::body::Body::from_stream(stream)
            }
        }),
    );
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let client = GeminiClient::with_client(
        reqwest::Client::new(),
        format!("http://{addr}"),
        "test".into(),
        "fake".into(),
    );
    let text = Arc::new(Mutex::new(String::new()));
    let output = text.clone();
    let result = client
        .stream(vec![], None, None, 0.2, None, move |event| {
            let output = output.clone();
            Box::pin(async move {
                if let StreamEvent::Text(t) = event {
                    output.lock().await.push_str(&t);
                }
            })
        })
        .await;
    server.abort();
    let _ = server.await;
    result.map_err(|e| e.to_string())?;
    Ok(text.lock().await.clone())
}

#[tokio::test]
async fn utf8_split_and_crlf_multiline_sse() {
    let data = "data: {\r\ndata: \"candidates\":[{\"content\":{\"parts\":[{\"text\":\"中文\"}]}}]}\r\n\r\n".as_bytes();
    let pos = data.windows(3).position(|w| w == "中".as_bytes()).unwrap() + 1;
    assert_eq!(
        collect(vec![data[..pos].to_vec(), data[pos..].to_vec()])
            .await
            .unwrap(),
        "中文"
    );
}

#[tokio::test]
async fn malformed_empty_and_oversized_streams_fail() {
    for data in [
        b"data: broken\n\n".to_vec(),
        b"data: {}\n\n".to_vec(),
        vec![b'x'; 1024 * 1024 + 1],
    ] {
        assert!(collect(vec![data]).await.is_err());
    }
}

#[tokio::test]
async fn backpressure_delivers_all_events_and_disconnect_releases_slot() {
    let manager = Arc::new(ChatManager::new(1, 2));
    let (_, mut rx) = manager
        .submit("burst".into(), 30, |_, tx| async move {
            for index in 0..5000 {
                if tx
                    .send(json!({"event":"delta", "index":index}))
                    .await
                    .is_err()
                {
                    return;
                }
            }
        })
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut count = 0;
    while let Some(event) = rx.recv().await {
        if event["event"] == "delta" {
            assert_eq!(event["index"], count);
            count += 1;
        }
        if event["event"] == "__end__" {
            break;
        }
    }
    assert_eq!(count, 5000);
    let (_, mut rx) = manager
        .submit("disconnect".into(), 30, |_, _| async {
            std::future::pending::<()>().await;
        })
        .await
        .unwrap();
    assert_eq!(rx.recv().await.unwrap()["event"], "started");
    drop(rx);
    tokio::time::timeout(Duration::from_secs(1), async {
        while manager.counts().await != (0, 0) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn panic_and_concurrent_cancel_do_not_deadlock_queue() {
    let manager = Arc::new(ChatManager::new(1, 10));
    let (job, mut rx) = manager
        .submit("panic".into(), 5, |_, _| async {
            panic!("injected");
        })
        .await
        .unwrap();
    while let Some(event) = rx.recv().await {
        if event["event"] == "__end__" {
            break;
        }
    }
    assert!(!manager.cancel(&job.request_id, "panic").await);
    let mut receivers = vec![];
    for index in 0..10 {
        let (_, rx) = manager
            .submit(index.to_string(), 5, |_, _| async {
                std::future::pending::<()>().await;
            })
            .await
            .unwrap();
        receivers.push(rx);
    }
    tokio::time::timeout(Duration::from_secs(1), manager.cancel_all())
        .await
        .unwrap();
    assert_eq!(manager.counts().await, (0, 0));
}

#[tokio::test]
async fn queue_enforces_byte_budget_and_rejects_single_oversize_event() {
    let manager = Arc::new(ChatManager::new(1, 1));
    let progress = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = progress.clone();
    let (_, mut rx) = manager
        .submit("bytes".into(), 10, move |_, tx| async move {
            assert!(
                tx.send(json!({"text":"x".repeat(1024 * 1024)}))
                    .await
                    .is_err()
            );
            for _ in 0..3 {
                tx.send(json!({"text":"x".repeat(600 * 1024)}))
                    .await
                    .unwrap();
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        })
        .await
        .unwrap();
    assert_eq!(rx.recv().await.unwrap()["event"], "started");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(progress.load(std::sync::atomic::Ordering::SeqCst), 1);
    for _ in 0..3 {
        assert!(rx.recv().await.unwrap()["text"].is_string());
    }
    assert_eq!(rx.recv().await.unwrap()["event"], "__end__");
}
