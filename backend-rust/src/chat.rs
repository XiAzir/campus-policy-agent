use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinHandle;

pub type JobEvent = serde_json::Value;

#[derive(Debug, Clone)]
pub enum ChatManagerError {
    ClientAlreadyRunning,
    QueueFull(usize, usize),
}

impl std::fmt::Display for ChatManagerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ClientAlreadyRunning => {
                write!(f, "同一浏览器已有进行中的问答，请等待完成或先取消")
            }
            Self::QueueFull(running, queued) => {
                write!(
                    f,
                    "当前排队已满（运行 {} + 队列 {}），请稍后再试",
                    running, queued
                )
            }
        }
    }
}

impl std::error::Error for ChatManagerError {}

pub struct ChatJob {
    pub request_id: String,
    pub client_id: String,
    pub tx: mpsc::Sender<JobEvent>,
    pub handle: Mutex<Option<JoinHandle<()>>>,
}

pub struct ChatManager {
    concurrency: usize,
    queue_max: usize,
    running: Arc<Mutex<Option<Arc<ChatJob>>>>,
    waiting: Arc<Mutex<Vec<Arc<ChatJob>>>>,
    by_client: Arc<Mutex<HashMap<String, Arc<ChatJob>>>>,
}

impl ChatManager {
    pub fn new(concurrency: usize, queue_max: usize) -> Self {
        Self {
            concurrency,
            queue_max,
            running: Arc::new(Mutex::new(None)),
            waiting: Arc::new(Mutex::new(Vec::new())),
            by_client: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn submit<F, Fut>(
        self: &Arc<Self>,
        client_id: String,
        timeout_s: u64,
        runner: F,
    ) -> Result<(Arc<ChatJob>, mpsc::Receiver<JobEvent>), ChatManagerError>
    where
        F: FnOnce(Arc<ChatJob>, mpsc::Sender<JobEvent>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let mut by_client = self.by_client.lock().await;
        if by_client.contains_key(&client_id) {
            return Err(ChatManagerError::ClientAlreadyRunning);
        }

        let mut running = self.running.lock().await;
        let mut waiting = self.waiting.lock().await;

        let total = if running.is_some() { 1 } else { 0 } + waiting.len();
        if total >= self.concurrency + self.queue_max {
            return Err(ChatManagerError::QueueFull(
                self.concurrency,
                self.queue_max,
            ));
        }

        let request_id = hex::encode(rand::random::<[u8; 16]>());
        let (tx, rx) = mpsc::channel::<JobEvent>(128);

        let job = Arc::new(ChatJob {
            request_id: request_id.clone(),
            client_id: client_id.clone(),
            tx: tx.clone(),
            handle: Mutex::new(None),
        });

        by_client.insert(client_id.clone(), Arc::clone(&job));

        if running.is_none() {
            *running = Some(Arc::clone(&job));
            drop(running);
            drop(waiting);
            drop(by_client);

            Self::start_job(Arc::clone(self), Arc::clone(&job), timeout_s, runner).await;
        } else {
            let position = waiting.len() + 1;
            waiting.push(Arc::clone(&job));
            let _ = tx
                .send(serde_json::json!({
                    "event": "queued",
                    "position": position,
                    "request_id": request_id
                }))
                .await;

            drop(running);
            drop(waiting);
            drop(by_client);

            // 存入延后执行器并在 advance 时调度
            let self_clone = Arc::clone(self);
            let job_clone = Arc::clone(&job);
            tokio::spawn(async move {
                // 等待成为 running
                let mut interval = tokio::time::interval(Duration::from_millis(50));
                loop {
                    interval.tick().await;
                    let r = self_clone.running.lock().await;
                    let is_current = r
                        .as_ref()
                        .map(|curr| curr.request_id == job_clone.request_id)
                        .unwrap_or(false);
                    if is_current {
                        break;
                    }
                    let by_c = self_clone.by_client.lock().await;
                    if !by_c.contains_key(&job_clone.client_id) {
                        // 已被取消/断开
                        return;
                    }
                }
                Self::start_job(self_clone, job_clone, timeout_s, runner).await;
            });
        }

        Ok((job, rx))
    }

    async fn start_job<F, Fut>(manager: Arc<Self>, job: Arc<ChatJob>, timeout_s: u64, runner: F)
    where
        F: FnOnce(Arc<ChatJob>, mpsc::Sender<JobEvent>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let _ = job
            .tx
            .send(serde_json::json!({
                "event": "started",
                "request_id": job.request_id
            }))
            .await;

        let job_for_task = Arc::clone(&job);
        let mgr_for_task = Arc::clone(&manager);
        let task = tokio::spawn(async move {
            let tx = job_for_task.tx.clone();
            let run_fut = runner(Arc::clone(&job_for_task), tx.clone());
            let timeout_res = tokio::time::timeout(Duration::from_secs(timeout_s), run_fut).await;

            if timeout_res.is_err() {
                let _ = tx
                    .send(serde_json::json!({
                        "event": "error",
                        "message": "请求超时，任务已终止"
                    }))
                    .await;
            }

            let _ = tx.send(serde_json::json!({ "event": "__end__" })).await;
            mgr_for_task.advance(&job_for_task.client_id).await;
        });

        *job.handle.lock().await = Some(task);
    }

    async fn advance(&self, completed_client_id: &str) {
        let mut running = self.running.lock().await;
        let mut by_client = self.by_client.lock().await;
        let mut waiting = self.waiting.lock().await;

        if let Some(curr) = running.as_ref() {
            let is_match = curr.client_id == completed_client_id;
            if is_match {
                by_client.remove(completed_client_id);
                *running = None;
            }
        }

        while !waiting.is_empty() {
            let next_job = waiting.remove(0);
            if !by_client.contains_key(&next_job.client_id) {
                // 已被取消
                continue;
            }
            *running = Some(Arc::clone(&next_job));
            break;
        }
    }

    pub async fn cancel(&self, request_id: &str, client_id: &str) -> bool {
        let mut by_client = self.by_client.lock().await;
        let Some(job) = by_client.get(client_id).cloned() else {
            return false;
        };
        if job.request_id != request_id {
            return false;
        }

        by_client.remove(client_id);
        drop(by_client);

        let mut running = self.running.lock().await;
        let mut waiting = self.waiting.lock().await;

        let is_running = running
            .as_ref()
            .map(|r| r.request_id == request_id)
            .unwrap_or(false);

        if is_running {
            if let Some(handle) = job.handle.lock().await.take() {
                handle.abort();
            }
            *running = None;
            let _ = job
                .tx
                .send(serde_json::json!({ "event": "error", "message": "已取消" }))
                .await;
            let _ = job.tx.send(serde_json::json!({ "event": "__end__" })).await;

            // 调度下一个等待者
            while !waiting.is_empty() {
                let next_job = waiting.remove(0);
                let by_c = self.by_client.lock().await;
                if by_c.contains_key(&next_job.client_id) {
                    *running = Some(next_job);
                    break;
                }
            }
        } else {
            waiting.retain(|w| w.request_id != request_id);
            let _ = job
                .tx
                .send(serde_json::json!({ "event": "error", "message": "已取消" }))
                .await;
            let _ = job.tx.send(serde_json::json!({ "event": "__end__" })).await;
        }

        true
    }

    pub async fn detach(&self, client_id: &str) {
        let mut by_client = self.by_client.lock().await;
        let Some(job) = by_client.remove(client_id) else {
            return;
        };
        drop(by_client);

        let mut running = self.running.lock().await;
        let mut waiting = self.waiting.lock().await;

        let is_running = running
            .as_ref()
            .map(|r| r.client_id == client_id)
            .unwrap_or(false);

        if is_running {
            if let Some(handle) = job.handle.lock().await.take() {
                handle.abort();
            }
            *running = None;

            while !waiting.is_empty() {
                let next_job = waiting.remove(0);
                let by_c = self.by_client.lock().await;
                if by_c.contains_key(&next_job.client_id) {
                    *running = Some(next_job);
                    break;
                }
            }
        } else {
            waiting.retain(|w| w.client_id != client_id);
        }
    }

    pub async fn cancel_all(&self) {
        let mut by_client = self.by_client.lock().await;
        let mut running = self.running.lock().await;
        let mut waiting = self.waiting.lock().await;

        for job in by_client.values() {
            if let Some(handle) = job.handle.lock().await.take() {
                handle.abort();
            } else {
                let _ = job
                    .tx
                    .send(serde_json::json!({ "event": "error", "message": "资料库恢复，问答已中断" }))
                    .await;
                let _ = job.tx.send(serde_json::json!({ "event": "__end__" })).await;
            }
        }

        by_client.clear();
        *running = None;
        waiting.clear();
    }

    pub async fn counts(&self) -> (usize, usize) {
        let running = self.running.lock().await;
        let waiting = self.waiting.lock().await;
        (if running.is_some() { 1 } else { 0 }, waiting.len())
    }
}

pub fn trim_history(
    history: Vec<serde_json::Value>,
    max_rounds: usize,
    max_chars: usize,
) -> Vec<serde_json::Value> {
    let count_chars = |msgs: &[serde_json::Value]| -> usize {
        msgs.iter()
            .filter_map(|m| m.get("parts").and_then(|p| p.as_array()))
            .flat_map(|p| p.iter())
            .filter_map(|part| part.get("text").and_then(|t| t.as_str()))
            .map(|s| s.len())
            .sum()
    };

    let start_idx = history.len().saturating_sub(max_rounds * 2);
    let mut trimmed: Vec<serde_json::Value> = history[start_idx..].to_vec();

    while !trimmed.is_empty() && count_chars(&trimmed) > max_chars {
        trimmed.remove(0);
    }

    trimmed
}
