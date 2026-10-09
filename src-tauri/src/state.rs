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
    /// 对比校验是否正在跑（与备份任务**互相独立**，只防同时开两个对比）
    pub comparing: AtomicBool,
    /// 对比校验的取消标志 —— 独立于 `cancel`，
    /// 这样「对比进行中取消备份」/「备份中取消对比」不会互相误伤。
    pub compare_cancel: AtomicBool,
    /// 「跳过当前正在校验的文件」请求标志（校验阶段专用）。
    ///
    /// 与 `cancel` 是**两回事**：`cancel` 会中断整个任务，这个只放弃**当前这一个文件**
    /// 的校验、直接进入下一个。所以必须独立，不能让两者共用同一个标志。
    pub verify_skip: AtomicBool,
    /// 是否正处于「校验阶段」。`skip_current` 命令靠它判断能不能接受跳过请求 ——
    /// 拷贝阶段点「跳过」是没有意义的（那时根本没在校验）。
    pub in_verify: AtomicBool,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            busy: AtomicBool::new(false),
            cancel: AtomicBool::new(false),
            reply: Mutex::new(None),
            comparing: AtomicBool::new(false),
            compare_cancel: AtomicBool::new(false),
            verify_skip: AtomicBool::new(false),
            in_verify: AtomicBool::new(false),
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

    /// 尝试占用对比槽位；已有对比在跑时返回 false
    pub fn try_acquire_compare(&self) -> bool {
        self.comparing
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    pub fn release_compare(&self) {
        self.comparing.store(false, Ordering::SeqCst);
    }

    pub fn is_comparing(&self) -> bool {
        self.comparing.load(Ordering::SeqCst)
    }

    pub fn request_compare_cancel(&self) {
        self.compare_cancel.store(true, Ordering::SeqCst);
    }

    pub fn clear_compare_cancel(&self) {
        self.compare_cancel.store(false, Ordering::SeqCst);
    }

    /// 进入 / 退出校验阶段。退出时顺手清掉可能残留的跳过请求，
    /// 否则下一轮任务的**第一个**文件会被上一次的点击误跳过。
    pub fn set_in_verify(&self, on: bool) {
        self.in_verify.store(on, Ordering::SeqCst);
        if !on {
            self.verify_skip.store(false, Ordering::SeqCst);
        }
    }

    /// 请求跳过「当前正在校验的文件」。不在校验阶段时返回 false（不做任何事）。
    pub fn request_skip_current(&self) -> bool {
        if !self.in_verify.load(Ordering::SeqCst) {
            return false;
        }
        self.verify_skip.store(true, Ordering::SeqCst);
        true
    }

    /// 校验循环消费（取走）跳过请求。取走后标志复位，**只影响当前文件**。
    pub fn take_skip_current(&self) -> bool {
        self.verify_skip.swap(false, Ordering::SeqCst)
    }

    /// 清掉跳过请求（判定为跳过 / 任务收尾时调用）
    pub fn clear_skip_current(&self) {
        self.verify_skip.store(false, Ordering::SeqCst);
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
