//! Tauri Command 层（设计文档 §8）。所有大结果走「落库 + 游标分页」，
//! 没有任何接口返回全量路径数组。

use rusqlite::{params, Row};
use tauri::ipc::Channel;
use tauri::State;
use uuid::Uuid;

use crate::execute;
use crate::model::{
    AppError, AppResult, EntryPage, EntryRow, JobInfo, OkResult, OpResult, PlanSummary,
    PrecheckResult, PreviewItem, Progress, UndoResult,
};
use crate::plan;
use crate::rule::RuleSpec;
use crate::scan;
use crate::state::AppState;
use crate::undo;

const JOB_COLS: &str = "id, status, root_path, recursive, output_mode, output_dir,
    total, bytes_total, done, bytes_done, failed, skipped, conflicted,
    undo_available, undo_precheck_at, undo_precheck_token, created_at";

const ENTRY_COLS: &str = "id, seq, src_path, src_name, src_size, rel_dir,
    dst_path, dst_name, status, err_code, undo_check";

fn map_job(r: &Row<'_>) -> rusqlite::Result<JobInfo> {
    Ok(JobInfo {
        id: r.get(0)?,
        status: r.get(1)?,
        root_path: r.get(2)?,
        recursive: r.get::<_, i64>(3)? != 0,
        output_mode: r.get(4)?,
        output_dir: r.get(5)?,
        total: r.get(6)?,
        bytes_total: r.get(7)?,
        done: r.get(8)?,
        bytes_done: r.get(9)?,
        failed: r.get(10)?,
        skipped: r.get(11)?,
        conflicted: r.get(12)?,
        undo_available: r.get::<_, i64>(13)? != 0,
        undo_precheck_at: r.get(14)?,
        undo_precheck_token: r.get(15)?,
        created_at: r.get(16)?,
    })
}

fn map_entry(r: &Row<'_>) -> rusqlite::Result<EntryRow> {
    Ok(EntryRow {
        id: r.get(0)?,
        seq: r.get(1)?,
        src_path: r.get(2)?,
        src_name: r.get(3)?,
        src_size: r.get(4)?,
        rel_dir: r.get(5)?,
        dst_path: r.get(6)?,
        dst_name: r.get(7)?,
        status: r.get(8)?,
        err_code: r.get(9)?,
        undo_check: r.get(10)?,
    })
}

#[tauri::command]
pub fn scan_files(
    state: State<'_, AppState>,
    root_path: String,
    recursive: bool,
    paths: Option<Vec<String>>,
    channel: Channel<Progress>,
) -> AppResult<OpResult> {
    // M3：新建任务时低频触发 TTL 清理（只删过期终态 job 记录）。
    let _ = crate::maintenance::prune_history(state.inner());
    let job_id = format!("j_{}", Uuid::new_v4().simple());
    scan::start_scan(
        state.inner().clone(),
        job_id,
        root_path,
        recursive,
        paths,
        channel,
    )
}

#[tauri::command]
pub fn build_plan(
    state: State<'_, AppState>,
    job_id: String,
    output_mode: String,
    output_dir: Option<String>,
    rule: RuleSpec,
) -> AppResult<PlanSummary> {
    plan::build_plan(state.inner(), &job_id, &output_mode, output_dir, rule)
}

/// M1 实时预览：规则一改就重算当前页，不重扫、不写库。
#[tauri::command]
pub fn preview_rule(
    state: State<'_, AppState>,
    job_id: String,
    rule: RuleSpec,
    cursor: Option<i64>,
    limit: Option<i64>,
) -> AppResult<Vec<PreviewItem>> {
    plan::preview_rule(
        state.inner(),
        &job_id,
        rule,
        cursor.unwrap_or(0),
        limit.unwrap_or(200).clamp(1, 1_000),
    )
}

#[tauri::command]
pub fn start_execute(
    state: State<'_, AppState>,
    job_id: String,
    channel: Channel<Progress>,
) -> AppResult<OpResult> {
    execute::start_execute(state.inner().clone(), job_id, channel)
}

#[tauri::command]
pub fn cancel_job(state: State<'_, AppState>, job_id: String) -> AppResult<OkResult> {
    let ok = if let Some(rt) = state.get_runtime(&job_id) {
        rt.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        true
    } else {
        false
    };
    Ok(OkResult { ok })
}

/// v1.15：暂停 / 恢复执行。只置运行时标志，不改 jobs 状态、不动 DB 事务，
/// 因此崩溃恢复（reconcile_on_startup）与既有 M2 稳定性语义保持不变。
fn set_paused(state: State<'_, AppState>, job_id: &str, value: bool) -> AppResult<OkResult> {
    let ok = if let Some(rt) = state.get_runtime(job_id) {
        rt.paused.store(value, std::sync::atomic::Ordering::Relaxed);
        true
    } else {
        false
    };
    Ok(OkResult { ok })
}

#[tauri::command]
pub fn pause_job(state: State<'_, AppState>, job_id: String) -> AppResult<OkResult> {
    set_paused(state, &job_id, true)
}

#[tauri::command]
pub fn resume_job(state: State<'_, AppState>, job_id: String) -> AppResult<OkResult> {
    set_paused(state, &job_id, false)
}

#[tauri::command]
pub fn get_job(state: State<'_, AppState>, job_id: String) -> AppResult<JobInfo> {
    let conn = state.db.lock();
    let sql = format!("SELECT {JOB_COLS} FROM jobs WHERE id=?1");
    Ok(conn.query_row(&sql, params![job_id], map_job)?)
}

