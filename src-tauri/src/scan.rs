//! 流式并行扫描 → 批量落 SQLite。
//! 设计文档 §6.1、§7.2、§10「无任何 O(N) 常驻内存结构」。
//!
//! - 目录模式：jwalk 按 CPU 并行预取，迭代器逐个产出，绝不收集全量路径到 Vec。
//! - 文件清单模式（M1）：原生对话框多选文件 → 求公共父目录 → 显式清单，递归自动失效。
//! - 每 BATCH 条开一次事务写入，内存恒定为 O(BATCH)。
//! - **M1 起目标名不再在扫描期生成**：`dst_name` 留空，由 `plan` 阶段按规则计算，
//!   这样改规则无需重扫，且预览与执行共用同一引擎。

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use rusqlite::params;
use tauri::ipc::Channel;

use crate::model::{AppError, AppResult, OpResult, Progress};
use crate::state::{now_ms, spawn_emitter, AppState, JobRuntime};

const BATCH: usize = 2_000;
/// 目录深度防御上限，非文件数上限（文件数全链路不设限）。
const MAX_DEPTH: usize = 1024;

struct Draft {
    seq: i64,
    src_path: String,
    src_name: String,
    src_size: i64,
    rel_dir: String,
    dst_name: String,
}

/// 立即返回 job_id；扫描在后台线程进行，进度经 Channel 推送。
pub fn start_scan(
    state: AppState,
    job_id: String,
    root: String,
    recursive: bool,
    paths: Option<Vec<String>>,
    channel: Channel<Progress>,
) -> AppResult<OpResult> {
    let explicit = paths.filter(|p| !p.is_empty());
    let (root_path, effective_recursive, explicit_paths) = match &explicit {
        Some(list) => (common_parent(list)?, false, Some(list.clone())),
        None => {
            let p = PathBuf::from(&root);
            if !p.is_dir() {
                return Err(AppError::new(
                    "ROOT_INVALID",
                    format!("目录不存在或不可读：{root}"),
                ));
            }
            (p, recursive, None)
        }
    };
    let root_str = root_path.to_string_lossy().into_owned();

    {
        let conn = state.db.lock();
        conn.execute(
            "INSERT INTO jobs (id, session_id, status, root_path, recursive, output_mode, created_at)
             VALUES (?1, ?2, 'scanning', ?3, ?4, 'in_place', ?5)",
            params![job_id, state.session_id, root_str, effective_recursive as i32, now_ms()],
        )?;
    }

    let rt = JobRuntime::new();
    state.set_runtime(&job_id, rt.clone());

    {
        let state = state.clone();
        let job_id = job_id.clone();
        std::thread::spawn(move || {
            spawn_emitter(
                channel,
                rt.clone(),
                job_id.clone(),
                "scanning".to_string(),
                "in_place".to_string(),
            );

            let result = do_scan(
                &state,
                &job_id,
                &root_path,
                effective_recursive,
                explicit_paths.as_deref(),
                &rt,
            );
            let conn = state.db.lock();
            match result {
                Ok((total, bytes)) => {
                    let cancelled = rt.is_cancelled();
                    let _ = conn.execute(
                        "UPDATE jobs SET status=?, total=?, bytes_total=?, done=?, bytes_done=?, finished_at=? WHERE id=?",
                        params![
                            if cancelled { "cancelled" } else { "planned" },
                            total as i64,
                            bytes as i64,
                            rt.done.load(Ordering::Relaxed) as i64,
                            bytes as i64,
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
                    eprintln!("[grename] scan failed: {} {}", e.code, e.message);
                }
            }
            rt.finished.store(true, Ordering::Relaxed);
            state.clear_runtime(&job_id);
        });
    }

    Ok(OpResult { job_id })
}

/// 多选文件的公共父目录；跨盘符无公共前缀时拒绝。
fn common_parent(paths: &[String]) -> AppResult<PathBuf> {
    let first = PathBuf::from(&paths[0]);
    let mut base = first
        .parent()
        .unwrap_or_else(|| Path::new(""))
        .to_path_buf();
    for p in paths.iter().skip(1) {
        let parent = PathBuf::from(p)
            .parent()
            .unwrap_or_else(|| Path::new(""))
            .to_path_buf();
        let mut next = PathBuf::new();
        for (a, b) in base.components().zip(parent.components()) {
            if a == b {
                next.push(a.as_os_str());
            } else {
                break;
            }
        }
        base = next;
    }
    if base.as_os_str().is_empty() {
        return Err(AppError::new(
            "ROOT_INVALID",
            "所选文件没有公共父目录（跨盘符多选暂不支持）",
        ));
    }
    Ok(base)
}

fn do_scan(
    state: &AppState,
    job_id: &str,
    root: &Path,
    recursive: bool,
    explicit: Option<&[String]>,
    rt: &JobRuntime,
) -> AppResult<(u64, u64)> {
    let mut batch: Vec<Draft> = Vec::with_capacity(BATCH);
    let mut seq: i64 = 0;
    let mut total: u64 = 0;
    let mut bytes: u64 = 0;

    macro_rules! commit {
        () => {{
            let (n, b) = commit_batch(state, job_id, &mut batch, rt)?;
            total += n;
            bytes += b;
        }};
    }

    if let Some(list) = explicit {
        for p in list {
            if rt.is_cancelled() {
                break;
            }
            let path = PathBuf::from(p);
            if !path.is_file() || path.is_symlink() {
                continue;
            }
            push_file(&mut batch, &mut seq, &path, root);
            if batch.len() >= BATCH {
                commit!();
            }
        }
    } else {
        let depth = if recursive { MAX_DEPTH } else { 1 };
        let walker = jwalk::WalkDir::new(root).max_depth(depth);
        for entry in walker.into_iter().filter_map(Result::ok) {
            if rt.is_cancelled() {
                break;
            }
            let path = entry.path();
            let file_type = entry.file_type();
            // 仅文件为对象；目录永不产生 Entry；符号链接默认跳过（§6.0.3 第 5 条）。
            if file_type.is_dir() || file_type.is_symlink() || !path.is_file() {
                continue;
            }
            push_file(&mut batch, &mut seq, &path, root);
            if batch.len() >= BATCH {
                commit!();
            }
        }
    }

    if !batch.is_empty() {
        commit!();
    }
    Ok((total, bytes))
}

fn push_file(batch: &mut Vec<Draft>, seq: &mut i64, path: &Path, root: &Path) {
    let Some(src_name) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else {
        return;
    };
    let src_size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let rel_dir = path
        .parent()
        .and_then(|p| p.strip_prefix(root).ok())
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_default();
    batch.push(Draft {
        seq: *seq,
        src_path: path.to_string_lossy().into_owned(),
        src_name,
        src_size: src_size as i64,
        rel_dir,
        // M1：目标名由 plan 阶段按规则计算，扫描期不决定。
        dst_name: String::new(),
    });
    *seq += 1;
}

fn commit_batch(
    state: &AppState,
    job_id: &str,
    batch: &mut Vec<Draft>,
    rt: &JobRuntime,
) -> AppResult<(u64, u64)> {
    let (n, b) = flush_batch(state, job_id, batch)?;
    rt.total.fetch_add(n, Ordering::Relaxed);
    rt.bytes_total.fetch_add(b, Ordering::Relaxed);
    rt.done.fetch_add(n, Ordering::Relaxed);
    rt.bytes_done.fetch_add(b, Ordering::Relaxed);
    Ok((n, b))
}

/// 一个事务写一批，避免 100 万次隐式事务。
fn flush_batch(state: &AppState, job_id: &str, batch: &mut Vec<Draft>) -> AppResult<(u64, u64)> {
    let mut conn = state.db.lock();
    let tx = conn.transaction()?;
    let mut n = 0u64;
    let mut bytes = 0u64;
    {
        let mut stmt = tx.prepare(
            "INSERT INTO entries (job_id, seq, src_path, src_name, src_size, rel_dir, dst_name, status)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'scanned')",
        )?;
        for d in batch.iter() {
            stmt.execute(params![
                job_id, d.seq, d.src_path, d.src_name, d.src_size, d.rel_dir, d.dst_name
            ])?;
            n += 1;
            bytes += d.src_size as u64;
        }
    }
    tx.commit()?;
    batch.clear();
    Ok((n, bytes))
}
