use campus_policy_backend::agent::Agent;
use campus_policy_backend::api::{AppState, create_router};
use campus_policy_backend::auth::{RateLimiter, TokenService};
use campus_policy_backend::chat::ChatManager;
use campus_policy_backend::config::Config;
use campus_policy_backend::db::DbPool;
use campus_policy_backend::vectors::VectorIndex;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;

#[tokio::main(worker_threads = 1)]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::from_env(None);
    config.ensure_startup().map_err(std::io::Error::other)?;
    println!("启动 campus_policy_backend (Rust 版本)...");
    println!("数据目录: {:?}", config.data_dir);
    println!("监听地址: {}:{}", config.host, config.port);

    let db_path = config.data_dir.join("campus.db");
    let db = DbPool::new(&db_path, 2, 64)?;

    // 初始化管理员密码（若尚未设置）
    if db
        .setting_get("admin_password_hash".into())
        .await?
        .is_none()
    {
        let hash = campus_policy_backend::auth::hash_password(&config.initial_admin_password, None);
        db.setting_set("admin_password_hash".into(), hash).await?;
        db.audit(
            "system".into(),
            "init_admin_password".into(),
            "初始管理员密码已设置".into(),
        )
        .await?;
    }

    // 初始化 token 服务
    let secret = match db.setting_get("token_secret".into()).await? {
        Some(s) => s,
        None => {
            let mut sec_bytes = [0u8; 32];
            rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut sec_bytes);
            let s = hex::encode(sec_bytes);
            db.setting_set("token_secret".into(), s.clone()).await?;
            s
        }
    };

    let token_svc = TokenService::from_hex_secret(&secret, config.token_ttl_days)
        .map_err(std::io::Error::other)?;
    let tokens = Arc::new(tokio::sync::RwLock::new(token_svc));

    let limiter = Arc::new(RateLimiter::new(10_000));
    let vectors = Arc::new(VectorIndex::new(config.data_dir.join("vectors")));
    let agent = Arc::new(Agent::new(db.clone(), Arc::clone(&vectors), config.clone()));
    let chats = Arc::new(ChatManager::new(
        config.chat_concurrency,
        config.chat_queue_max,
    ));
    let maintenance = Arc::new(campus_policy_backend::maintenance::MaintenanceState::new());

    let state = AppState {
        config: config.clone(),
        db,
        tokens,
        limiter,
        vectors,
        agent,
        chats,
        maintenance,
    };

    let app = create_router(state);
    let addr: SocketAddr = format!("{}:{}", config.host, config.port).parse()?;
    let listener = TcpListener::bind(addr).await?;
    println!("服务已就绪，正在监听: http://{}", addr);

    axum::serve(listener, app.into_make_service_with_connect_info::<std::net::SocketAddr>()).await?;
    Ok(())
}
