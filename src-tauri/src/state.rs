//! 全局运行状态：单任务互斥、取消标志、模态框回信通道

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Mutex, MutexGuard};

use crate::types::UserReply;

/// 应用全局状态（由 `tauri::Builder::manage` 注入）
pub struct AppState {
    /// 同一时间只允许一个任务在跑
    pub busy: AtomicBool,
    /// 取消请求标志（后台线程轮询）
    pub cancel: AtomicBool,
    /// 当前挂起的模态框回信通道；前端点按钮后由此送回
    pub reply: Mutex<Option<Sender<UserReply>>>,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            busy: AtomicBool::new(false),
            cancel: AtomicBool::new(false),
            reply: Mutex::new(None),
        }
    }

    /// 尝试占用任务槽位；已有任务时返回 false
    pub fn try_acquire(&self) -> bool {
        self.busy
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    pub fn release(&self) {
        self.busy.store(false, Ordering::SeqCst);
    }

    pub fn is_busy(&self) -> bool {
        self.busy.load(Ordering::SeqCst)
    }

    pub fn request_cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }

    pub fn clear_cancel(&self) {
        self.cancel.store(false, Ordering::SeqCst);
    }

    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }

    /// 设置当前模态框的回信通道
    pub fn set_reply_sender(&self, s: Option<Sender<UserReply>>) {
        *lock(&self.reply) = s;
    }

    /// 前端回信；没有挂起弹窗时返回 false
    pub fn send_reply(&self, r: UserReply) -> bool {
        let guard = lock(&self.reply);
        match guard.as_ref() {
            Some(tx) => tx.send(r).is_ok(),
            None => false,
        }
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

/// 抗中毒加锁：即使某个线程 panic 也不会让整个应用死锁
pub fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}
