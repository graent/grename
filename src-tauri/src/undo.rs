//! v1.3 跨会话撤销：资格校验 → 全量状态复核 → 短期令牌 → 二次 stat 后执行。
//! 设计文档 §1.2、§3.2 第 7/10 条、§5.2、§6.6。
//!
//! 这是 B 方案的安全核心：不假设「文件从执行完到现在没变过」，
//! 而是逐条证明「这仍是本次产生、且未被改动的那份文件」，否则一律跳过。

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::sync::atomic::Ordering;

use rayon::prelude::*;
use rusqlite::params;
use tauri::ipc::Channel;
use uuid::Uuid;

use crate::execute::{fingerprint, remove_file_confirmed, stat_identity};
use crate::model::{
    AppError, AppResult, ConflictCount, PrecheckResult, Progress, UndoResult,
};
use crate::state::{now_ms, spawn_emitter, AppState, JobRuntime};

const CHECK_BATCH: i64 = 500;

#[derive(Clone)]
struct CheckRow {
    id: i64,
    /// 由 SQL 的 ORDER BY seq 使用；结构体保留以便 M1 输出逐条审计报告。
    #[allow(dead_code)]
    seq: i64,
    src_path: String,
    dst_path: String,
    dst_size: Option<i64>,
    dst_mtime_ns: Option<i64>,
    /// §7.1 可选内容指纹：执行时开关开启才有值；老任务为 NULL，回退原校验。
    dst_fingerprint: Option<String>,
}

/// 按输出模式判定某条 done 条目当前是否可安全撤销。
/// 返回 "ok"、"ALREADY_UNDONE"（撤销目的已达成，需收敛状态）或 §6.5 中的撤销类错误码。
fn classify(row: &CheckRow, output_mode: &str, verify_content: bool) -> &'static str {
    let src = Path::new(&row.src_path);
    let dst = Path::new(&row.dst_path);

    if !dst.exists() {
        // 撤销执行中崩溃会留下「文件已还原、批量状态未回写」的窗口：
        // 产物已不在而原始文件在位，说明撤销目的已达成，应收敛为 undone
        // 而不是判成冲突把任务永久卡在 completed（M2 扩展崩溃注入实测）。
        if src.exists() {
            return "ALREADY_UNDONE";
        }
        return "UNDO_TARGET_MISSING";
    }

    if output_mode == "in_place" {
        // ① 撤销是反向 rename：内容被编辑过不影响改回名字，
        // 但 src 已被新文件占用时反向改名会覆盖它，必须阻断。
        if src.exists() {
            return "UNDO_SOURCE_OCCUPIED";
        }
        "ok"
    } else {
        // ② 撤销是删除产物：删错不可逆，必须证明产物身份完全一致。
        let (Some(expected_size), Some(expected_mtime)) = (row.dst_size, row.dst_mtime_ns) else {
            return "UNDO_TARGET_MODIFIED";
        };
        match stat_identity(dst) {
            Some((size, mtime)) if size == expected_size && mtime == expected_mtime => {
                // §7.1 可选内容指纹：只在开关开启、且该条目执行时确实记录了
                // 指纹时追加一次内容比对。老任务 / 关闭开关时执行的任务没有
                // 指纹，回退既有 size+mtime 判据 —— 开启开关不会让历史任务
                // 突然全部不可撤销，也不会改变默认关闭时的任何行为。
                if verify_content {
                    if let Some(expected) = row.dst_fingerprint.as_deref() {
                        return match fingerprint(dst) {
                            Some(actual) if actual == expected => "ok",
                            _ => "UNDO_TARGET_MODIFIED",
                        };
                    }
                }
                "ok"
            }
            _ => "UNDO_TARGET_MODIFIED",
        }
    }
}

