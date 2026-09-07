use campus_policy_backend::agent::Agent;
use campus_policy_backend::api::{AppState, create_router};
use campus_policy_backend::auth::{RateLimiter, TokenService};
use campus_policy_backend::chat::ChatManager;
use campus_policy_backend::config::Config;
use campus_policy_backend::db::DbPool;
use campus_policy_backend::llm::GeminiClient;
use campus_policy_backend::vectors::VectorIndex;
use futures_util::StreamExt;
use reqwest::header::AUTHORIZATION;
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use tokio::net::TcpListener;

#[tokio::test]
async fn test_chat_manager_queue_and_concurrency() {
    let mgr = Arc::new(ChatManager::new(1, 2));

    let (j1, mut rx1) = mgr
        .submit("client-1".into(), 5, |_job, tx| async move {
            tokio::time::sleep(std::time::Duration::from_millis(80)).await;
            let _ = tx.send(json!({ "event": "done", "client": 1 })).await;
        })
        .await
        .unwrap();

    let (_j2, mut rx2) = mgr
        .submit("client-2".into(), 5, |_job, tx| async move {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            let _ = tx.send(json!({ "event": "done", "client": 2 })).await;
        })
        .await
        .unwrap();

    // j1 应立刻 started
    let ev1 = rx1.recv().await.unwrap();
    assert_eq!(ev1["event"], "started");
    assert_eq!(ev1["request_id"], j1.request_id);

    // j2 应先 queued，position=1
    let ev2_q = rx2.recv().await.unwrap();
    assert_eq!(ev2_q["event"], "queued");
    assert_eq!(ev2_q["position"], 1);

    // 同一客户端再次提交应被拒绝
    let dup_res = mgr.submit("client-2".into(), 5, |_job, _tx| async {}).await;
    assert!(dup_res.is_err());

    // 等待 j1 完成
    let ev1_done = rx1.recv().await.unwrap();
    assert_eq!(ev1_done["event"], "done");
    let ev1_end = rx1.recv().await.unwrap();
    assert_eq!(ev1_end["event"], "__end__");

    // j2 此时应自动被调度 started
    let ev2_started = rx2.recv().await.unwrap();
    assert_eq!(ev2_started["event"], "started");
    let ev2_done = rx2.recv().await.unwrap();
    assert_eq!(ev2_done["event"], "done");
}

#[tokio::test]
async fn test_chat_manager_cancel() {
    let mgr = Arc::new(ChatManager::new(1, 2));

    let (job, mut rx) = mgr
        .submit("client-cancel".into(), 10, |_job, tx| async move {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            let _ = tx.send(json!({ "event": "should_not_reach" })).await;
        })
        .await
        .unwrap();

    let _ev_started = rx.recv().await.unwrap();
    let cancelled = mgr.cancel(&job.request_id, "client-cancel").await;
    assert!(cancelled);

    let ev_err = rx.recv().await.unwrap();
    assert_eq!(ev_err["event"], "error");
    assert_eq!(ev_err["message"], "已取消");
    let ev_end = rx.recv().await.unwrap();
    assert_eq!(ev_end["event"], "__end__");
}

#[tokio::test]
async fn test_trim_history() {
    let mut history = Vec::new();
    for i in 0..30 {
        history.push(json!({
            "role": "user",
            "parts": [{ "text": format!("user msg {}", i) }]
        }));
        history.push(json!({
            "role": "model",
            "parts": [{ "text": format!("model msg {}", i) }]
        }));
    }

    let trimmed = campus_policy_backend::chat::trim_history(history, 12, 4000);
    assert!(trimmed.len() <= 24);
    assert_eq!(trimmed.last().unwrap()["role"], "model");
}

