//! M2 崩溃恢复 + M3 设置 / TTL 清理 / 报告导出。
//! 设计文档 §6.6.4、§6.8、§7.1、§8。
//!
//! 设计约束（与 M0 的内存恒定口径一致）：
//! - 所有遍历走游标分页 / 流式迭代器，**不建 O(N) 常驻结构**；
//! - 只新增命令与表，不改动 scan / execute 既有状态机语义；
//! - 崩溃恢复只把文件系统收敛到可解释状态，**不剥夺撤销资格**，旧复核令牌失效。

use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use tauri::State;

use crate::execute::{remove_file_confirmed, stat_identity};
use crate::model::{AppError, AppResult};
use crate::stage::{self, Recovery};
use crate::state::{now_ms, AppState};

const WORK_BATCH: i64 = 500;

pub const DEFAULT_RETENTION_DAYS: i64 = 30;
pub const DEFAULT_UNDO_TTL_SEC: i64 = 300;

// ---------------------------------------------------------------------------
// 设置（§6.8 / §7.1：settings 表 key → JSON value）
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    /// 历史保留天数，同时是撤销有效期上限；0 = 退出即清空，-1 = 永久保留。
    pub history_retention_days: i64,
    /// 撤销复核令牌有效期（秒）。
    pub undo_precheck_ttl_sec: i64,
    /// §7.1 可选内容指纹：开启后新任务在 to_dir 执行完成时记录产物摘要，
    /// 撤销复核时比对内容；默认 false，保持 size+mtime 的既有校验与零额外读盘。
    pub undo_verify_content: bool,
    /// 主题 / 语言：M3 起与后端双向同步，不再只存 localStorage。
    pub theme: String,
    pub locale: String,
    /// v1.17 默认值后端化：设置里的「默认递归子目录」。
    /// 此前只写 localStorage 且主界面从不读取，属于死配置；现在由 Rust 侧持久化，
    /// 前端启动时以后端值为准（详见 docs/M3-收尾B-默认值后端化报告.md）。
    pub default_recursive: bool,
    /// v1.17 默认值后端化：设置里的「默认输出去向」。
    /// 只接受 `in_place` / `to_dir` 两个值，其余一律拒绝，避免非法模式流到执行层。
    pub default_output_mode: String,
}

/// 默认输出去向的合法取值（与前端 RuleSpec.outputMode 一致）。
pub const OUTPUT_MODE_IN_PLACE: &str = "in_place";
pub const OUTPUT_MODE_TO_DIR: &str = "to_dir";

impl Default for Settings {
    fn default() -> Self {
        Self {
            history_retention_days: DEFAULT_RETENTION_DAYS,
            undo_precheck_ttl_sec: DEFAULT_UNDO_TTL_SEC,
            undo_verify_content: false,
            theme: "system".to_string(),
            locale: "zh-CN".to_string(),
            // 默认值与 v1.16 主界面硬编码的初值保持一致，老库即便没有这两个键，
            // 读出来也仍是用户此前看到的行为，不产生任何可见变化。
            default_recursive: false,
            default_output_mode: OUTPUT_MODE_IN_PLACE.to_string(),
        }
    }
}

fn get_raw(state: &AppState, key: &str) -> AppResult<Option<String>> {
    let conn = state.db.lock();
    Ok(conn
        .query_row(
            "SELECT value FROM settings WHERE key=?1",
            params![key],
            |r| r.get::<_, String>(0),
        )
        .optional()?)
}

fn put_raw(conn: &rusqlite::Connection, key: &str, value: &str) -> AppResult<()> {
    conn.execute(
        "INSERT INTO settings(key, value, updated_at) VALUES(?1, ?2, ?3)
         ON CONFLICT(key) DO UPDATE SET value=excluded.value, updated_at=excluded.updated_at",
        params![key, value, now_ms()],
    )?;
    Ok(())
}

fn parse_i64(state: &AppState, key: &str, fallback: i64) -> i64 {
    get_raw(state, key)
        .ok()
        .flatten()
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(fallback)
}

fn parse_string(state: &AppState, key: &str, fallback: &str) -> String {
    get_raw(state, key)
        .ok()
        .flatten()
        .and_then(|v| serde_json::from_str::<String>(v.as_str()).ok())
        .unwrap_or_else(|| fallback.to_string())
}

fn parse_bool(state: &AppState, key: &str, fallback: bool) -> bool {
    get_raw(state, key)
        .ok()
        .flatten()
        .and_then(|v| serde_json::from_str::<bool>(v.as_str()).ok())
        .unwrap_or(fallback)
}

