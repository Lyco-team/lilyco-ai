//! 生产护栏：串行队列（并发度 = 1）、常量时间 Bearer 鉴权与滑动 1 小时限流。

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// 滑动 1 小时窗口限流器
pub struct HourlyLimiter {
    limit: u32,
    stamps: Mutex<VecDeque<Instant>>,
}

impl HourlyLimiter {
    pub fn new(limit: u32) -> Self {
        Self {
            limit,
            stamps: Mutex::new(VecDeque::new()),
        }
    }

    pub fn limit(&self) -> u32 {
        self.limit
    }

    /// 尝试占一个名额；超限时返回 `Err(还需等待的秒数)`
    pub fn try_take(&self) -> Result<(), u64> {
        let mut q = self.stamps.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        let window = Duration::from_secs(3600);
        while q.front().is_some_and(|t| now.duration_since(*t) >= window) {
            q.pop_front();
        }
        if self.limit > 0 && q.len() >= self.limit as usize {
            let retry_after = q
                .front()
                .map(|oldest| {
                    window
                        .saturating_sub(now.duration_since(*oldest))
                        .as_secs()
                        .max(1)
                })
                .unwrap_or(60);
            return Err(retry_after);
        }
        q.push_back(now);
        Ok(())
    }

    pub fn count_last_hour(&self) -> usize {
        let mut q = self.stamps.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        let window = Duration::from_secs(3600);
        while q.front().is_some_and(|t| now.duration_since(*t) >= window) {
            q.pop_front();
        }
        q.len()
    }
}

/// RAII 排队计数守卫：离开作用域自动减 1
pub struct QueueGuard {
    counter: Arc<AtomicUsize>,
}

impl Drop for QueueGuard {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::SeqCst);
    }
}

/// 并发度 = 1 的浏览器串行门闩（带最大排队深度保护）
pub struct SerialQueue {
    gate: Arc<Semaphore>,
    depth: Arc<AtomicUsize>,
    max_queue: usize,
}

impl SerialQueue {
    pub fn new(max_queue: usize) -> Self {
        Self {
            gate: Arc::new(Semaphore::new(1)),
            depth: Arc::new(AtomicUsize::new(0)),
            max_queue: max_queue.max(1),
        }
    }

    pub fn depth(&self) -> usize {
        self.depth.load(Ordering::SeqCst)
    }

    pub fn max_queue(&self) -> usize {
        self.max_queue
    }

    /// 尝试入队；超过 `max_queue` 返回 `Err((当前深度, 上限))`
    pub fn enter(&self) -> Result<QueueGuard, (usize, usize)> {
        let prev = self.depth.fetch_add(1, Ordering::SeqCst);
        let guard = QueueGuard {
            counter: self.depth.clone(),
        };
        if prev >= self.max_queue {
            drop(guard);
            return Err((prev, self.max_queue));
        }
        Ok(guard)
    }

    pub fn gate(&self) -> Arc<Semaphore> {
        self.gate.clone()
    }

    pub async fn acquire(&self) -> Option<OwnedSemaphorePermit> {
        self.gate.clone().acquire_owned().await.ok()
    }
}

/// 常量时间字节比对，避免通过响应时间侧信道猜 API Key
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (&x, &y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// 校验 `Authorization: Bearer <key>` 头部值（`expected` 为 `None` 时放行）
pub fn verify_bearer(auth_header: Option<&str>, expected: Option<&str>) -> bool {
    let Some(exp) = expected else {
        return true;
    };
    let provided = auth_header
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(str::trim)
        .unwrap_or("");
    ct_eq(provided.as_bytes(), exp.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_check_uses_exact_match() {
        assert!(verify_bearer(None, None));
        assert!(!verify_bearer(None, Some("k1")));
        assert!(!verify_bearer(Some("Bearer wrong"), Some("k1")));
        assert!(verify_bearer(Some("Bearer k1"), Some("k1")));
    }

    #[test]
    fn serial_queue_caps_and_releases() {
        let q = SerialQueue::new(2);
        let g1 = q.enter().unwrap();
        let g2 = q.enter().unwrap();
        assert!(q.enter().is_err());
        assert_eq!(q.depth(), 2);
        drop(g1);
        assert_eq!(q.depth(), 1);
        drop(g2);
        assert_eq!(q.depth(), 0);
    }

    #[test]
    fn hourly_limiter_blocks_over_cap() {
        let l = HourlyLimiter::new(2);
        assert!(l.try_take().is_ok());
        assert!(l.try_take().is_ok());
        assert!(l.try_take().is_err());
        assert_eq!(l.count_last_hour(), 2);
    }
}