// 模拟 HTTP 端到端 SSE 问答集成测试
#[tokio::test]
async fn test_mock_upstream_sse_chat_flow() {
    // 搭建模拟 upstream Gemini 服务
    let mock_gemini_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mock_addr: SocketAddr = mock_gemini_listener.local_addr().unwrap();

    let mock_app = axum::Router::new()
        .route(
            "/models/{model_action}",
            axum::routing::post(|axum::Json(body): axum::Json<Value>| async move {
                let contents = body
                    .get("contents")
                    .and_then(|c| c.as_array())
                    .cloned()
                    .unwrap_or_default();
                let has_tool_resp = contents.iter().any(|m| {
                    m.get("role").and_then(|r| r.as_str()) == Some("user")
                        && m.get("parts")
                            .and_then(|p| p.as_array())
                            .is_some_and(|parts| {
                                parts.iter().any(|p| p.get("functionResponse").is_some())
                            })
                });

                let sse_data = if !has_tool_resp {
                    // 第一轮：发出 policy_search 工具调用
                    let resp_obj = json!({
                        "candidates": [{
                            "content": {
                                "role": "model",
                                "parts": [{
                                    "functionCall": {
                                        "name": "policy_search",
                                        "args": { "query": "三下乡 报名" },
                                        "id": "call-1"
                                    }
                                }]
                            }
                        }]
                    });
                    format!("data: {}\n\n", resp_obj)
                } else {
                    // 第二轮：返回最终带正规 EV1 与伪造 EV99 的回答
                    let text = "报名需要在6月22日前完成 [[EV1]]，伪造的 [[EV99]] 应被剔除。";
                    let resp_obj = json!({
                        "candidates": [{
                            "content": {
                                "role": "model",
                                "parts": [{
                                    "text": text
                                }]
                            }
                        }],
                        "usageMetadata": {
                            "promptTokenCount": 50,
                            "candidatesTokenCount": 20,
                            "thoughtsTokenCount": 0
                        }
                    });
                    format!("data: {}\n\n", resp_obj)
                };

                ([("content-type", "text/event-stream")], sse_data)
            }),
        )
        .route(
            "/embeddings",
            axum::routing::post(|| async move {
                // 模拟返回维度为 1024 的有效归一化向量
                let mut emb = vec![0.0f32; 1024];
                emb[0] = 1.0;
                axum::Json(json!({
                    "data": [{ "embedding": emb }],
                    "usage": { "total_tokens": 8 }
                }))
            }),
        );

    tokio::spawn(async move {
        axum::serve(mock_gemini_listener, mock_app).await.unwrap();
    });

    // 搭建后端服务
    let legacy_db = Path::new("tests/fixtures/legacy_data/campus.db");
    assert!(legacy_db.exists(), "必须存在 legacy_data/campus.db");
    let pool = DbPool::new(legacy_db, 2, 64).expect("初始化测试 DbPool 失败");

    let mut config = Config::from_env(None);
    config.data_dir = Path::new("tests/fixtures/legacy_data").to_path_buf();
    config.gemini_base_url = format!("http://{}", mock_addr);
    config.gemini_api_key = "test-key".to_string();
    config.siliconflow_base_url = format!("http://{}", mock_addr);
    config.siliconflow_api_key = "test-key".to_string();

    let secret = pool
        .setting_get("token_secret".into())
        .await
        .unwrap()
        .unwrap();
    let tokens = TokenService::from_hex_secret(&secret, 30).unwrap();
    let limiter = Arc::new(RateLimiter::new(100));
    let vectors = Arc::new(VectorIndex::new(config.data_dir.join("vectors")));

    let gemini = GeminiClient::with_client(
        reqwest::Client::new(),
        config.gemini_base_url.clone(),
        config.gemini_model.clone(),
        config.gemini_api_key.clone(),
    );
    let agent = Arc::new(Agent::with_gemini(
        pool.clone(),
        Arc::clone(&vectors),
        config.clone(),
        gemini,
    ));
    let chats = Arc::new(ChatManager::new(1, 10));

    let user_token = tokens.issue("user", "test-user");
    let tokens = Arc::new(tokio::sync::RwLock::new(tokens));
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
    let server_addr: SocketAddr = listener.local_addr().unwrap();
    let base_url = format!("http://{}", server_addr);

    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let client = reqwest::Client::new();
    let chat_body = json!({
        "question": "三下乡什么时候报名？",
        "messages": [],
        "scope": { "mode": "auto" },
        "profile": { "college": "信息工程学院", "entry_year": "2025" }
    });

    let res = client
        .post(format!("{}/api/chat", base_url))
        .header(AUTHORIZATION, format!("Bearer {}", user_token))
        .json(&chat_body)
        .send()
        .await
        .unwrap();

    assert_eq!(res.status(), 200);

    let mut stream = res.bytes_stream();
    let mut events = Vec::new();
    let mut buffer = String::new();

    while let Some(chunk_res) = stream.next().await {
        let chunk = chunk_res.unwrap();
        buffer.push_str(&String::from_utf8_lossy(&chunk));

        while let Some(pos) = buffer.find("\n\n") {
            let msg: String = buffer.drain(..pos + 2).collect();
            for line in msg.lines() {
                let trimmed = line.trim();
                if let Some(stripped) = trimmed.strip_prefix("data:") {
                    let json_str = stripped.trim();
                    if let Ok(ev) = serde_json::from_str::<Value>(json_str) {
                        events.push(ev);
                    }
                }
            }
        }
    }

    let event_names: Vec<&str> = events
        .iter()
        .filter_map(|e| e.get("event").and_then(|v| v.as_str()))
        .collect();

    assert!(event_names.contains(&"started"));
    assert!(event_names.contains(&"generating"));
    assert!(event_names.contains(&"stage"));
    assert!(event_names.contains(&"metrics"));
    assert!(event_names.contains(&"citations"));
    assert!(event_names.contains(&"done"));

    // 验证伪造的 [[EV99]] 被剔除，而合法的 [[EV1]] 得到保留
    let done_event = events.iter().find(|e| e["event"] == "done").unwrap();
    let text = done_event["text"].as_str().unwrap();
    assert!(text.contains("[[EV1]]"));
    assert!(!text.contains("[[EV99]]"));

    // 验证引用数据
    let citations_event = events.iter().find(|e| e["event"] == "citations").unwrap();
    let citations = citations_event["citations"].as_array().unwrap();
    assert!(!citations.is_empty());
    assert_eq!(citations[0]["evidence_id"], "EV1");
}