/// 逐键读取，任何一项缺失都回落到默认值；旧库无需迁移即可工作。
pub fn load_settings(state: &AppState) -> AppResult<Settings> {
    let d = Settings::default();
    Ok(Settings {
        history_retention_days: parse_i64(state, "history_retention_days", d.history_retention_days),
        undo_precheck_ttl_sec: parse_i64(state, "undo_precheck_ttl_sec", d.undo_precheck_ttl_sec),
        undo_verify_content: parse_bool(state, "undo_verify_content", d.undo_verify_content),
        theme: parse_string(state, "theme", &d.theme),
        locale: parse_string(state, "locale", &d.locale),
        default_recursive: parse_bool(state, "default_recursive", d.default_recursive),
        // 读出来也要过一遍白名单：手改数据库写入垃圾值时回落到 in_place，
        // 不让非法模式流到执行层（执行层只认这两个字面量）。
        default_output_mode: normalize_output_mode(&parse_string(
            state,
            "default_output_mode",
            &d.default_output_mode,
        )),
    })
}

/// 输出去向归一化：非 `to_dir` 一律视为 `in_place`（含历史垃圾值与空串）。
pub fn normalize_output_mode(raw: &str) -> String {
    if raw == OUTPUT_MODE_TO_DIR {
        OUTPUT_MODE_TO_DIR.to_string()
    } else {
        OUTPUT_MODE_IN_PLACE.to_string()
    }
}

#[tauri::command]
pub fn get_settings(state: State<'_, AppState>) -> AppResult<Settings> {
    load_settings(state.inner())
}

#[tauri::command]
pub fn set_settings(state: State<'_, AppState>, settings: Settings) -> AppResult<Settings> {
    set_settings_impl(state.inner(), settings)
}

/// 纯 Rust 核心逻辑，与上面的命令完全同构。
/// 供 M3 后端命令级回归（`grename-regress`）在无 GUI / 无 WebView 环境直接驱动。
pub fn set_settings_impl(state: &AppState, settings: Settings) -> AppResult<Settings> {
    if settings.history_retention_days < -1 {
        return Err(AppError::new(
            "SETTINGS_INVALID",
            "history_retention_days 只能为 -1（永久）或 >= 0",
        ));
    }
    if !(30..=86_400).contains(&settings.undo_precheck_ttl_sec) {
        return Err(AppError::new(
            "SETTINGS_INVALID",
            "undo_precheck_ttl_sec 需在 30 ~ 86400 秒之间",
        ));
    }
    // v1.17：输出去向只接受两个字面量。这里**不做归一化**而是直接报错，
    // 让调用方（前端 / 回归程序）立刻发现传错，而不是静默改成 in_place。
    if settings.default_output_mode != OUTPUT_MODE_IN_PLACE
        && settings.default_output_mode != OUTPUT_MODE_TO_DIR
    {
        return Err(AppError::new(
            "SETTINGS_INVALID",
            "default_output_mode 只能是 in_place 或 to_dir",
        ));
    }
    let conn = state.db.lock();
    // 数值本身就是合法 JSON；字符串按 JSON 编码，与 §7.1 一致。
    put_raw(
        &conn,
        "history_retention_days",
        &settings.history_retention_days.to_string(),
    )?;
    put_raw(
        &conn,
        "undo_precheck_ttl_sec",
        &settings.undo_precheck_ttl_sec.to_string(),
    )?;
    // bool 的字面量本身就是合法 JSON，与数值键同一处理方式。
    put_raw(
        &conn,
        "undo_verify_content",
        &settings.undo_verify_content.to_string(),
    )?;
    for (k, v) in [("theme", &settings.theme), ("locale", &settings.locale)] {
        let encoded = serde_json::to_string(v)
            .map_err(|e| AppError::new("SETTINGS_INVALID", e.to_string()))?;
        put_raw(&conn, k, &encoded)?;
    }
    // v1.17 默认值：bool 与字符串都按 JSON 编码，与上面同一套规则。
    put_raw(
        &conn,
        "default_recursive",
        &settings.default_recursive.to_string(),
    )?;
    let mode = serde_json::to_string(&settings.default_output_mode)
        .map_err(|e| AppError::new("SETTINGS_INVALID", e.to_string()))?;
    put_raw(&conn, "default_output_mode", &mode)?;
    drop(conn);
    load_settings(state)
}