/// 历史列表：v1.3 起跨会话撤销的入口。重启应用后仍可列出并撤销保留期内的任务。
#[tauri::command]
pub fn list_jobs(state: State<'_, AppState>, limit: Option<i64>) -> AppResult<Vec<JobInfo>> {
    let limit = limit.unwrap_or(30).clamp(1, 200);
    let conn = state.db.lock();
    let sql = format!("SELECT {JOB_COLS} FROM jobs ORDER BY created_at DESC LIMIT ?1");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![limit], map_job)?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

#[tauri::command]
pub fn get_entries(
    state: State<'_, AppState>,
    job_id: String,
    cursor: Option<i64>,
    limit: Option<i64>,
    status: Option<String>,
) -> AppResult<EntryPage> {
    let cursor = cursor.unwrap_or(0);
    let limit = limit.unwrap_or(200).clamp(1, 1_000);
    let conn = state.db.lock();

    let total: i64 = match &status {
        Some(s) => conn.query_row(
            "SELECT COUNT(*) FROM entries WHERE job_id=?1 AND status=?2",
            params![job_id, s],
            |r| r.get(0),
        )?,
        None => conn.query_row(
            "SELECT COUNT(*) FROM entries WHERE job_id=?1",
            params![job_id],
            |r| r.get(0),
        )?,
    };

    let sql = match &status {
        Some(_) => format!(
            "SELECT {ENTRY_COLS} FROM entries WHERE job_id=?1 AND id>?2 AND status=?4 ORDER BY id LIMIT ?3"
        ),
        None => format!(
            "SELECT {ENTRY_COLS} FROM entries WHERE job_id=?1 AND id>?2 ORDER BY id LIMIT ?3"
        ),
    };
    let mut stmt = conn.prepare(&sql)?;
    let items: Vec<EntryRow> = match &status {
        Some(s) => stmt
            .query_map(params![job_id, cursor, limit, s], map_entry)?
            .collect::<Result<Vec<_>, _>>()?,
        None => stmt
            .query_map(params![job_id, cursor, limit], map_entry)?
            .collect::<Result<Vec<_>, _>>()?,
    };

    let next_cursor = if items.len() as i64 == limit {
        items.last().map(|e| e.id)
    } else {
        None
    };
    Ok(EntryPage {
        items,
        next_cursor,
        total,
    })
}

/// v1.3：复核冲突明细分页，前端只收统计 + 当前页，不收全量。
#[tauri::command]
pub fn get_undo_conflicts(
    state: State<'_, AppState>,
    job_id: String,
    cursor: Option<i64>,
    limit: Option<i64>,
) -> AppResult<EntryPage> {
    let cursor = cursor.unwrap_or(0);
    let limit = limit.unwrap_or(200).clamp(1, 1_000);
    let conn = state.db.lock();
    let total: i64 = conn.query_row(
        "SELECT COUNT(*) FROM entries WHERE job_id=?1 AND undo_check IS NOT NULL AND undo_check<>'ok'",
        params![job_id],
        |r| r.get(0),
    )?;
    let sql = format!(
        "SELECT {ENTRY_COLS} FROM entries
         WHERE job_id=?1 AND id>?2 AND undo_check IS NOT NULL AND undo_check<>'ok'
         ORDER BY id LIMIT ?3"
    );
    let mut stmt = conn.prepare(&sql)?;
    let items = stmt
        .query_map(params![job_id, cursor, limit], map_entry)?
        .collect::<Result<Vec<_>, _>>()?;
    let next_cursor = if items.len() as i64 == limit {
        items.last().map(|e| e.id)
    } else {
        None
    };
    Ok(EntryPage {
        items,
        next_cursor,
        total,
    })
}

#[tauri::command]
pub fn precheck_undo(
    state: State<'_, AppState>,
    job_id: String,
    channel: Channel<Progress>,
) -> AppResult<PrecheckResult> {
    undo::precheck_undo(state.inner(), &job_id, channel)
}

#[tauri::command]
pub fn undo_job(
    state: State<'_, AppState>,
    job_id: String,
    token: String,
    channel: Channel<Progress>,
) -> AppResult<UndoResult> {
    undo::undo_job(state.inner(), &job_id, &token, channel)
}

// --- 运行日志：只落盘，不在界面展示流水 -------------------------------------

/// 前端每次状态变更追加一行；fire-and-forget，写失败不影响主流程。
#[tauri::command]
pub fn append_log(app: tauri::AppHandle, line: String) -> AppResult<OkResult> {
    crate::logging::append(&app, &line)?;
    Ok(OkResult { ok: true })
}

/// 供状态栏显示日志文件位置。
#[tauri::command]
pub fn get_log_path(app: tauri::AppHandle) -> AppResult<String> {
    Ok(crate::logging::log_path(&app)?
        .to_string_lossy()
        .into_owned())
}

/// 用系统默认程序打开日志（后端调用 opener，无需前端 capability）。
#[tauri::command]
pub fn open_log_file(app: tauri::AppHandle) -> AppResult<OkResult> {
    use tauri_plugin_opener::OpenerExt;
    let path = crate::logging::ensure_file(&app)?;
    app.opener()
        .open_path(path.to_string_lossy().into_owned(), None::<&str>)
        .map_err(|e| AppError::new("OPEN_LOG_FAILED", e.to_string()))?;
    Ok(OkResult { ok: true })
}