fn load_job_for_undo(
    state: &AppState,
    job_id: &str,
) -> AppResult<(String, String, i64)> {
    let row: (String, i64, i64, String) = {
        let conn = state.db.lock();
        conn.query_row(
            "SELECT status, undo_available, created_at, output_mode FROM jobs WHERE id=?1",
            params![job_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?
    };
    let (status, undo_available, created_at, output_mode) = row;
    if status != "completed" && status != "cancelled" {
        return Err(AppError::new(
            "UNDO_NOT_AVAILABLE",
            "任务尚未结束，无法撤销",
        ));
    }
    if undo_available == 0 {
        return Err(AppError::new(
            "UNDO_NOT_AVAILABLE",
            "该任务已撤销或不具备撤销资格",
        ));
    }
    // M3：撤销有效期 = 历史保留期，均由 settings 驱动；-1 表示永久。
    let retention = crate::maintenance::load_settings(state)?.history_retention_days;
    if retention >= 0 {
        let cutoff = now_ms() - retention * 24 * 3_600_000;
        if created_at < cutoff {
            return Err(AppError::new(
                "UNDO_RETENTION_EXPIRED",
                format!("已超过历史保留期 {retention} 天，撤销窗口已关闭；文件本身不受影响"),
            ));
        }
    }
    Ok((status, output_mode, created_at))
}

fn fetch_check_batch(
    state: &AppState,
    job_id: &str,
    cursor: i64,
) -> AppResult<Vec<CheckRow>> {
    let conn = state.db.lock();
    let mut stmt = conn.prepare(
        "SELECT id, seq, src_path, dst_path, dst_size, dst_mtime_ns, dst_fingerprint FROM entries
         WHERE job_id=?1 AND status='done' AND id>?2 ORDER BY id LIMIT ?3",
    )?;
    let mapped = stmt.query_map(params![job_id, cursor, CHECK_BATCH], |r| {
        Ok(CheckRow {
            id: r.get(0)?,
            seq: r.get(1)?,
            src_path: r.get(2)?,
            dst_path: r.get(3)?,
            dst_size: r.get(4)?,
            dst_mtime_ns: r.get(5)?,
            dst_fingerprint: r.get(6)?,
        })
    })?;
    Ok(mapped.collect::<Result<Vec<_>, _>>()?)
}

/// 阶段 1：全量复核。命令内同步执行（Tauri 命令在线程池，不阻塞 UI），
/// Channel 由独立 emitter 每 80ms 推送聚合进度。
pub fn precheck_undo(
    state: &AppState,
    job_id: &str,
    channel: Channel<Progress>,
) -> AppResult<PrecheckResult> {
    let (_, output_mode, _) = load_job_for_undo(state, job_id)?;
    // 同样在取 DB 锁之前读设置，避免非重入锁自锁。
    let verify_content = crate::maintenance::load_settings(state)?.undo_verify_content;

    let total: i64 = {
        let conn = state.db.lock();
        conn.query_row(
            "SELECT COUNT(*) FROM entries WHERE job_id=?1 AND status='done'",
            params![job_id],
            |r| r.get(0),
        )?
    };
    if total == 0 {
        return Err(AppError::new(
            "UNDO_NOT_AVAILABLE",
            "没有可撤销的成功条目",
        ));
    }

    let rt = JobRuntime::new();
    rt.total.store(total as u64, Ordering::Relaxed);
    state.set_runtime(job_id, rt.clone());
    spawn_emitter(
        channel,
        rt.clone(),
        job_id.to_string(),
        "undo_prechecking".to_string(),
        output_mode.clone(),
    );

    // 复核前清空上一轮结果，避免陈旧结论被复用。
    {
        let conn = state.db.lock();
        conn.execute(
            "UPDATE entries SET undo_check=NULL WHERE job_id=?1 AND status='done'",
            params![job_id],
        )?;
        conn.execute(
            "UPDATE jobs SET undo_precheck_token=NULL, undo_precheck_at=NULL WHERE id=?1",
            params![job_id],
        )?;
    }

    let mut cursor: i64 = 0;
    let mut already_undone: i64 = 0;
    loop {
        if rt.is_cancelled() {
            break;
        }
        let rows = fetch_check_batch(state, job_id, cursor)?;
        if rows.is_empty() {
            break;
        }
        cursor = rows.last().map(|r| r.id).unwrap_or(cursor);

        // rayon 并行 stat：纯元数据 IO，无共享写状态，内存 O(批)。
        let checks: Vec<(i64, &'static str)> = rows
            .par_iter()
            .map(|r| (r.id, classify(r, &output_mode, verify_content)))
            .collect();

        {
            let mut conn = state.db.lock();
            let tx = conn.transaction()?;
            {
                let mut mark = tx.prepare("UPDATE entries SET undo_check=?1 WHERE id=?2")?;
                let mut heal = tx.prepare(
                    "UPDATE entries SET status='undone', undo_check=NULL WHERE id=?1",
                )?;
                for (id, code) in checks.iter() {
                    if *code == "ALREADY_UNDONE" {
                        heal.execute(params![*id])?;
                        already_undone += 1;
                    } else {
                        mark.execute(params![*code, *id])?;
                    }
                }
            }
            tx.commit()?;
        }
        rt.done.fetch_add(checks.len() as u64, Ordering::Relaxed);
    }

    let (ok_count, conflicts): (i64, Vec<ConflictCount>) = {
        let conn = state.db.lock();
        let ok = conn.query_row(
            "SELECT COUNT(*) FROM entries WHERE job_id=?1 AND status='done' AND undo_check='ok'",
            params![job_id],
            |r| r.get(0),
        )?;
        let mut stmt = conn.prepare(
            "SELECT undo_check, COUNT(*) FROM entries
             WHERE job_id=?1 AND status='done' AND undo_check IS NOT NULL AND undo_check<>'ok'
             GROUP BY undo_check ORDER BY undo_check",
        )?;
        let mapped = stmt.query_map(params![job_id], |r| {
            Ok(ConflictCount {
                code: r.get(0)?,
                count: r.get(1)?,
            })
        })?;
        (ok, mapped.collect::<Result<Vec<_>, _>>()?)
    };

    let token = Uuid::new_v4().to_string();
    let at = now_ms();
    let ttl_sec = crate::maintenance::load_settings(state)?.undo_precheck_ttl_sec;
    let expires_at = at + ttl_sec * 1000;
    {
        let conn = state.db.lock();
        conn.execute(
            "UPDATE jobs SET undo_precheck_at=?1, undo_precheck_token=?2 WHERE id=?3",
            params![at, token, job_id],
        )?;
    }

    rt.finished.store(true, Ordering::Relaxed);
    state.clear_runtime(job_id);

    Ok(PrecheckResult {
        job_id: job_id.to_string(),
        total,
        ok_count,
        already_undone,
        conflicts,
        token,
        expires_at,
    })
}

/// 阶段 2：持令牌执行撤销。每条操作前**二次 stat**（挡住复核后又发生的变化）。
pub fn undo_job(
    state: &AppState,
    job_id: &str,
    token: &str,
    channel: Channel<Progress>,
) -> AppResult<UndoResult> {
    let (_, output_mode, _) = load_job_for_undo(state, job_id)?;
    let settings = crate::maintenance::load_settings(state)?;
    let ttl_sec = settings.undo_precheck_ttl_sec;
    let verify_content = settings.undo_verify_content;

    let (saved_token, saved_at): (Option<String>, Option<i64>) = {
        let conn = state.db.lock();
        conn.query_row(
            "SELECT undo_precheck_token, undo_precheck_at FROM jobs WHERE id=?1",
            params![job_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?
    };
    let (Some(saved_token), Some(saved_at)) = (saved_token, saved_at) else {
        return Err(AppError::new(
            "UNDO_PRECHECK_REQUIRED",
            "尚未复核，请先执行撤销复核",
        ));
    };
    if saved_token != token || now_ms() - saved_at > ttl_sec * 1000 {
        return Err(AppError::new(
            "UNDO_PRECHECK_REQUIRED",
            "复核已过期或令牌不匹配，请重新复核",
        ));
    }

    let total: i64 = {
        let conn = state.db.lock();
        conn.query_row(
            "SELECT COUNT(*) FROM entries WHERE job_id=?1 AND status='done' AND undo_check='ok'",
            params![job_id],
            |r| r.get(0),
        )?
    };

    let rt = JobRuntime::new();
    rt.total.store(total as u64, Ordering::Relaxed);
    state.set_runtime(job_id, rt.clone());
    spawn_emitter(
        channel,
        rt.clone(),
        job_id.to_string(),
        "undoing".to_string(),
        output_mode.clone(),
    );

    let mut undone: i64 = 0;
    let mut conflicts: i64 = 0;
    loop {
        if rt.is_cancelled() {
            break;
        }
        // 按 seq 逆序（§6.6.3）：交换/链式改名场景必须逆序还原。
        let rows: Vec<CheckRow> = {
            let conn = state.db.lock();
            let mut stmt = conn.prepare(
                "SELECT id, seq, src_path, dst_path, dst_size, dst_mtime_ns, dst_fingerprint FROM entries
                 WHERE job_id=?1 AND status='done' AND undo_check='ok'
                 ORDER BY seq DESC LIMIT ?2",
            )?;
            let mapped = stmt.query_map(params![job_id, CHECK_BATCH], |r| {
                Ok(CheckRow {
                    id: r.get(0)?,
                    seq: r.get(1)?,
                    src_path: r.get(2)?,
                    dst_path: r.get(3)?,
                    dst_size: r.get(4)?,
                    dst_mtime_ns: r.get(5)?,
                    dst_fingerprint: r.get(6)?,
                })
            })?;
            mapped.collect::<Result<Vec<_>, _>>()?
        };
        if rows.is_empty() {
            break;
        }

        let mut updates: Vec<(i64, &'static str, Option<&'static str>)> =
            Vec::with_capacity(rows.len());
        for row in rows.iter() {
            if rt.is_cancelled() {
                break;
            }
            // 二次 stat：绝不依赖「刚复核过」这个假设。
            let check = classify(row, &output_mode, verify_content);
            // 二次 stat 同样自愈：复核之后、撤销之前若又发生还原，按已达成处理。
            if check == "ALREADY_UNDONE" {
                updates.push((row.id, "undone", None));
                rt.done.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            if check != "ok" {
                updates.push((row.id, "done", Some(check)));
                conflicts += 1;
                rt.failed.fetch_add(1, Ordering::Relaxed);
                continue;
            }

            let src = Path::new(&row.src_path);
            let dst = Path::new(&row.dst_path);
            let ok = if output_mode == "in_place" {
                fs::rename(dst, src).is_ok()
            } else {
                // ② 只删产物，原文件全程不动。
                // D-2：Windows 过滤驱动可能瞬时占用刚生成的文件，单次删除失败
                // 不代表产物被改动；有界重试并确认消失后才算成功。
                remove_file_confirmed(dst)
            };
            if ok {
                updates.push((row.id, "undone", None));
                undone += 1;
            } else {
                updates.push((row.id, "done", Some("UNDO_TARGET_MODIFIED")));
                conflicts += 1;
                rt.failed.fetch_add(1, Ordering::Relaxed);
            }
            rt.done.fetch_add(1, Ordering::Relaxed);
        }

        {
            let mut conn = state.db.lock();
            let tx = conn.transaction()?;
            {
                let mut stmt = tx.prepare(
                    "UPDATE entries SET status=?1, undo_check=?2 WHERE id=?3",
                )?;
                for (id, status, check) in updates.iter() {
                    stmt.execute(params![*status, *check, *id])?;
                }
            }
            tx.commit()?;
        }
    }

    let remaining: i64 = {
        let conn = state.db.lock();
        conn.query_row(
            "SELECT COUNT(*) FROM entries WHERE job_id=?1 AND status='done'",
            params![job_id],
            |r| r.get(0),
        )?
    };
    // 部分冲突：保留 undo_available，允许用户处理后重新复核只撤剩余条目。
    let status = if remaining == 0 { "undone" } else { "completed" };
    {
        let conn = state.db.lock();
        conn.execute(
            "UPDATE jobs SET status=?1, undo_available=?2, undo_precheck_token=NULL,
                              undo_precheck_at=NULL, finished_at=?3 WHERE id=?4",
            params![
                status,
                if remaining == 0 { 0 } else { 1 },
                now_ms(),
                job_id
            ],
        )?;
    }

    rt.finished.store(true, Ordering::Relaxed);
    state.clear_runtime(job_id);

    Ok(UndoResult {
        job_id: job_id.to_string(),
        undone,
        conflicts,
        status: status.to_string(),
    })
}

/// 供 get_undo_conflicts 复用的分组统计（M0 预留，避免报告里塞全量明细）。
#[allow(dead_code)]
pub fn conflict_summary(state: &AppState, job_id: &str) -> AppResult<BTreeMap<String, i64>> {
    let conn = state.db.lock();
    let mut stmt = conn.prepare(
        "SELECT undo_check, COUNT(*) FROM entries
         WHERE job_id=?1 AND undo_check IS NOT NULL AND undo_check<>'ok'
         GROUP BY undo_check",
    )?;
    let mapped = stmt.query_map(params![job_id], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
    })?;
    Ok(mapped.collect::<Result<BTreeMap<_, _>, _>>()?)
}