#[tokio::test]
async fn test_mock_upstream_expand_request_flow() {
    let mock_gemini_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mock_addr: SocketAddr = mock_gemini_listener.local_addr().unwrap();

    let mock_app = axum::Router::new().route(
        "/models/{model_action}",
        axum::routing::post(|axum::Json(_body): axum::Json<Value>| async move {
            let text = "范围不足，[[EXPAND_REQUEST:需要检索其他学院的相关规定]]";
            let resp_obj = json!({
                "candidates": [{
                    "content": {
                        "role": "model",
                        "parts": [{ "text": text }]
                    }
                }],
                "usageMetadata": {
                    "promptTokenCount": 30,
                    "candidatesTokenCount": 15,
                    "thoughtsTokenCount": 0
                }
            });
            (
                [("content-type", "text/event-stream")],
                format!("data: {}\n\n", resp_obj),
            )
        }),
    );

    tokio::spawn(async move {
        axum::serve(mock_gemini_listener, mock_app).await.unwrap();
    });

    let legacy_db = Path::new("tests/fixtures/legacy_data/campus.db");
    let pool = DbPool::new(legacy_db, 2, 64).expect("初始化测试 DbPool 失败");

    let mut config = Config::from_env(None);
    config.data_dir = Path::new("tests/fixtures/legacy_data").to_path_buf();
    config.gemini_base_url = format!("http://{}", mock_addr);
    config.gemini_api_key = "test-key".to_string();

    let secret = pool
        .setting_get("token_secret".into())
        .await
        .unwrap()
        .unwrap();
    let tokens = TokenService::from_hex_secret(&secret, 30).unwrap();
    let limiter = Arc::new(RateLimiter::new(100));
    let vectors = Arc::new(VectorIndex::new(config.data_dir.join("vectors")));

    let gemini = GeminiClient::with_client(
        reqwest::Client::new(),
        config.gemini_base_url.clone(),
        config.gemini_model.clone(),
        config.gemini_api_key.clone(),
    );
    let agent = Arc::new(Agent::with_gemini(
        pool.clone(),
        Arc::clone(&vectors),
        config.clone(),
        gemini,
    ));
    let chats = Arc::new(ChatManager::new(1, 10));
    let user_token = tokens.issue("user", "test-user");
    let tokens = Arc::new(tokio::sync::RwLock::new(tokens));
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
    let server_addr: SocketAddr = listener.local_addr().unwrap();
    let base_url = format!("http://{}", server_addr);

    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let client = reqwest::Client::new();
    let chat_body = json!({
        "question": "其他学院的规定是什么？",
        "messages": [],
        "scope": { "mode": "domains", "domains": ["安全纪律"] },
        "profile": {}
    });

    let res = client
        .post(format!("{}/api/chat", base_url))
        .header(AUTHORIZATION, format!("Bearer {}", user_token))
        .json(&chat_body)
        .send()
        .await
        .unwrap();

    assert_eq!(res.status(), 200);

    let mut stream = res.bytes_stream();
    let mut events = Vec::new();
    let mut buffer = String::new();

    while let Some(chunk_res) = stream.next().await {
        let chunk = chunk_res.unwrap();
        buffer.push_str(&String::from_utf8_lossy(&chunk));

        while let Some(pos) = buffer.find("\n\n") {
            let msg: String = buffer.drain(..pos + 2).collect();
            for line in msg.lines() {
                let trimmed = line.trim();
                if let Some(stripped) = trimmed.strip_prefix("data:") {
                    let json_str = stripped.trim();
                    if let Ok(ev) = serde_json::from_str::<Value>(json_str) {
                        events.push(ev);
                    }
                }
            }
        }
    }

    let expand_ev = events
        .iter()
        .find(|e| e["event"] == "expand_request")
        .unwrap();
    assert_eq!(expand_ev["reason"], "需要检索其他学院的相关规定");

    // Spec 6: expand_request 后不发成功 done 事件
    assert!(!events.iter().any(|e| e["event"] == "done"));
}

