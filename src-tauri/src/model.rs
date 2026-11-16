//! M0 数据契约：前后端共享的序列化结构。
//! 对应设计文档 §7（表结构）、§8（接口与进度事件 schema）。

use serde::Serialize;

/// 统一错误返回 { code, message }，前端按 code 分支，不解析文本。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppError {
    pub code: String,
    pub message: String,
}

impl AppError {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_string(),
            message: message.into(),
        }
    }
}

impl From<rusqlite::Error> for AppError {
    fn from(e: rusqlite::Error) -> Self {
        Self::new("DB_ERROR", e.to_string())
    }
}

impl From<std::io::Error> for AppError {
    fn from(e: std::io::Error) -> Self {
        Self::new("IO_ERROR", e.to_string())
    }
}

pub type AppResult<T> = Result<T, AppError>;

/// 每 80ms 聚合一次的进度事件。字段量恒定，与文件数 N 无关（§3.1）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Progress {
    pub job_id: String,
    /// scanning | executing | undo_prechecking | undoing
    pub phase: String,
    /// in_place | to_dir
    pub output_mode: String,
    pub done: u64,
    pub total: u64,
    pub failed: u64,
    pub skipped: u64,
    pub bytes_done: u64,
    pub bytes_total: u64,
    /// 文件/秒
    pub speed: f64,
    /// 字节/秒
    pub bytes_per_sec: f64,
    pub eta_ms: u64,
    pub elapsed_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobInfo {
    pub id: String,
    pub status: String,
    pub root_path: String,
    pub recursive: bool,
    pub output_mode: String,
    pub output_dir: Option<String>,
    pub total: i64,
    pub bytes_total: i64,
    pub done: i64,
    pub bytes_done: i64,
    pub failed: i64,
    pub skipped: i64,
    pub conflicted: i64,
    pub undo_available: bool,
    pub undo_precheck_at: Option<i64>,
    pub undo_precheck_token: Option<String>,
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EntryRow {
    pub id: i64,
    pub seq: i64,
    pub src_path: String,
    pub src_name: String,
    pub src_size: i64,
    pub rel_dir: String,
    pub dst_path: String,
    pub dst_name: String,
    pub status: String,
    pub err_code: Option<String>,
    pub undo_check: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EntryPage {
    pub items: Vec<EntryRow>,
    pub next_cursor: Option<i64>,
    pub total: i64,
}

/// M1 实时预览条目：只算当前页，不写库、不重扫。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviewItem {
    pub id: i64,
    pub src_name: String,
    pub dst_name: Option<String>,
    pub status: String,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OpResult {
    pub job_id: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OkResult {
    pub ok: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanSummary {
    pub job_id: String,
    pub total: i64,
    pub planned: i64,
    pub conflicted: i64,
    pub bytes_total: i64,
    pub output_dir: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConflictCount {
    pub code: String,
    pub count: i64,
}

/// v1.3 跨会话撤销：复核报告（不含全量明细）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PrecheckResult {
    pub job_id: String,
    pub total: i64,
    pub ok_count: i64,
    /// 复核时发现「产物已不在、原始文件在位」的条目数：
    /// 即撤销执行中崩溃留下的「文件已还原、状态未回写」窗口，已自愈为 undone。
    pub already_undone: i64,
    pub conflicts: Vec<ConflictCount>,
    pub token: String,
    pub expires_at: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UndoResult {
    pub job_id: String,
    pub undone: i64,
    pub conflicts: i64,
    pub status: String,
}
