//! 任务运行时状态 + 流式进度聚合器。
//! 设计文档 §4.3 线程模型、§6.5 进度事件、§10「进度事件 O(时长) 而非 O(N)」。
//!
//! 关键：文件处理线程只做原子自增，绝不逐文件发事件；
//! 独立 emitter 线程每 80ms 读一次快照并推送（≤12.5 次/秒）。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tauri::ipc::Channel;
use uuid::Uuid;

use crate::db::Db;
use crate::model::Progress;

pub const EMIT_INTERVAL_MS: u64 = 80;
/// v1.3 遗留常量：自 v1.6 起撤销链路改由 `maintenance::load_settings` 读取
/// settings 表，这里的数值仅作为「设置表为空时」的默认值来源说明，运行时不再直接引用。
pub const UNDO_PRECHECK_TTL_SEC: i64 = 300;
/// v1.3 遗留常量，同上；运行时以 settings.history_retention_days 为准。
pub const HISTORY_RETENTION_DAYS: i64 = 30;

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

/// 轻量取消令牌。M0 用 AtomicBool；M1 可替换为 tokio-util::CancellationToken
/// 以获得「大文件分块写入 + 组合取消」等更细的控制，语义等价。
#[derive(Clone)]
pub struct JobRuntime {
    pub done: Arc<AtomicU64>,
    pub total: Arc<AtomicU64>,
    pub bytes_done: Arc<AtomicU64>,
    pub bytes_total: Arc<AtomicU64>,
    pub failed: Arc<AtomicU64>,
    pub skipped: Arc<AtomicU64>,
    pub cancel: Arc<AtomicBool>,
    /// v1.15：执行暂停标志。只在批次边界 / 条目边界读取，不改变并发语义。
    pub paused: Arc<AtomicBool>,
    pub finished: Arc<AtomicBool>,
    pub started_at: Arc<Instant>,
}

impl JobRuntime {
    pub fn new() -> Self {
        Self {
            done: Arc::new(AtomicU64::new(0)),
            total: Arc::new(AtomicU64::new(0)),
            bytes_done: Arc::new(AtomicU64::new(0)),
            bytes_total: Arc::new(AtomicU64::new(0)),
            failed: Arc::new(AtomicU64::new(0)),
            skipped: Arc::new(AtomicU64::new(0)),
            cancel: Arc::new(AtomicBool::new(false)),
            paused: Arc::new(AtomicBool::new(false)),
            finished: Arc::new(AtomicBool::new(false)),
            started_at: Arc::new(Instant::now()),
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    /// 暂停期间在边界等待（50ms 轮询，不忙等、不持 DB 锁）。
    /// 返回 false 表示等待中被取消，调用方应立即跳出循环。
    pub fn wait_while_paused(&self) -> bool {
        while self.paused.load(Ordering::Relaxed) {
            if self.is_cancelled() {
                return false;
            }
            thread::sleep(Duration::from_millis(50));
        }
        !self.is_cancelled()
    }
}

impl Default for JobRuntime {
    fn default() -> Self {
        Self::new()
    }
}

/// AppState 可廉价 clone：内部全是 Arc，后台线程持有自己的句柄即可。
#[derive(Clone)]
pub struct AppState {
    pub db: Arc<Db>,
    pub runtimes: Arc<Mutex<HashMap<String, JobRuntime>>>,
    /// v1.3：仅审计归属，不参与撤销资格判定。
    pub session_id: String,
}

impl AppState {
    pub fn new(db: Db) -> Self {
        Self {
            db: Arc::new(db),
            runtimes: Arc::new(Mutex::new(HashMap::new())),
            session_id: Uuid::new_v4().to_string(),
        }
    }

    pub fn set_runtime(&self, job_id: &str, rt: JobRuntime) {
        self.runtimes
            .lock()
            .expect("runtimes mutex poisoned")
            .insert(job_id.to_string(), rt);
    }

    pub fn get_runtime(&self, job_id: &str) -> Option<JobRuntime> {
        self.runtimes
            .lock()
            .expect("runtimes mutex poisoned")
            .get(job_id)
            .cloned()
    }

    pub fn clear_runtime(&self, job_id: &str) {
        self.runtimes
            .lock()
            .expect("runtimes mutex poisoned")
            .remove(job_id);
    }

    /// 应用启动时的会话收敛（设计文档 §6.6.4）：
    /// 只把非终态任务标记为 interrupted，**不清零历史任务的 undo_available**（v1.3 关键变更）。
    pub fn reconcile_on_startup(&self) {
        let conn = self.db.lock();
        let _ = conn.execute_batch(
            "UPDATE jobs SET status='interrupted', undo_precheck_token=NULL
             WHERE status IN ('scanning','planned','executing','undoing','undo_prechecking');
             UPDATE entries SET undo_check=NULL WHERE undo_check IS NOT NULL;",
        );
    }
}

/// 定时聚合推送。与文件处理逻辑完全解耦，事件数与文件数无关。
pub fn spawn_emitter(
    channel: Channel<Progress>,
    rt: JobRuntime,
    job_id: String,
    phase: String,
    output_mode: String,
) {
    thread::spawn(move || {
        let mut last_done = 0u64;
        let mut last_bytes = 0u64;
        let mut last_tick = Instant::now();

        loop {
            thread::sleep(Duration::from_millis(EMIT_INTERVAL_MS));
            let now = Instant::now();
            let dt = now.duration_since(last_tick).as_secs_f64().max(1e-6);

            let done = rt.done.load(Ordering::Relaxed);
            let bytes_done = rt.bytes_done.load(Ordering::Relaxed);
            let total = rt.total.load(Ordering::Relaxed);
            let bytes_total = rt.bytes_total.load(Ordering::Relaxed);
            let speed = done.saturating_sub(last_done) as f64 / dt;
            let bytes_per_sec = bytes_done.saturating_sub(last_bytes) as f64 / dt;

            // ETA：① 按文件速率，② 按字节速率（设计文档 §8）。
            let eta_ms = if output_mode == "to_dir" && bytes_per_sec > 0.0 {
                (((bytes_total.saturating_sub(bytes_done)) as f64 / bytes_per_sec) * 1000.0) as u64
            } else if speed > 0.0 {
                (((total.saturating_sub(done)) as f64 / speed) * 1000.0) as u64
            } else {
                0
            };

            let finished = rt.finished.load(Ordering::Relaxed);
            let p = Progress {
                job_id: job_id.clone(),
                phase: phase.clone(),
                output_mode: output_mode.clone(),
                done,
                total,
                failed: rt.failed.load(Ordering::Relaxed),
                skipped: rt.skipped.load(Ordering::Relaxed),
                bytes_done,
                bytes_total,
                speed,
                bytes_per_sec,
                eta_ms,
                elapsed_ms: now.duration_since(*rt.started_at).as_millis() as u64,
            };
            // 前端窗口关闭后 send 会失败，忽略即可，不影响后端任务收尾。
            let _ = channel.send(p);

            last_done = done;
            last_bytes = bytes_done;
            last_tick = now;
            if finished {
                break;
            }
        }
    });
}
