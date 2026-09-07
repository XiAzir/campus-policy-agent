use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

#[derive(Debug, Default)]
pub struct MaintenanceState {
    pub restoring: AtomicBool,
    pub failed: AtomicBool,
    pub active_requests: AtomicUsize,
}

impl MaintenanceState {
    pub fn new() -> Self {
        Self {
            restoring: AtomicBool::new(false),
            failed: AtomicBool::new(false),
            active_requests: AtomicUsize::new(0),
        }
    }

    pub fn is_restoring(&self) -> bool {
        self.restoring.load(Ordering::Acquire)
    }

    pub fn is_failed(&self) -> bool {
        self.failed.load(Ordering::Acquire)
    }

    pub fn enter_request(&self) -> Result<(), &'static str> {
        if self.is_restoring() {
            if self.is_failed() {
                return Err("资料恢复失败，服务已锁定，请联系管理员");
            } else {
                return Err("资料库正在恢复，请稍后重试");
            }
        }
        self.active_requests.fetch_add(1, Ordering::SeqCst);
        // 双重检查
        if self.is_restoring() {
            self.active_requests.fetch_sub(1, Ordering::SeqCst);
            if self.is_failed() {
                return Err("资料恢复失败，服务已锁定，请联系管理员");
            } else {
                return Err("资料库正在恢复，请稍后重试");
            }
        }
        Ok(())
    }

    pub fn exit_request(&self) {
        self.active_requests.fetch_sub(1, Ordering::SeqCst);
    }

    /// 等待所有活动请求退出，最多等待 timeout_s 秒
    pub async fn drain(&self, timeout_s: u64) -> Result<(), &'static str> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_s);
        while self.active_requests.load(Ordering::SeqCst) > 1 {
            if tokio::time::Instant::now() > deadline {
                return Err("现有请求未及时结束，恢复未执行，请稍后重试");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Ok(())
    }
}
