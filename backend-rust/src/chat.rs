use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;
use futures_util::FutureExt;
use tokio::sync::{Mutex, Notify, mpsc, watch};

pub type JobEvent = serde_json::Value;

#[derive(Debug, Clone)]
pub enum ChatManagerError { ClientAlreadyRunning, QueueFull(usize, usize) }
impl std::fmt::Display for ChatManagerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ClientAlreadyRunning => write!(f, "同一浏览器已有进行中的问答，请等待完成或先取消"),
            Self::QueueFull(r, q) => write!(f, "当前排队已满（运行 {} + 队列 {}），请稍后再试", r, q),
        }
    }
}
impl std::error::Error for ChatManagerError {}

pub struct ChatJob {
    pub request_id: String,
    pub client_id: String,
    pub tx: mpsc::Sender<JobEvent>,
    cancel: watch::Sender<bool>,
    finished: watch::Sender<bool>,
}

#[derive(Default)]
struct QueueState {
    running: HashMap<String, Arc<ChatJob>>,
    waiting: VecDeque<Arc<ChatJob>>,
    by_client: HashMap<String, Arc<ChatJob>>,
}

pub struct ChatManager {
    concurrency: usize,
    queue_max: usize,
    state: Mutex<QueueState>,
    changed: Notify,
}

impl ChatManager {
    pub fn new(concurrency: usize, queue_max: usize) -> Self {
        Self { concurrency: concurrency.max(1), queue_max, state: Mutex::new(QueueState::default()), changed: Notify::new() }
    }

    pub async fn submit<F, Fut>(self: &Arc<Self>, client_id: String, timeout_s: u64, runner: F)
        -> Result<(Arc<ChatJob>, mpsc::Receiver<JobEvent>), ChatManagerError>
    where F: FnOnce(Arc<ChatJob>, mpsc::Sender<JobEvent>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let mut state = self.state.lock().await;
        if state.by_client.contains_key(&client_id) { return Err(ChatManagerError::ClientAlreadyRunning); }
        if state.by_client.len() >= self.concurrency + self.queue_max {
            return Err(ChatManagerError::QueueFull(state.running.len(), state.waiting.len()));
        }
        let (tx, rx) = mpsc::channel(64);
        let (cancel, _) = watch::channel(false);
        let (finished, _) = watch::channel(false);
        let job = Arc::new(ChatJob { request_id: hex::encode(rand::random::<[u8; 16]>()), client_id: client_id.clone(), tx, cancel, finished });
        state.by_client.insert(client_id, job.clone());
        let position = if state.running.len() < self.concurrency {
            state.running.insert(job.request_id.clone(), job.clone());
            0
        } else {
            state.waiting.push_back(job.clone());
            state.waiting.len()
        };
        drop(state);
        let manager = self.clone();
        let task_job = job.clone();
        tokio::spawn(async move { manager.execute(task_job, position, timeout_s, runner).await; });
        Ok((job, rx))
    }

    async fn execute<F, Fut>(self: Arc<Self>, job: Arc<ChatJob>, position: usize, timeout_s: u64, runner: F)
    where F: FnOnce(Arc<ChatJob>, mpsc::Sender<JobEvent>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let mut cancellation = job.cancel.subscribe();
        let work = async {
            if position > 0 {
                let _ = job.tx.send(serde_json::json!({"event":"queued", "position":position, "request_id":job.request_id})).await;
            }
            loop {
                let notified = self.changed.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if self.state.lock().await.running.contains_key(&job.request_id) { break; }
                notified.await;
            }
            let _ = job.tx.send(serde_json::json!({"event":"started", "request_id":job.request_id})).await;
            runner(job.clone(), job.tx.clone()).await;
        };
        let result = tokio::select! {
            biased;
            _ = async { if !*cancellation.borrow() { let _ = cancellation.changed().await; } } => Some("已取消"),
            _ = job.tx.closed() => None,
            result = tokio::time::timeout(Duration::from_secs(timeout_s), std::panic::AssertUnwindSafe(work).catch_unwind()) => {
                match result { Ok(Ok(())) => None, Ok(Err(_)) => Some("问答任务异常退出"), Err(_) => Some("问答超时，请重试") }
            }
        };
        // Release the slot only after the runner future (and its upstream requests) was dropped.
        {
            let mut state = self.state.lock().await;
            state.running.remove(&job.request_id);
            state.waiting.retain(|j| j.request_id != job.request_id);
            state.by_client.remove(&job.client_id);
            while state.running.len() < self.concurrency {
                let Some(next) = state.waiting.pop_front() else { break; };
                state.running.insert(next.request_id.clone(), next);
            }
        }
        self.changed.notify_waiters();
        job.finished.send_replace(true);
        let ending = async {
            if let Some(message) = result { let _ = job.tx.send(serde_json::json!({"event":"error", "message":message})).await; }
            let _ = job.tx.send(serde_json::json!({"event":"__end__"})).await;
        };
        let _ = tokio::time::timeout(Duration::from_secs(2), ending).await;
    }

    pub async fn cancel(&self, request_id: &str, client_id: &str) -> bool {
        let job = self.state.lock().await.by_client.get(client_id).cloned();
        let Some(job) = job.filter(|j| j.request_id == request_id) else { return false; };
        let mut finished = job.finished.subscribe();
        job.cancel.send_replace(true);
        if !*finished.borrow() { let _ = finished.changed().await; }
        true
    }

    pub async fn detach(&self, client_id: &str) {
        let job = self.state.lock().await.by_client.get(client_id).cloned();
        if let Some(job) = job { self.cancel(&job.request_id, client_id).await; }
    }

    pub async fn cancel_all(&self) {
        let jobs: Vec<_> = self.state.lock().await.by_client.values().cloned().collect();
        for job in &jobs { job.cancel.send_replace(true); }
        for job in jobs {
            let mut finished = job.finished.subscribe();
            if !*finished.borrow() { let _ = finished.changed().await; }
        }
    }

    pub async fn counts(&self) -> (usize, usize) {
        let state = self.state.lock().await;
        (state.running.len(), state.waiting.len())
    }
}

pub fn trim_history(history: Vec<serde_json::Value>, max_rounds: usize, max_chars: usize) -> Vec<serde_json::Value> {
    let count_chars = |msgs: &[serde_json::Value]| -> usize {
        msgs.iter().filter_map(|m| m.get("parts").and_then(|p| p.as_array())).flatten()
            .filter_map(|p| p.get("text").and_then(|t| t.as_str())).map(|s| s.chars().count()).sum()
    };
    let mut trimmed = history[history.len().saturating_sub(max_rounds * 2)..].to_vec();
    while !trimmed.is_empty() && count_chars(&trimmed) > max_chars { trimmed.remove(0); }
    trimmed
}
