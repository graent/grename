//! 两态执行：① 原地 rename / ② `.part` → 长度校验 → 原子改名。
//! 设计文档 §6.0.2、§6.0.5、§6.3、§6.5。
//!
//! M0 采用单线程保序执行以建立可复现的吞吐基线；并发度（① 1~4 / ② 默认 4）
//! 作为 M1 的可调参数，扫描阶段已由 jwalk 并行。
//! 源文件在模式 ② 全程只读，这是「撤销只需删产物」的前提。
//!
//! v1.19 起 ① 增加**两阶段临时名**（§6.3）：链式 `a→b`+`b→c`、交换 `a↔b`、
//! 仅改大小写 `a.txt→A.txt` 这三类场景里，目标位上坐着另一个参与改名的文件，
//! 直接 rename 必然失败。计划期把这些「挡路者」标进 `entries.tmp_path`，
//! 执行时先挪到同目录临时名让位，再统一落到终名。

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::Ordering;
use std::time::UNIX_EPOCH;

use rusqlite::params;
use sha2::{Digest, Sha256};
use tauri::ipc::Channel;

use crate::model::{AppError, AppResult, OpResult, Progress};
use crate::state::{now_ms, spawn_emitter, AppState, JobRuntime};

const WORK_BATCH: i64 = 500;
const COPY_BUF: usize = 1024 * 1024;

struct WorkRow {
    id: i64,
    seq: i64,
    src_path: String,
    src_size: i64,
    dst_path: String,
    /// 阶段 1 的让位目标；为空表示这条不挡路，直接从源名落地。
    tmp_path: Option<String>,
}

struct EntryUpdate {
    id: i64,
    status: &'static str,
    err_code: Option<&'static str>,
    dst_size: Option<i64>,
    dst_mtime_ns: Option<i64>,
    /// §7.1 可选内容指纹；仅 undo_verify_content 开启且模式 ② 时写入。
    dst_fingerprint: Option<String>,
}

/// 产物身份快照：执行完成时记录，供 v1.3 跨会话撤销复核比对（§6.6.2）。
pub fn stat_identity(path: &Path) -> Option<(i64, i64)> {
    let md = fs::metadata(path).ok()?;
    let size = md.len() as i64;
    let ns = md
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_nanos() as i64;
    Some((size, ns))
}

/// §7.1 可选内容指纹：流式 SHA-256，取摘要前 16 字节的 hex（32 字符）。
///
/// 只在 `undo_verify_content` 开启且为模式 ② 的产物上调用；默认关闭时
/// 完全不读盘，M0 五档吞吐基线与 M2 崩溃注入判据不受影响。
/// 设计中写的是 BLAKE3 前 16 字节，此处用同已在依赖树中的 SHA-256 做等价
/// 实现（同为加密摘要截断），避免为一个默认关闭的开关引入新 crate / 新下载。
pub fn fingerprint(path: &Path) -> Option<String> {
    let mut file = File::open(path).ok()?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; COPY_BUF];
    loop {
        let n = file.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(32);
    for b in &digest[..16] {
        hex.push_str(&format!("{b:02x}"));
    }
    Some(hex)
}

/// Windows 删除的有界最终一致性封装（M2 扩展崩溃注入实测缺陷 D-2）。
///
/// 第三方文件过滤驱动 / 杀软会短暂持有刚创建文件的句柄，使 `DeleteFileW`
/// 直接失败，或返回成功后文件仍处于 delete-pending（`metadata` 已失败但
/// `read_dir` 仍可枚举）。单次删除失败即把撤销条目标成冲突，会让任务永久
/// 卡在 completed 并留下产物，属于误判。
///
/// 这里只做有界退避（总等待上界约 420ms）+ 删除确认，不改变安全语义：
/// 身份校验仍在 `classify()` 之前完成，重试的仍是同一个已验证过的路径；
/// 重试耗尽仍失败才返回 false，由调用方按冲突处理。
pub(crate) fn remove_file_confirmed(path: &Path) -> bool {
    const BACKOFF_MS: [u64; 6] = [0, 20, 40, 80, 120, 160];
    for wait in BACKOFF_MS {
        if wait > 0 {
            std::thread::sleep(std::time::Duration::from_millis(wait));
        }
        let _ = fs::remove_file(path);
        if fs::metadata(path).is_err() {
            return true;
        }
    }
    false
}