// ---------------------------------------------------------------------------
// TTL 清理（§6.8 / §12.2 M3）
// ---------------------------------------------------------------------------

/// 删除超过保留期的**终态** job；`entries` 由外键 ON DELETE CASCADE 级联清理。
/// `interrupted`（未完成崩溃收尾）绝不清理，避免丢失可恢复的撤销资格。
/// **只删数据库记录，绝不触碰文件系统上的任何产物。**
pub fn prune_history(state: &AppState) -> AppResult<i64> {
    let days = load_settings(state)?.history_retention_days;
    if days < 0 {
        return Ok(0); // 永久保留
    }
    let cutoff = now_ms() - days * 24 * 3_600_000;
    let conn = state.db.lock();
    let n = conn.execute(
        "DELETE FROM jobs WHERE created_at < ?1
         AND status IN ('completed','cancelled','undone')",
        params![cutoff],
    )?;
    Ok(n as i64)
}

#[tauri::command]
pub fn prune_history_cmd(state: State<'_, AppState>) -> AppResult<i64> {
    prune_history(state.inner())
}

// ---------------------------------------------------------------------------
// M2 崩溃恢复（§6.6.4 / §8 resume_interrupted）
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecoverResult {
    pub job_id: String,
    pub parts_removed: i64,
    /// 文件已到位但状态未落库的条目，补记 done + 身份快照（保留撤销资格）。
    pub reconciled_done: i64,
    /// 未开始执行的条目，收尾为 skipped，避免被下次执行重复处理。
    pub marked_skipped: i64,
    /// 源与目标都不存在，列入人工核查，绝不猜测。
    pub needs_review: i64,
    pub undo_available: bool,
    pub status: String,
}

/// 把 interrupted 任务收敛到一致状态：
/// ① 清理本任务遗留的 `.part`；② 用文件系统实际状态反推 entries；
/// ③ job 收尾为 completed 并保留撤销资格，旧复核令牌失效。
///
/// 只处理 `status='interrupted'` 的任务；按 id 游标分页，内存 O(批)，与 N 无关。
#[tauri::command]
pub fn resume_interrupted(
    state: State<'_, AppState>,
    job_id: String,
) -> AppResult<RecoverResult> {
    resume_interrupted_impl(state.inner(), &job_id)
}