#[tokio::test]
async fn test_mock_upstream_error_flow() {
    let mock_gemini_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mock_addr: SocketAddr = mock_gemini_listener.local_addr().unwrap();

    let mock_app = axum::Router::new().route(
        "/models/{model_action}",
        axum::routing::post(|| async move {
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "Internal Error",
            )
        }),
    );

    tokio::spawn(async move {
        axum::serve(mock_gemini_listener, mock_app).await.unwrap();
    });

    let legacy_db = Path::new("tests/fixtures/legacy_data/campus.db");
    let pool = DbPool::new(legacy_db, 2, 64).expect("初始化测试 DbPool 失败");

    let mut config = Config::from_env(None);
    config.data_dir = Path::new("tests/fixtures/legacy_data").to_path_buf();
    config.gemini_base_url = format!("http://{}", mock_addr);
    config.gemini_api_key = "test-key".to_string();

    let secret = pool
        .setting_get("token_secret".into())
        .await
        .unwrap()
        .unwrap();
    let tokens = TokenService::from_hex_secret(&secret, 30).unwrap();
    let limiter = Arc::new(RateLimiter::new(100));
    let vectors = Arc::new(VectorIndex::new(config.data_dir.join("vectors")));

    let gemini = GeminiClient::with_client(
        reqwest::Client::new(),
        config.gemini_base_url.clone(),
        config.gemini_model.clone(),
        config.gemini_api_key.clone(),
    );
    let agent = Arc::new(Agent::with_gemini(
        pool.clone(),
        Arc::clone(&vectors),
        config.clone(),
        gemini,
    ));
    let chats = Arc::new(ChatManager::new(1, 10));
    let user_token = tokens.issue("user", "test-user");
    let tokens = Arc::new(tokio::sync::RwLock::new(tokens));
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
    let server_addr: SocketAddr = listener.local_addr().unwrap();
    let base_url = format!("http://{}", server_addr);

    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let client = reqwest::Client::new();
    let chat_body = json!({
        "question": "触发上游错误",
        "messages": [],
        "scope": {},
        "profile": {}
    });

    let res = client
        .post(format!("{}/api/chat", base_url))
        .header(AUTHORIZATION, format!("Bearer {}", user_token))
        .json(&chat_body)
        .send()
        .await
        .unwrap();

    assert_eq!(res.status(), 200);

    let mut stream = res.bytes_stream();
    let mut events = Vec::new();
    let mut buffer = String::new();

    while let Some(chunk_res) = stream.next().await {
        let chunk = chunk_res.unwrap();
        buffer.push_str(&String::from_utf8_lossy(&chunk));

        while let Some(pos) = buffer.find("\n\n") {
            let msg: String = buffer.drain(..pos + 2).collect();
            for line in msg.lines() {
                let trimmed = line.trim();
                if let Some(stripped) = trimmed.strip_prefix("data:") {
                    let json_str = stripped.trim();
                    if let Ok(ev) = serde_json::from_str::<Value>(json_str) {
                        events.push(ev);
                    }
                }
            }
        }
    }

    let error_ev = events.iter().find(|e| e["event"] == "error").unwrap();
    assert!(error_ev["message"].as_str().unwrap().contains("500"));
    // 错误后不应发送 done
    assert!(!events.iter().any(|e| e["event"] == "done"));
}