pub fn start_execute(
    state: AppState,
    job_id: String,
    channel: Channel<Progress>,
) -> AppResult<OpResult> {
    // 必须在任何 db.lock() 作用域之外读 settings：load_settings 内部会再次取锁，
    // parking_lot 非重入，同作用域内取锁会自锁（M3 命令级回归已验证该边界）。
    let verify_content = crate::maintenance::load_settings(&state)?.undo_verify_content;
    let (output_mode, output_dir, total, bytes_total): (String, Option<String>, i64, i64) = {
        let conn = state.db.lock();
        conn.query_row(
            "SELECT output_mode, output_dir, total, bytes_total FROM jobs WHERE id=?1",
            params![job_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?
    };

    if output_mode == "to_dir" && output_dir.as_deref().unwrap_or("").is_empty() {
        return Err(AppError::new(
            "OUTPUT_DIR_INVALID",
            "复制模式缺少输出目录，请先生成计划",
        ));
    }

    {
        let conn = state.db.lock();
        conn.execute(
            "UPDATE jobs SET status='executing', undo_available=0, undo_precheck_token=NULL WHERE id=?1",
            params![job_id],
        )?;
    }

    let rt = JobRuntime::new();
    rt.total.store(total as u64, Ordering::Relaxed);
    rt.bytes_total.store(bytes_total as u64, Ordering::Relaxed);
    state.set_runtime(&job_id, rt.clone());

    {
        let state = state.clone();
        let job_id = job_id.clone();
        let mode = output_mode.clone();
        std::thread::spawn(move || {
            spawn_emitter(
                channel,
                rt.clone(),
                job_id.clone(),
                "executing".to_string(),
                mode.clone(),
            );
            let result = run_execute(&state, &job_id, &mode, &rt, verify_content);

            let conn = state.db.lock();
            match result {
                Ok(()) => {
                    let cancelled = rt.is_cancelled();
                    let done = rt.done.load(Ordering::Relaxed) as i64;
                    let _ = conn.execute(
                        "UPDATE jobs SET status=?, done=?, bytes_done=?, failed=?, undo_available=?, finished_at=? WHERE id=?",
                        params![
                            if cancelled { "cancelled" } else { "completed" },
                            done,
                            rt.bytes_done.load(Ordering::Relaxed) as i64,
                            rt.failed.load(Ordering::Relaxed) as i64,
                            if done > 0 { 1 } else { 0 },
                            now_ms(),
                            job_id
                        ],
                    );
                }
                Err(e) => {
                    let _ = conn.execute(
                        "UPDATE jobs SET status='interrupted', finished_at=? WHERE id=?",
                        params![now_ms(), job_id],
                    );
                    eprintln!("[grename] execute failed: {} {}", e.code, e.message);
                }
            }
            rt.finished.store(true, Ordering::Relaxed);
            state.clear_runtime(&job_id);
        });
    }

    Ok(OpResult { job_id })
}

/// 执行主循环（阶段 1 让位 + 阶段 2 落终名）。
///
/// 公开可见性是**为可测性**：`tests/two_phase.rs` 用它在无 GUI、无 Channel 的
/// 环境里驱动产品同一份代码，而不是另写一份模拟实现（与
/// `maintenance::resume_interrupted_impl` 的开放方式一致）。它不是 Tauri 命令，
/// 前端无法调用。
pub fn run_execute(
    state: &AppState,
    job_id: &str,
    output_mode: &str,
    rt: &JobRuntime,
    verify_content: bool,
) -> AppResult<()> {
    // 阶段 1（§6.3）：把「目标位被本次另一个文件占着」的挡路者先挪到同目录临时名。
    // 只有 ① 的计划期会写 tmp_path，② 在这里自然是 0 条，完全走原路径。
    let staged = stage_blockers(state, job_id, rt)?;
    if staged > 0 {
        eprintln!("[grename] job {job_id} 阶段 1 让位 {staged} 条");
    }

    // 阶段 2（§6.3）：所有待执行条目落终名；挡路者从临时名出发。
    loop {
        if rt.is_cancelled() || !rt.wait_while_paused() {
            break;
        }
        let rows: Vec<WorkRow> = {
            let conn = state.db.lock();
            let mut stmt = conn.prepare(
                "SELECT id, seq, src_path, src_size, dst_path, tmp_path FROM entries
                 WHERE job_id=?1 AND status='planned' ORDER BY seq LIMIT ?2",
            )?;
            let mapped = stmt.query_map(params![job_id, WORK_BATCH], |r| {
                Ok(WorkRow {
                    id: r.get(0)?,
                    seq: r.get(1)?,
                    src_path: r.get(2)?,
                    src_size: r.get(3)?,
                    dst_path: r.get(4)?,
                    tmp_path: r.get(5)?,
                })
            })?;
            mapped.collect::<Result<Vec<_>, _>>()?
        };
        if rows.is_empty() {
            break;
        }

        let mut updates: Vec<EntryUpdate> = Vec::with_capacity(rows.len());
        for row in rows.iter() {
            if rt.is_cancelled() || !rt.wait_while_paused() {
                break;
            }
            let src = Path::new(&row.src_path);
            let dst = Path::new(&row.dst_path);

            // 阶段 1 是否真的把这条挪走了，以**文件系统**为准而不是以列为准：
            // 阶段 1 中途取消 / 崩过时，tmp_path 可能非空而文件仍在原位。
            let staged_to = row
                .tmp_path
                .as_deref()
                .filter(|s| !s.is_empty())
                .map(Path::new)
                .filter(|p| p.exists());
            let from = staged_to.unwrap_or(src);

            // 预览后文件被外部删除 / 改动（TOCTOU，§6.3）。
            // 已在临时名上的条目源本就不该存在，跳过这段校验。
            if staged_to.is_none() {
                let md = fs::metadata(src);
                let bad = match md {
                    Ok(m) => m.len() != row.src_size as u64,
                    Err(_) => true,
                };
                if bad {
                    updates.push(EntryUpdate {
                        id: row.id,
                        status: "failed",
                        err_code: Some("NOT_FOUND"),
                        dst_size: None,
                        dst_mtime_ns: None,
                        dst_fingerprint: None,
                    });
                    rt.failed.fetch_add(1, Ordering::Relaxed);
                    rt.bytes_done
                        .fetch_add(row.src_size as u64, Ordering::Relaxed);
                    rt.done.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
            }

            // 永不静默覆盖（§3.2 第 1 条）。
            // 挡路者已在阶段 1 让位，到这里目标位正常是空的；命中即外部文件占位
            // （或本任务外的同名文件），一律不覆盖。
            if dst.exists() {
                if let Some(t) = staged_to {
                    // 让过位却没落到终名：退回源名，绝不停在临时名上（§3.2 第 3 条）。
                    let _ = fs::rename(t, src);
                }
                updates.push(EntryUpdate {
                    id: row.id,
                    status: "failed",
                    err_code: Some("TARGET_EXISTS"),
                    dst_size: None,
                    dst_mtime_ns: None,
                    dst_fingerprint: None,
                });
                rt.failed.fetch_add(1, Ordering::Relaxed);
                rt.bytes_done
                    .fetch_add(row.src_size as u64, Ordering::Relaxed);
                rt.done.fetch_add(1, Ordering::Relaxed);
                continue;
            }

            let outcome = if output_mode == "in_place" {
                execute_in_place(from, dst)
            } else {
                execute_to_dir(job_id, row, src, dst, rt)
            };

            match outcome {
                Ok((size, mtime_ns)) => {
                    // 内容指纹默认关闭；开启时只对模式 ② 的产物额外读一次盘。
                    let fp = if verify_content && output_mode == "to_dir" {
                        fingerprint(dst)
                    } else {
                        None
                    };
                    updates.push(EntryUpdate {
                        id: row.id,
                        status: "done",
                        err_code: None,
                        dst_size: Some(size),
                        dst_mtime_ns: Some(mtime_ns),
                        dst_fingerprint: fp,
                    });
                }
                Err(code) => {
                    if let Some(t) = staged_to {
                        // 落终名失败：还原回源名。宁可这一条没做，也不留临时名。
                        let _ = fs::rename(t, src);
                    }
                    updates.push(EntryUpdate {
                        id: row.id,
                        status: "failed",
                        err_code: Some(code),
                        dst_size: None,
                        dst_mtime_ns: None,
                        dst_fingerprint: None,
                    });
                    rt.failed.fetch_add(1, Ordering::Relaxed);
                }
            }
            rt.done.fetch_add(1, Ordering::Relaxed);
            if output_mode == "in_place" {
                rt.bytes_done
                    .fetch_add(row.src_size as u64, Ordering::Relaxed);
            }
        }

        // 状态批量回写，一个事务覆盖整批（§7.3）。
        // tmp_path 一并清空：本条已处理完，无论成败都不该再指向临时名。
        {
            let mut conn = state.db.lock();
            let tx = conn.transaction()?;
            {
                let mut stmt = tx.prepare(
                    "UPDATE entries SET status=?1, err_code=?2, dst_size=?3, dst_mtime_ns=?4,
                     dst_fingerprint=?5, tmp_path=NULL WHERE id=?6",
                )?;
                for u in updates.iter() {
                    stmt.execute(params![
                        u.status,
                        u.err_code,
                        u.dst_size,
                        u.dst_mtime_ns,
                        u.dst_fingerprint,
                        u.id
                    ])?;
                }
            }
            tx.commit()?;
        }
    }

    // 兜底：取消 / 提前跳出时可能仍有条目停在临时名上，统一还原回源名。
    // 正常跑完时这里查不到任何残留，是幂等空转。
    let restored = unstage_blockers(state, job_id)?;
    if restored > 0 {
        eprintln!("[grename] job {job_id} 收尾还原 {restored} 条临时名");
    }
    Ok(())
}

/// 阶段 1：挡路者 `源名 → 同目录临时名`。
///
/// **只做 rename、不写库**——`tmp_path` 在计划期已持久化，因此阶段 1 中途崩溃后
/// 只需看「终名 / 源名 / 临时名」三者谁存在就能反推停在哪一步
/// （`stage::recovery_action`），不需要额外的进度记录。
fn stage_blockers(state: &AppState, job_id: &str, rt: &JobRuntime) -> AppResult<i64> {
    let mut cursor: i64 = 0;
    let mut staged: i64 = 0;
    loop {
        let rows: Vec<(i64, String, i64, String)> = {
            let conn = state.db.lock();
            let mut stmt = conn.prepare(
                "SELECT id, src_path, src_size, tmp_path FROM entries
                 WHERE job_id=?1 AND status='planned' AND tmp_path IS NOT NULL AND tmp_path<>''
                   AND id>?2 ORDER BY id LIMIT ?3",
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

        // (id, err_code)
        let mut failures: Vec<(i64, &'static str)> = Vec::new();
        for (id, src_path, src_size, tmp_path) in rows.iter() {
            let src = Path::new(src_path);
            let tmp = Path::new(tmp_path);

            // 与阶段 2 同口径的 TOCTOU 校验：预览后源被换掉就不挪。
            let bad = match fs::metadata(src) {
                Ok(m) => m.len() != *src_size as u64,
                Err(_) => true,
            };
            if bad {
                failures.push((*id, "NOT_FOUND"));
                rt.failed.fetch_add(1, Ordering::Relaxed);
                rt.bytes_done
                    .fetch_add(*src_size as u64, Ordering::Relaxed);
                rt.done.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            if fs::rename(src, tmp).is_err() {
                failures.push((*id, "ACCESS_DENIED"));
                rt.failed.fetch_add(1, Ordering::Relaxed);
                rt.bytes_done
                    .fetch_add(*src_size as u64, Ordering::Relaxed);
                rt.done.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            staged += 1;
        }

        // 让位失败的直接收尾：置 failed 并清掉 tmp_path，免得阶段 2 再碰它。
        if !failures.is_empty() {
            let mut conn = state.db.lock();
            let tx = conn.transaction()?;
            {
                let mut stmt = tx.prepare(
                    "UPDATE entries SET status='failed', err_code=?1, tmp_path=NULL WHERE id=?2",
                )?;
                for (id, code) in failures.iter() {
                    stmt.execute(params![code, id])?;
                }
            }
            tx.commit()?;
        }

        // 取消后不再挪新的；已挪的交由 unstage_blockers 还原。
        if rt.is_cancelled() || !rt.wait_while_paused() {
            break;
        }
    }
    Ok(staged)
}

/// 把仍停在临时名上的 planned 条目还原回源名。
///
/// 幂等：临时名已不在（已被还原或已落终名）的条目只清标记；还原失败的**保留**
/// `tmp_path` 原值，让崩溃收尾（`resume_interrupted`）能据此判成人工核查，
/// 而不是把线索一起抹掉。
fn unstage_blockers(state: &AppState, job_id: &str) -> AppResult<i64> {
    let mut cursor: i64 = 0;
    let mut restored: i64 = 0;
    loop {
        let rows: Vec<(i64, String, String)> = {
            let conn = state.db.lock();
            let mut stmt = conn.prepare(
                "SELECT id, src_path, tmp_path FROM entries
                 WHERE job_id=?1 AND status='planned' AND tmp_path IS NOT NULL AND tmp_path<>''
                   AND id>?2 ORDER BY id LIMIT ?3",
            )?;
            stmt.query_map(params![job_id, cursor, WORK_BATCH], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })?
            .collect::<Result<Vec<_>, _>>()?
        };
        if rows.is_empty() {
            break;
        }
        cursor = rows.last().map(|r| r.0).unwrap_or(cursor);

        let mut cleared: Vec<i64> = Vec::new();
        for (id, src_path, tmp_path) in rows.iter() {
            let src = Path::new(src_path);
            let tmp = Path::new(tmp_path);
            if !tmp.exists() {
                cleared.push(*id);
                continue;
            }
            if fs::rename(tmp, src).is_ok() {
                cleared.push(*id);
                restored += 1;
            }
        }
        if !cleared.is_empty() {
            let mut conn = state.db.lock();
            let tx = conn.transaction()?;
            {
                let mut stmt = tx.prepare("UPDATE entries SET tmp_path=NULL WHERE id=?1")?;
                for id in cleared.iter() {
                    stmt.execute(params![id])?;
                }
            }
            tx.commit()?;
        }
    }
    Ok(restored)
}

/// ① 原地改名。`from` 可能是源名，也可能是阶段 1 已让位的临时名——
/// 「仅改大小写」「链式」「交换」现在都由计划期的 `tmp_path` 统一覆盖，
/// 这里不再需要按场景分支（v1.18 的单条 `.cs.tmp` 特判已删除）。
fn execute_in_place(from: &Path, dst: &Path) -> Result<(i64, i64), &'static str> {
    fs::rename(from, dst).map_err(|_| "ACCESS_DENIED")?;
    stat_identity(dst).ok_or("IO_ERROR")
}

fn execute_to_dir(
    job_id: &str,
    row: &WorkRow,
    src: &Path,
    dst: &Path,
    rt: &JobRuntime,
) -> Result<(i64, i64), &'static str> {
    let parent = dst.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|_| "OUTPUT_DIR_INVALID")?;

    // 禁止直写最终名：崩溃只可能留下 .part，绝不会有半成品（§6.0.5）。
    let part = parent.join(format!("__gre_{}_{}.part", &job_id[..8.min(job_id.len())], row.seq));
    let mut input = File::open(src).map_err(|_| "ACCESS_DENIED")?;
    let mut output = File::create(&part).map_err(|_| "IO_ERROR")?;
    let mut buf = vec![0u8; COPY_BUF];
    let mut written: u64 = 0;

    loop {
        if rt.is_cancelled() {
            drop(output);
            let _ = fs::remove_file(&part);
            return Err("CANCELLED");
        }
        let n = input.read(&mut buf).map_err(|_| "IO_ERROR")?;
        if n == 0 {
            break;
        }
        output.write_all(&buf[..n]).map_err(|_| "IO_ERROR")?;
        written += n as u64;
        // 大文件写入期间按字节推进进度，避免进度条假死（§6.0.6）。
        rt.bytes_done.fetch_add(n as u64, Ordering::Relaxed);
    }
    output.flush().map_err(|_| "IO_ERROR")?;
    drop(output);
    drop(input);

    if written != row.src_size as u64 {
        let _ = fs::remove_file(&part);
        return Err("COPY_INCOMPLETE");
    }
    fs::rename(&part, dst).map_err(|_| "IO_ERROR")?;
    stat_identity(dst).ok_or("IO_ERROR")
}