/// 纯 Rust 核心逻辑，与上面的命令完全同构。
/// 供 M2 稳定性用例（`grename-crash`）在无 GUI / 无 WebView 环境直接驱动，
/// 保证测试走的是**产品同一份代码**，而不是另写的模拟实现。
pub fn resume_interrupted_impl(
    state: &AppState,
    job_id: &str,
) -> AppResult<RecoverResult> {
    let (status, output_mode, output_dir): (String, String, Option<String>) = {
        let conn = state.db.lock();
        conn.query_row(
            "SELECT status, output_mode, output_dir FROM jobs WHERE id=?1",
            params![job_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?
    };
    if status != "interrupted" {
        return Err(AppError::new(
            "RECOVER_NOT_NEEDED",
            format!("任务当前状态为 {status}，无需崩溃收尾"),
        ));
    }

    // ① .part 清理：按 job_id 前缀精确匹配，绝不误删输出目录里的其他文件。
    let mut parts_removed = 0i64;
    if output_mode == "to_dir" {
        if let Some(dir) = output_dir.as_deref().filter(|d| !d.is_empty()) {
            let root = Path::new(dir);
            if root.is_dir() {
                let prefix = format!("__gre_{}_", &job_id[..8.min(job_id.len())]);
                // jwalk 是并行流式迭代器，边遍历边删除会让底层 read_dir 游标
                // 失效：M2 稳定性用例实测 10 轮中有 1 轮注入 3 个 .part 只删掉 2 个。
                // 两阶段（先完整收集、再统一删除）+ 单批 512 条上限，内存有界。
                // D-2 追加：Windows delete-pending 项可能仍被 read_dir 枚举到，
                // 若把「本轮删掉了 N 个」当收敛条件，会无限重扫同一批幽灵项
                // （串行批次实测挂起 8 分钟、CPU 近乎为 0）。改用 handled 集合
                // 记录已处理路径 + 固定迭代上界，保证循环必然终止。
                let mut handled: HashSet<PathBuf> = HashSet::new();
                for _sweep in 0..8 {
                    let batch: Vec<PathBuf> = jwalk::WalkDir::new(root)
                        .into_iter()
                        .filter_map(Result::ok)
                        .filter(|e| {
                            if !e.file_type().is_file() {
                                return false;
                            }
                            let name = e.file_name().to_string_lossy();
                            name.starts_with(&prefix)
                                && name.ends_with(".part")
                                && !handled.contains(&e.path())
                        })
                        .take(512)
                        .map(|e| e.path())
                        .collect();
                    if batch.is_empty() {
                        break;
                    }
                    for p in &batch {
                        // D-2：删除成功但文件仍处于 delete-pending 时不能计数，
                        // 否则「已清理 N 个」与输出目录实际残留不自洽。
                        if remove_file_confirmed(p) {
                            parts_removed += 1;
                        }
                        // 无论本轮是否确认消失都不再重试同一路径：
                        // remove_file_confirmed 内部已有 6 次退避，外部再重试
                        // 只会把 delete-pending 幽灵项反复计数。
                        handled.insert(p.clone());
                    }
                }
            }
        }
    }

    // ② 逐批反推条目状态。
    let mut cursor = 0i64;
    let (mut done_n, mut skipped_n, mut review_n) = (0i64, 0i64, 0i64);
    let mut restored_n = 0i64;
    loop {
        let rows: Vec<(i64, String, String, Option<String>)> = {
            let conn = state.db.lock();
            let mut stmt = conn.prepare(
                "SELECT id, src_path, dst_path, tmp_path FROM entries
                 WHERE job_id=?1 AND id>?2 AND status='planned' ORDER BY id LIMIT ?3",
            )?;
            stmt.query_map(params![job_id, cursor, WORK_BATCH], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })?
            .collect::<Result<Vec<_>, _>>()?
        };
        if rows.is_empty() {
            break;
        }
        cursor = rows.last().map(|r| r.0).unwrap_or(cursor);

        // (id, status, err_code, dst_size, dst_mtime_ns, 是否清掉 tmp_path)
        let mut updates: Vec<(i64, &str, Option<&str>, Option<i64>, Option<i64>, bool)> =
            Vec::with_capacity(rows.len());
        for (id, src, dst, tmp) in &rows {
            let dst_p = Path::new(dst);
            let src_p = Path::new(src);
            let tmp_p = tmp.as_deref().filter(|s| !s.is_empty()).map(Path::new);
            let tmp_exists = tmp_p.map(|p| p.exists()).unwrap_or(false);
            // §6.3：三者的存在性唯一决定停在哪一阶段，不依赖任何内存状态。
            match stage::recovery_action(dst_p.exists(), src_p.exists(), tmp_exists) {
                Recovery::Done => {
                    // 改名已完成但批量状态回写前崩溃：补身份快照，撤销仍可验证。
                    let ident = stat_identity(dst_p);
                    updates.push((
                        *id,
                        "done",
                        None,
                        ident.map(|x| x.0),
                        ident.map(|x| x.1),
                        true,
                    ));
                    done_n += 1;
                }
                Recovery::RestoreTmp => {
                    // 阶段 1 让过位、阶段 2 没落终名：还原回源名，当作没执行。
                    // 安全优先——回到执行前，而不是替用户猜该不该落终名。
                    let put_back = match tmp_p {
                        Some(p) => fs::rename(p, src_p).is_ok(),
                        None => false,
                    };
                    if put_back {
                        updates.push((*id, "skipped", Some("INTERRUPTED"), None, None, true));
                        skipped_n += 1;
                        restored_n += 1;
                    } else {
                        // 还原失败：文件仍在临时名上，留住 tmp_path 供人工核查。
                        updates.push((*id, "needs_review", Some("NEEDS_REVIEW"), None, None, false));
                        review_n += 1;
                    }
                }
                Recovery::Skipped => {
                    updates.push((*id, "skipped", Some("INTERRUPTED"), None, None, true));
                    skipped_n += 1;
                }
                Recovery::NeedsReview => {
                    // 组合不自洽（终名与临时名并存、源与临时名并存、三者皆无）：
                    // 不猜，保留 tmp_path 原值作为核查线索。
                    updates.push((*id, "needs_review", Some("NEEDS_REVIEW"), None, None, false));
                    review_n += 1;
                }
            }
        }
        {
            let mut conn = state.db.lock();
            let tx = conn.transaction()?;
            {
                let mut clear_stmt = tx.prepare(
                    "UPDATE entries SET status=?1, err_code=?2, dst_size=?3, dst_mtime_ns=?4,
                     tmp_path=NULL WHERE id=?5",
                )?;
                let mut keep_stmt = tx.prepare(
                    "UPDATE entries SET status=?1, err_code=?2, dst_size=?3, dst_mtime_ns=?4
                     WHERE id=?5",
                )?;
                for u in &updates {
                    if u.5 {
                        clear_stmt.execute(params![u.1, u.2, u.3, u.4, u.0])?;
                    } else {
                        keep_stmt.execute(params![u.1, u.2, u.3, u.4, u.0])?;
                    }
                }
            }
            tx.commit()?;
        }
    }
    if restored_n > 0 {
        eprintln!("[grename] job {job_id} 崩溃收尾还原 {restored_n} 条临时名");
    }

    // ③ 收尾：保留 undo_available，令牌失效（收尾本身改变了文件状态）。
    let done_total: i64 = {
        let conn = state.db.lock();
        conn.query_row(
            "SELECT COUNT(*) FROM entries WHERE job_id=?1 AND status='done'",
            params![job_id],
            |r| r.get(0),
        )?
    };
    let undo_available = done_total > 0;
    {
        let conn = state.db.lock();
        conn.execute(
            "UPDATE jobs SET status='completed', undo_available=?1, undo_precheck_token=NULL,
             undo_precheck_at=NULL, finished_at=?2, done=?3 WHERE id=?4",
            params![
                if undo_available { 1 } else { 0 },
                now_ms(),
                done_total,
                job_id
            ],
        )?;
    }

    Ok(RecoverResult {
        job_id: job_id.to_string(),
        parts_removed,
        reconciled_done: done_n,
        marked_skipped: skipped_n,
        needs_review: review_n,
        undo_available,
        status: "completed".to_string(),
    })
}

// ---------------------------------------------------------------------------
// M3 报告导出（§8 / §12.2）
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportResult {
    pub path: String,
    pub rows: i64,
}

fn csv_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// 把某个任务的逐条结果导出为 CSV。游标分页流式写出，内存 O(批)。
/// 带 UTF-8 BOM，Excel 直接打开不乱码；路径含引号/逗号均正确转义。
#[tauri::command]
pub fn export_report(
    state: State<'_, AppState>,
    job_id: String,
    path: String,
) -> AppResult<ExportResult> {
    export_report_impl(state.inner(), &job_id, &path)
}

/// 纯 Rust 核心逻辑，与上面的命令完全同构（同上，供无 GUI 回归驱动）。
pub fn export_report_impl(
    state: &AppState,
    job_id: &str,
    path: &str,
) -> AppResult<ExportResult> {
    let p = PathBuf::from(path);
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent)?;
    }
    let file = File::create(&p)?;
    let mut w = BufWriter::new(file);
    w.write_all(&[0xEF, 0xBB, 0xBF])?;
    writeln!(
        w,
        "job_id,seq,src_path,src_name,src_size,rel_dir,dst_path,dst_name,status,err_code,err_msg"
    )?;

    let mut cursor = 0i64;
    let mut rows = 0i64;
    loop {
        // 首列必须是 id（游标），不是 seq。
        let batch: Vec<(
            i64, i64, String, String, i64, String, String, String, String,
            Option<String>, Option<String>,
        )> = {
            let conn = state.db.lock();
            let mut stmt = conn.prepare(
                "SELECT id, seq, src_path, src_name, src_size, rel_dir, dst_path, dst_name,
                        status, err_code, err_msg FROM entries
                 WHERE job_id=?1 AND id>?2 ORDER BY id LIMIT ?3",
            )?;
            stmt.query_map(params![job_id, cursor, WORK_BATCH], |r| {
                Ok((
                    r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?,
                    r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?, r.get(9)?, r.get(10)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?
        };
        if batch.is_empty() {
            break;
        }
        cursor = batch.last().map(|b| b.0).unwrap_or(cursor);
        for b in &batch {
            writeln!(
                w,
                "{},{},{},{},{},{},{},{},{},{},{}",
                csv_quote(job_id),
                b.1,
                csv_quote(&b.2),
                csv_quote(&b.3),
                b.4,
                csv_quote(&b.5),
                csv_quote(&b.6),
                csv_quote(&b.7),
                csv_quote(&b.8),
                csv_quote(b.9.as_deref().unwrap_or("")),
                csv_quote(b.10.as_deref().unwrap_or("")),
            )?;
            rows += 1;
        }
    }
    w.flush()?;
    Ok(ExportResult {
        path: p.to_string_lossy().into_owned(),
        rows,
    })
}
