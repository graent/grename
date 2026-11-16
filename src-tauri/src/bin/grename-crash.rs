//! M2 崩溃恢复稳定性用例（设计文档 §6.6.4 / §12.3 第 ① 项）。
//!
//! 与 grename-bench 一样，用 `Channel::new` 在无 GUI / 无 WebView 环境驱动
//! **产品自身的后端模块**，不是另写的模拟实现。
//!
//! 两个相位（由 Python 驱动脚本分两次进程调用，中间即为「进程被强杀」）：
//!
//! - `crash`：建库 → 扫描 → 计划 → 启动执行 → 轮询 `JobRuntime.done`
//!   原子计数，达到 `--ratio × total` 时 `process::exit(137)` 硬退出。
//!   退出不运行任何析构、不 flush SQLite 事务、不清理 `.part`，
//!   等价于执行中掉电 / taskkill /F。
//! - `verify`：重新打开**同一个 DB**（跨会话）→ 快照崩溃现场 →
//!   `reconcile_on_startup()` → 可选注入确定性 `.part` →
//!   `resume_interrupted_impl` → `precheck_undo` → `undo_job` →
//!   第二次 `undo_job`（重复撤销防护）→ 文件系统一致性校验，
//!   最后输出单行 `VERIFY_JSON={...}` 供 Python 汇总。
//!
//! 崩溃点用运行时原子计数而非计时：本机 I/O 抖动大，按毫秒杀进程很容易
//! 落在「还没开始」或「已经跑完」，比例触发能保证 100% 命中执行中途。
//!
//! §12.3 第 ③ 项扩展的三个相位（原有 `crash` / `verify` 保持不变，10/10 基线可复跑）：
//! - `crash --kill-mode external`：执行中命中比例后打印 `KILL_POINT pid=...`
//!   并挂起，由外部 `taskkill /F /T /PID` 真正终止进程（OS 级 TerminateProcess）；
//!   `verify` 相位原样复用。
//! - `crash_undo` / `verify_undo`：先完整执行到 completed → 复核 → 在 `undo_job`
//!   执行到 `--ratio` 时硬退出；重启后重新复核并补撤剩余条目，验证不重不漏。
//! - `crash_scan` / `verify_scan`：在 `scanning` 阶段按已入库条数硬退出；
//!   重启后收敛 + 收尾，验证不误开撤销资格、文件系统原样、DB 可重新扫描。
//!
//! v1.20 扩展的两个相位（两阶段临时名的真实强杀，见 `run_stage_crash_matrix.py`）：
//! - `crash_stage` / `verify_stage`：确定性链式样本（N-1 个挡路者），
//!   杀点由第 K 个 `__gre_*.tmp` 的「出现 / 消失」判定，覆盖阶段 1 / 阶段 2。
//!   旧的比例杀点在阶段 1 恒命中不了（`stage_blockers` 成功不增加 `rt.done`）。

use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::{Duration, Instant};

use rusqlite::params;
use serde_json::{json, Value};
use tauri::ipc::Channel;
use tauri_app_lib::db::Db;
use tauri_app_lib::model::Progress;
use tauri_app_lib::rule::RuleSpec;
use tauri_app_lib::state::AppState;
use tauri_app_lib::{execute, maintenance, plan, scan, undo};

fn args() -> Vec<String> {
    env::args().collect()
}

fn arg(name: &str) -> String {
    let key = format!("--{name}");
    let a = args();
    for i in 0..a.len().saturating_sub(1) {
        if a[i] == key {
            return a[i + 1].clone();
        }
    }
    panic!("缺少参数 {key}");
}

fn opt(name: &str, default: &str) -> String {
    let key = format!("--{name}");
    let a = args();
    for i in 0..a.len().saturating_sub(1) {
        if a[i] == key {
            return a[i + 1].clone();
        }
    }
    default.to_string()
}

fn noop_channel() -> Channel<Progress> {
    Channel::new(|_| -> tauri::Result<()> { Ok(()) })
}

fn status_of(state: &AppState, job_id: &str) -> String {
    let conn = state.db.lock();
    conn.query_row(
        "SELECT status FROM jobs WHERE id=?1",
        params![job_id],
        |r| r.get(0),
    )
    .unwrap_or_else(|_| "unknown".to_string())
}

fn scalar<T: rusqlite::types::FromSql>(state: &AppState, sql: &str, p: &str) -> T {
    let conn = state.db.lock();
    conn.query_row(sql, params![p], |r| r.get(0))
        .expect("查询失败")
}

/// 扫描是异步 spawn。scan.rs 完成时把 jobs.status 置为 `planned`
/// （`scanned` 是 entries 的初始状态，别混淆），并以 clear_runtime 收尾。
fn wait_scan(state: &AppState, job_id: &str, timeout: Duration) {
    let t0 = Instant::now();
    loop {
        let running = state.get_runtime(job_id).is_some();
        let s = status_of(state, job_id);
        if !running && s == "planned" {
            return;
        }
        if matches!(s.as_str(), "failed" | "interrupted" | "cancelled") {
            panic!("扫描未正常完成：status={s}");
        }
        if t0.elapsed() > timeout {
            panic!("扫描超时：status={s}");
        }
        thread::sleep(Duration::from_millis(5));
    }
}

/// 等待执行线程跑完（`execute.rs` 完成时把 jobs.status 置为 `completed`）。
fn wait_execute_done(state: &AppState, job_id: &str, timeout: Duration) {
    let t0 = Instant::now();
    loop {
        let running = state.get_runtime(job_id).is_some();
        let s = status_of(state, job_id);
        if !running && s == "completed" {
            return;
        }
        if matches!(s.as_str(), "failed" | "interrupted" | "cancelled") {
            panic!("执行未正常完成：status={s}");
        }
        if t0.elapsed() > timeout {
            panic!("执行超时：status={s}");
        }
        thread::sleep(Duration::from_millis(5));
    }
}

/// 规则 `b_{n}` 生成的目标文件名数量：撤销完成后应为 0。
fn count_renamed_names(dir: &Path) -> i64 {
    if !dir.is_dir() {
        return 0;
    }
    let mut n = 0i64;
    for e in jwalk::WalkDir::new(dir).into_iter().filter_map(Result::ok) {
        if !e.file_type().is_file() {
            continue;
        }
        let name = e.file_name().to_string_lossy();
        if name.starts_with("b_") && name.ends_with(".dat") {
            n += 1;
        }
    }
    n
}

/// 建库 + 扫描 + 计划 + 启动执行，`crash` / `crash_undo` / `crash_wait` 共用。
#[allow(clippy::too_many_arguments)]
fn prepare_executing(
    root: &str,
    mode: &str,
    outdir: &str,
    db_path: &str,
) -> (AppState, String, i64) {
    let job_id = format!("crash_{}", uuid::Uuid::new_v4().simple());
    let db_file = PathBuf::from(db_path);
    if let Some(p) = db_file.parent() {
        fs::create_dir_all(p).expect("创建 DB 目录失败");
    }
    if db_file.exists() {
        fs::remove_file(&db_file).expect("清理旧 DB 失败");
    }
    let state = AppState::new(Db::open(&db_file).expect("打开 DB 失败"));
    state.reconcile_on_startup();

    scan::start_scan(
        state.clone(),
        job_id.clone(),
        root.to_string(),
        true,
        None,
        noop_channel(),
    )
    .expect("start_scan 失败");
    wait_scan(&state, &job_id, Duration::from_secs(600));

    let total: i64 = scalar(&state, "SELECT total FROM jobs WHERE id=?1", &job_id);
    assert!(total > 0, "扫描到 0 个文件");

    let rule = RuleSpec {
        replacement: "b_{n}".to_string(),
        ..RuleSpec::default()
    };
    plan::build_plan(
        &state,
        &job_id,
        mode,
        if mode == "to_dir" {
            Some(outdir.to_string())
        } else {
            None
        },
        rule,
    )
    .expect("build_plan 失败");
    (state, job_id, total)
}

/// 递归统计普通文件数与 .part 数。迭代器流式产出，不收集全量路径。
/// 目录最终一致性重读（M2 扩展实测观察项 O-2）：
/// Windows 删除可能短暂处于 delete-pending，`read_dir` 仍枚举得到已删除项。
/// 返回 (最后一次计数, 实际重读次数)；判据仍以最终稳定值为准，
/// 真正残留的文件重试 10 次后依旧会被判失败，判别力不被削弱。
fn count_files_settled(dir: &Path, expected: i64) -> (i64, i64) {
    let mut n = 0i64;
    for reads in 1..=10i64 {
        (n, _) = count_files(dir);
        if n == expected {
            return (n, reads);
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    (n, 10)
}

fn count_files(dir: &Path) -> (i64, i64) {
    if !dir.is_dir() {
        return (0, 0);
    }
    let (mut n, mut p) = (0i64, 0i64);
    for e in jwalk::WalkDir::new(dir).into_iter().filter_map(Result::ok) {
        if e.file_type().is_file() {
            n += 1;
            if e.file_name().to_string_lossy().ends_with(".part") {
                p += 1;
            }
        }
    }
    (n, p)
}

/// 只数属于本任务的 `.part`（前缀 `__gre_<jobId 前 8 位>_`）。
fn count_job_parts(dir: &Path, prefix: &str) -> i64 {
    if !dir.is_dir() {
        return 0;
    }
    let mut n = 0i64;
    for e in jwalk::WalkDir::new(dir).into_iter().filter_map(Result::ok) {
        if !e.file_type().is_file() {
            continue;
        }
        let name = e.file_name().to_string_lossy();
        if name.starts_with(prefix) && name.ends_with(".part") {
            n += 1;
        }
    }
    n
}

/// 原始文件名 `f_*.dat` 的数量：撤销还原后应等于 total。
fn count_orig_names(dir: &Path) -> i64 {
    if !dir.is_dir() {
        return 0;
    }
    let mut n = 0i64;
    for e in jwalk::WalkDir::new(dir).into_iter().filter_map(Result::ok) {
        if !e.file_type().is_file() {
            continue;
        }
        let name = e.file_name().to_string_lossy();
        if name.starts_with("f_") && name.ends_with(".dat") {
            n += 1;
        }
    }
    n
}

// ---------------------------------------------------------------------------
// 相位 1：执行中硬退出
// ---------------------------------------------------------------------------
fn phase_crash() -> ! {
    let root = arg("root");
    let mode = arg("mode");
    let outdir = arg("outdir");
    let db_path = arg("db");
    let ratio: f64 = opt("ratio", "0.4").parse().expect("ratio 非法");
    assert!((0.0..1.0).contains(&ratio), "ratio 必须在 (0,1)");
    let job_id = format!("crash_{}", uuid::Uuid::new_v4().simple());

    let db_file = PathBuf::from(&db_path);
    if let Some(p) = db_file.parent() {
        fs::create_dir_all(p).expect("创建 DB 目录失败");
    }
    if db_file.exists() {
        fs::remove_file(&db_file).expect("清理旧 DB 失败");
    }

    let state = AppState::new(Db::open(&db_file).expect("打开 DB 失败"));
    state.reconcile_on_startup();

    scan::start_scan(
        state.clone(),
        job_id.clone(),
        root.clone(),
        true,
        None,
        noop_channel(),
    )
    .expect("start_scan 失败");
    wait_scan(&state, &job_id, Duration::from_secs(600));

    let total: i64 = scalar(&state, "SELECT total FROM jobs WHERE id=?1", &job_id);
    assert!(total > 0, "扫描到 0 个文件");

    let rule = RuleSpec {
        replacement: "b_{n}".to_string(),
        ..RuleSpec::default()
    };
    plan::build_plan(
        &state,
        &job_id,
        &mode,
        if mode == "to_dir" {
            Some(outdir.clone())
        } else {
            None
        },
        rule,
    )
    .expect("build_plan 失败");

    execute::start_execute(state.clone(), job_id.clone(), noop_channel())
        .expect("start_execute 失败");
    println!("CRASH_READY job_id={job_id} total={total} ratio={ratio}");
    let _ = std::io::stdout().flush();

    // 运行时原子计数每条文件 +1，不受 500 条批量回写延迟影响。
    let target = ((total as f64) * ratio).max(1.0) as u64;
    let deadline = Instant::now() + Duration::from_secs(900);
    loop {
        let done = match state.get_runtime(&job_id) {
            Some(rt) => rt.done.load(Ordering::Relaxed),
            None => {
                eprintln!("CRASH_MISSED executor finished before ratio={ratio}");
                process::exit(139);
            }
        };
        if done >= target {
            eprintln!("KILL_AT done={done} target={target} total={total}");
            if opt("kill-mode", "self") == "external" {
                // 不自行退出：把 PID 交给外部，由 taskkill /F 做真正的 OS 级强杀。
                println!(
                    "KILL_POINT pid={} done={} target={} total={}",
                    process::id(), done, target, total
                );
                let _ = std::io::stdout().flush();
                let held = Instant::now();
                loop {
                    thread::sleep(Duration::from_millis(50));
                    // 兜底：外部驱动没杀到时不要挂死整轮测试。
                    if held.elapsed() > Duration::from_secs(120) {
                        eprintln!("EXTERNAL_KILL_TIMEOUT");
                        process::exit(140);
                    }
                }
            }
            // 硬退出：不 destruct、不 commit、不清理 .part。
            process::exit(137);
        }
        if Instant::now() > deadline {
            eprintln!("CRASH_TIMEOUT done={done} target={target}");
            process::exit(138);
        }
        thread::sleep(Duration::from_millis(2));
    }
}

// ---------------------------------------------------------------------------
// 相位 2：重启收敛 + 收尾 + 撤销闭环 + 校验
// ---------------------------------------------------------------------------
fn phase_verify() {
    let root = arg("root");
    let outdir = arg("outdir");
    let db_path = arg("db");
    let mode = arg("mode");
    let round: i64 = opt("round", "0").parse().unwrap_or(0);
    let inject: i64 = opt("inject", "0").parse().unwrap_or(0);
    let out = Path::new(&outdir);

    // 故意不复用 crash 进程里的任何内存状态：重新 Db::open == 应用重启。
    let state = AppState::new(Db::open(Path::new(&db_path)).expect("打开 DB 失败"));
    let job_id: String = {
        let conn = state.db.lock();
        conn.query_row("SELECT id FROM jobs LIMIT 1", [], |r| r.get(0))
            .expect("DB 中没有任务")
    };
    let prefix = format!("__gre_{}_", &job_id[..8.min(job_id.len())]);

    // ---- 崩溃现场快照（reconcile 之前） ----------------------------------
    let (pre_status, total, jobs_done): (String, i64, i64) = {
        let conn = state.db.lock();
        conn.query_row(
            "SELECT status, total, done FROM jobs WHERE id=?1",
            params![job_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .expect("读取 job 失败")
    };
    let pre_done_entries: i64 = scalar(
        &state,
        "SELECT COUNT(*) FROM entries WHERE job_id=?1 AND status='done'",
        &job_id,
    );
    let pre_planned: i64 = scalar(
        &state,
        "SELECT COUNT(*) FROM entries WHERE job_id=?1 AND status='planned'",
        &job_id,
    );
    let (src_before, _) = count_files(Path::new(&root));
    let parts_before = count_job_parts(out, &prefix);

    // ---- 启动收敛（产品启动时的真实调用） --------------------------------
    state.reconcile_on_startup();
    let post_status = status_of(&state, &job_id);
    let token_cleared: bool = {
        let conn = state.db.lock();
        conn.query_row(
            "SELECT undo_precheck_token IS NULL FROM jobs WHERE id=?1",
            params![job_id],
            |r| r.get(0),
        )
        .unwrap_or(false)
    };
    let undo_check_cleared: bool = scalar::<i64>(
        &state,
        "SELECT COUNT(*) FROM entries WHERE job_id=?1 AND undo_check IS NOT NULL",
        &job_id,
    ) == 0;

    // ---- 确定性注入：无论随机强杀有没有抓到真实 .part，
    //      都覆盖「清理前缀精确匹配 + 不误删其他 .part」这条路径 ----------
    if inject > 0 {
        fs::create_dir_all(out).expect("创建输出目录失败");
        for k in 0..inject {
            fs::write(out.join(format!("{prefix}9{k:05}.part")), b"stale").unwrap();
        }
        // 不同前缀的无关文件：收尾绝不能删它。
        fs::write(out.join("other_keep.part"), b"keep").unwrap();
    }
    let parts_after_inject = count_job_parts(out, &prefix);
    let stray = out.join("other_keep.part");

    // ---- 崩溃一致性收尾（产品同一份逻辑） --------------------------------
    let rec = match maintenance::resume_interrupted_impl(&state, &job_id) {
        Ok(r) => r,
        Err(e) => {
            println!(
                "VERIFY_JSON={}",
                json!({ "round": round, "mode": mode, "jobId": job_id,
                        "pass": false, "reasons": [format!("resume 失败：{} {}", e.code, e.message)] })
            );
            return;
        }
    };
    let parts_after = count_job_parts(out, &prefix);
    let stray_kept = inject == 0 || stray.exists();

    // ---- 撤销三段式复核 + 执行 ------------------------------------------
    let pre = match undo::precheck_undo(&state, &job_id, noop_channel()) {
        Ok(p) => p,
        Err(e) => {
            println!(
                "VERIFY_JSON={}",
                json!({ "round": round, "mode": mode, "jobId": job_id,
                        "pass": false, "reasons": [format!("precheck 失败：{} {}", e.code, e.message)] })
            );
            return;
        }
    };
    let pre_conflicts: i64 = pre.conflicts.iter().map(|c| c.count).sum();
    let und = match undo::undo_job(&state, &job_id, &pre.token, noop_channel()) {
        Ok(u) => u,
        Err(e) => {
            println!(
                "VERIFY_JSON={}",
                json!({ "round": round, "mode": mode, "jobId": job_id,
                        "pass": false, "reasons": [format!("undo 失败：{} {}", e.code, e.message)] })
            );
            return;
        }
    };
    // 重复撤销：第一次成功后 status=undone / undo_available=0，第二次必须被拒绝。
    let undo_again_error = match undo::undo_job(&state, &job_id, &pre.token, noop_channel()) {
        Ok(_) => String::new(),
        Err(e) => e.code,
    };

    // ---- 文件系统最终一致性 ----------------------------------------------
    let (src_after, _) = count_files(Path::new(&root));
    let orig_restored = count_orig_names(Path::new(&root));
    let expected_out = if inject > 0 { 1 } else { 0 }; // 仅保留无关 .part
    let (out_after, out_after_reads) = count_files_settled(out, expected_out);

    let mut reasons: Vec<String> = Vec::new();
    let mut check = |ok: bool, msg: String| {
        if !ok {
            reasons.push(msg);
        }
    };
    check(pre_status == "executing", format!("崩溃时状态应为 executing，实际 {pre_status}"));
    check(total > 0 && src_before == total, format!("样本文件数异常：before={src_before} total={total}"));
    check(post_status == "interrupted", format!("reconcile 后应为 interrupted，实际 {post_status}"));
    check(token_cleared, "reconcile 未清空旧复核令牌".into());
    check(undo_check_cleared, "reconcile 未清空 entries.undo_check".into());
    check(rec.status == "completed", format!("收尾状态异常：{}", rec.status));
    check(rec.undo_available, "收尾后撤销资格丢失".into());
    check(rec.needs_review == 0, format!("出现待人工核查条目：{}", rec.needs_review));
    check(
        rec.reconciled_done + rec.marked_skipped + rec.needs_review == pre_planned,
        format!("条目反推不守恒：done={} skipped={} review={} planned={}",
                rec.reconciled_done, rec.marked_skipped, rec.needs_review, pre_planned),
    );
    check(parts_after == 0, format!("收尾后仍有本任务 .part：{parts_after} 个"));
    check(stray_kept, "误删了非本任务前缀的 .part".into());
    check(
        rec.parts_removed >= inject,
        format!("清理数少于注入数：{} < {inject}", rec.parts_removed),
    );
    check(pre_conflicts == 0, format!("撤销复核存在冲突：{pre_conflicts}"));
    check(pre.ok_count == pre.total, format!("复核 ok={} total={}", pre.ok_count, pre.total));
    check(und.conflicts == 0, format!("撤销存在冲突：{}", und.conflicts));
    check(und.undone == pre.total, format!("撤销条数不符：undone={} expected={}", und.undone, pre.total));
    check(und.status == "undone", format!("撤销后状态异常：{}", und.status));
    check(
        undo_again_error == "UNDO_NOT_AVAILABLE",
        format!("重复撤销未被拦截：{undo_again_error:?}"),
    );
    check(src_after == total, format!("撤销后源文件数不符：{src_after} != {total}"));
    check(orig_restored == total, format!("原始文件名未全部还原：{orig_restored} != {total}"));
    check(
        out_after == expected_out,
        format!("输出目录残留不符：{out_after} != {expected_out}"),
    );

    let v: Value = json!({
        "round": round,
        "mode": mode,
        "jobId": job_id,
        "total": total,
        "preStatus": pre_status,
        "jobsDone": jobs_done,
        "preDoneEntries": pre_done_entries,
        "prePlanned": pre_planned,
        "partsBefore": parts_before,
        "partsAfterInject": parts_after_inject,
        "postStatus": post_status,
        "tokenCleared": token_cleared,
        "undoCheckCleared": undo_check_cleared,
        "injectedParts": inject,
        "resume": {
            "partsRemoved": rec.parts_removed,
            "reconciledDone": rec.reconciled_done,
            "markedSkipped": rec.marked_skipped,
            "needsReview": rec.needs_review,
            "undoAvailable": rec.undo_available,
            "status": rec.status,
        },
        "partsAfter": parts_after,
        "strayKept": stray_kept,
        "precheck": { "total": pre.total, "okCount": pre.ok_count, "conflicts": pre_conflicts },
        "undo": { "undone": und.undone, "conflicts": und.conflicts, "status": und.status },
        "undoAgainError": undo_again_error,
        "srcBefore": src_before,
        "srcAfter": src_after,
        "origRestored": orig_restored,
        "outAfter": out_after,
        "outAfterReads": out_after_reads,
        "expectedOut": expected_out,
        "pass": reasons.is_empty(),
        "reasons": reasons,
    });
    println!("VERIFY_JSON={v}");
}


// ---------------------------------------------------------------------------
// 扩展场景 A：undo_job 执行中硬退出 / 重启后补撤
// ---------------------------------------------------------------------------
fn phase_crash_undo() -> ! {
    let root = arg("root");
    let mode = arg("mode");
    let outdir = arg("outdir");
    let db_path = arg("db");
    let ratio: f64 = opt("ratio", "0.4").parse().expect("ratio 非法");
    assert!((0.0..1.0).contains(&ratio), "ratio 必须在 (0,1)");

    let (state, job_id, total) = prepare_executing(&root, &mode, &outdir, &db_path);
    execute::start_execute(state.clone(), job_id.clone(), noop_channel())
        .expect("start_execute 失败");
    wait_execute_done(&state, &job_id, Duration::from_secs(900));

    // 复核必须在同一进程内完成：令牌存 DB，跨进程可读，但这里要让 undo_job
    // 真的「持令牌执行到一半」。
    let pre = undo::precheck_undo(&state, &job_id, noop_channel())
        .expect("precheck_undo 失败");
    assert!(pre.ok_count > 0, "没有可撤销条目");
    let token = pre.token.clone();
    let target = ((pre.ok_count as f64) * ratio).max(1.0) as u64;

    println!(
        "CRASH_READY job_id={job_id} total={total} undo_total={} ratio={ratio}",
        pre.ok_count
    );
    let _ = std::io::stdout().flush();

    // undo_job 是同步函数：放到工作线程，主线程按运行时原子计数下杀手。
    let st = state.clone();
    let jid = job_id.clone();
    thread::spawn(move || {
        let _ = undo::undo_job(&st, &jid, &token, noop_channel());
    });

    // 先等 undo_job 装上 runtime，避免把启动前的空窗误判成「已跑完」。
    let boot = Instant::now();
    loop {
        if state.get_runtime(&job_id).is_some() {
            break;
        }
        if boot.elapsed() > Duration::from_secs(30) {
            eprintln!("UNDO_RUNTIME_MISSING");
            process::exit(141);
        }
        thread::sleep(Duration::from_millis(1));
    }

    let deadline = Instant::now() + Duration::from_secs(900);
    loop {
        let done = match state.get_runtime(&job_id) {
            Some(rt) => rt.done.load(Ordering::Relaxed),
            None => {
                eprintln!("CRASH_MISSED undo finished before ratio={ratio}");
                process::exit(139);
            }
        };
        if done >= target {
            eprintln!("KILL_AT_UNDO done={done} target={target}");
            process::exit(137);
        }
        if Instant::now() > deadline {
            eprintln!("CRASH_TIMEOUT_UNDO done={done} target={target}");
            process::exit(138);
        }
        thread::sleep(Duration::from_millis(1));
    }
}

fn phase_verify_undo() {
    let root = arg("root");
    let outdir = arg("outdir");
    let db_path = arg("db");
    let mode = arg("mode");
    let round: i64 = opt("round", "0").parse().unwrap_or(0);
    let out = Path::new(&outdir);

    // 不复用崩溃进程的任何内存状态：重新 Db::open == 应用重启。
    let state = AppState::new(Db::open(Path::new(&db_path)).expect("打开 DB 失败"));
    let job_id: String = {
        let conn = state.db.lock();
        conn.query_row("SELECT id FROM jobs LIMIT 1", [], |r| r.get(0))
            .expect("DB 中没有任务")
    };
    let prefix = format!("__gre_{}_", &job_id[..8.min(job_id.len())]);

    // ---- 崩溃现场快照（reconcile 之前） ----------------------------------
    let (pre_status, total, pre_undo_available, token_present): (String, i64, i64, bool) = {
        let conn = state.db.lock();
        conn.query_row(
            "SELECT status, total, undo_available,
                    undo_precheck_token IS NOT NULL FROM jobs WHERE id=?1",
            params![job_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .expect("读取 job 失败")
    };
    let pre_done: i64 = scalar(
        &state,
        "SELECT COUNT(*) FROM entries WHERE job_id=?1 AND status='done'",
        &job_id,
    );
    let pre_undone: i64 = scalar(
        &state,
        "SELECT COUNT(*) FROM entries WHERE job_id=?1 AND status='undone'",
        &job_id,
    );
    let (src_before, _) = count_files(Path::new(&root));
    let orig_before = count_orig_names(Path::new(&root));
    let dst_before = count_renamed_names(if mode == "to_dir" { out } else { Path::new(&root) });
    let parts_before = count_job_parts(out, &prefix);

    // 观察项：undo_job 期间 jobs.status 是否仍停留在 completed。
    // reconcile_on_startup 只收敛 'undoing' 等非终态，若状态滞后则不会命中。
    let status_lag = pre_status == "completed";

    // ---- 启动收敛 ---------------------------------------------------------
    state.reconcile_on_startup();
    let post_status = status_of(&state, &job_id);
    let token_cleared: bool = {
        let conn = state.db.lock();
        conn.query_row(
            "SELECT undo_precheck_token IS NULL FROM jobs WHERE id=?1",
            params![job_id],
            |r| r.get(0),
        )
        .unwrap_or(false)
    };
    let undo_check_cleared: bool = scalar::<i64>(
        &state,
        "SELECT COUNT(*) FROM entries WHERE job_id=?1 AND undo_check IS NOT NULL",
        &job_id,
    ) == 0;

    // ---- 重新复核 + 补撤剩余（产品真实路径，不调用 resume_interrupted） ----
    let pre = match undo::precheck_undo(&state, &job_id, noop_channel()) {
        Ok(p) => p,
        Err(e) => {
            println!("VERIFY_JSON={}", json!({
                "round": round, "scenario": "undo_kill", "mode": mode,
                "jobId": job_id, "pass": false,
                "reasons": [format!("重新复核失败：{} {}", e.code, e.message)] }));
            return;
        }
    };
    let pre_conflicts: i64 = pre.conflicts.iter().map(|c| c.count).sum();
    let und = match undo::undo_job(&state, &job_id, &pre.token, noop_channel()) {
        Ok(u) => u,
        Err(e) => {
            println!("VERIFY_JSON={}", json!({
                "round": round, "scenario": "undo_kill", "mode": mode,
                "jobId": job_id, "pass": false,
                "reasons": [format!("补撤失败：{} {}", e.code, e.message)] }));
            return;
        }
    };
    let undo_again_error = match undo::undo_job(&state, &job_id, &pre.token, noop_channel()) {
        Ok(_) => String::new(),
        Err(e) => e.code,
    };

    // ---- 文件系统最终一致性 ----------------------------------------------
    let (src_after, _) = count_files(Path::new(&root));
    let orig_after = count_orig_names(Path::new(&root));
    let dst_after = count_renamed_names(if mode == "to_dir" { out } else { Path::new(&root) });
    let (out_after, _) = count_files(out);
    let parts_after = count_job_parts(out, &prefix);

    let mut reasons: Vec<String> = Vec::new();
    let mut check = |ok: bool, msg: String| {
        if !ok {
            reasons.push(msg);
        }
    };
    check(total > 0 && src_before == total, format!("样本文件数异常：before={src_before} total={total}"));
    check(pre_undo_available == 1, format!("崩溃时撤销资格丢失：{pre_undo_available}"));
    check(pre_done > 0, "崩溃时没有剩余可撤销条目（杀点太晚）".into());
    // 杀点可能落在首个 500 条事务批次内：文件系统已还原一部分，但 DB 里
    // 还没有任何 undone 条目。此时自愈条数会 > 0，两者之和 > 0 即证明窗口存在。
    check(
        pre_undone + pre.already_undone > 0,
        "崩溃现场没有任何已还原条目（未制造出崩溃窗口）".into(),
    );
    check(pre_done + pre_undone == total, format!("崩溃现场条目不守恒：done={pre_done} undone={pre_undone} total={total}"));
    check(undo_check_cleared, "reconcile 未清空 entries.undo_check".into());
    check(pre.total == pre_done, format!("重新复核条数不符：{} != {pre_done}", pre.total));
    check(
        pre.ok_count + pre.already_undone == pre.total,
        format!("复核口径不守恒：ok={} 自愈={} total={}",
                pre.ok_count, pre.already_undone, pre.total),
    );
    check(pre_conflicts == 0, format!("复核存在冲突：{pre_conflicts}"));
    check(und.conflicts == 0, format!("补撤存在冲突：{}", und.conflicts));
    check(
        und.undone == pre.ok_count,
        format!("补撤条数不符：{} != 可撤 {}", und.undone, pre.ok_count),
    );
    check(und.status == "undone", format!("最终状态异常：{}", und.status));
    check(
        pre_undone + pre.already_undone + und.undone == total,
        format!("撤销守恒失败：崩溃前已撤 {pre_undone} + 自愈 {} + 补撤 {} != {total}",
                pre.already_undone, und.undone),
    );
    check(undo_again_error == "UNDO_NOT_AVAILABLE", format!("重复撤销未被拦截：{undo_again_error:?}"));
    check(src_after == total, format!("源文件数不符：{src_after} != {total}"));
    check(orig_after == total, format!("原始文件名未全部还原：{orig_after} != {total}"));
    check(dst_after == 0, format!("仍残留重命名产物：{dst_after} 个"));
    check(parts_after == 0, format!("仍残留本任务 .part：{parts_after} 个"));
    check(out_after == 0, format!("输出目录未清空：{out_after} 个文件"));
    if mode == "in_place" {
        check(orig_before + dst_before <= total + 1, "崩溃现场文件数超界".into());
    }

    let v: Value = json!({
        "round": round,
        "scenario": "undo_kill",
        "mode": mode,
        "jobId": job_id,
        "total": total,
        "preStatus": pre_status,
        "preUndoAvailable": pre_undo_available,
        "preTokenPresent": token_present,
        "preDone": pre_done,
        "preUndone": pre_undone,
        "statusLag": status_lag,
        "postStatus": post_status,
        "tokenClearedByReconcile": token_cleared,
        "undoCheckCleared": undo_check_cleared,
        "precheck": { "total": pre.total, "okCount": pre.ok_count,
                      "alreadyUndone": pre.already_undone, "conflicts": pre_conflicts },
        "undo": { "undone": und.undone, "conflicts": und.conflicts, "status": und.status },
        "undoAgainError": undo_again_error,
        "srcBefore": src_before,
        "srcAfter": src_after,
        "origBefore": orig_before,
        "origAfter": orig_after,
        "dstBefore": dst_before,
        "dstAfter": dst_after,
        "outAfter": out_after,
        "partsBefore": parts_before,
        "partsAfter": parts_after,
        "pass": reasons.is_empty(),
        "reasons": reasons,
    });
    println!("VERIFY_JSON={v}");
}

// ---------------------------------------------------------------------------
// 扩展场景 B：扫描期硬退出 / 重启后收敛
// ---------------------------------------------------------------------------
fn phase_crash_scan() -> ! {
    let root = arg("root");
    let db_path = arg("db");
    let kill_at: u64 = opt("scan-kill-at", "2000").parse().expect("scan-kill-at 非法");
    let job_id = format!("crashscan_{}", uuid::Uuid::new_v4().simple());

    let db_file = PathBuf::from(&db_path);
    if let Some(p) = db_file.parent() {
        fs::create_dir_all(p).expect("创建 DB 目录失败");
    }
    if db_file.exists() {
        fs::remove_file(&db_file).expect("清理旧 DB 失败");
    }
    let state = AppState::new(Db::open(&db_file).expect("打开 DB 失败"));
    state.reconcile_on_startup();

    scan::start_scan(
        state.clone(),
        job_id.clone(),
        root.clone(),
        true,
        None,
        noop_channel(),
    )
    .expect("start_scan 失败");
    println!("CRASH_READY job_id={job_id} phase=scan kill_at={kill_at}");
    let _ = std::io::stdout().flush();

    let deadline = Instant::now() + Duration::from_secs(600);
    loop {
        let s = status_of(&state, &job_id);
        let done = state
            .get_runtime(&job_id)
            .map(|rt| rt.done.load(Ordering::Relaxed))
            .unwrap_or(0);
        if s == "scanning" && done >= kill_at {
            eprintln!("KILL_AT_SCAN done={done} target={kill_at}");
            process::exit(137);
        }
        if s == "planned" {
            eprintln!("CRASH_MISSED scan finished before kill_at={kill_at}");
            process::exit(139);
        }
        if matches!(s.as_str(), "failed" | "interrupted" | "cancelled") {
            eprintln!("CRASH_BAD_SCAN_STATUS status={s}");
            process::exit(142);
        }
        if Instant::now() > deadline {
            eprintln!("CRASH_TIMEOUT_SCAN done={done} target={kill_at}");
            process::exit(138);
        }
        thread::sleep(Duration::from_millis(1));
    }
}

fn phase_verify_scan() {
    let root = arg("root");
    let db_path = arg("db");
    let round: i64 = opt("round", "0").parse().unwrap_or(0);
    let expected: i64 = opt("expected-total", "0").parse().unwrap_or(0);
    let r = Path::new(&root);

    let state = AppState::new(Db::open(Path::new(&db_path)).expect("打开 DB 失败"));
    let job_id: String = {
        let conn = state.db.lock();
        conn.query_row("SELECT id FROM jobs LIMIT 1", [], |r| r.get(0))
            .expect("DB 中没有任务")
    };

    // ---- 崩溃现场快照 -----------------------------------------------------
    let (pre_status, pre_total): (String, i64) = {
        let conn = state.db.lock();
        conn.query_row(
            "SELECT status, total FROM jobs WHERE id=?1",
            params![job_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("读取 job 失败")
    };
    let entries_all: i64 = scalar(&state, "SELECT COUNT(*) FROM entries WHERE job_id=?1", &job_id);
    let entries_scanned: i64 = scalar(
        &state,
        "SELECT COUNT(*) FROM entries WHERE job_id=?1 AND status='scanned'",
        &job_id,
    );
    let (src_before, parts_before) = count_files(r);
    let orig_before = count_orig_names(r);
    let dst_before = count_renamed_names(r);

    // ---- 启动收敛 + 崩溃收尾 ---------------------------------------------
    state.reconcile_on_startup();
    let post_status = status_of(&state, &job_id);
    let rec = match maintenance::resume_interrupted_impl(&state, &job_id) {
        Ok(x) => x,
        Err(e) => {
            println!("VERIFY_JSON={}", json!({
                "round": round, "scenario": "scan_kill", "jobId": job_id,
                "pass": false,
                "reasons": [format!("收尾失败：{} {}", e.code, e.message)] }));
            return;
        }
    };
    let (final_status, undo_available): (String, i64) = {
        let conn = state.db.lock();
        conn.query_row(
            "SELECT status, undo_available FROM jobs WHERE id=?1",
            params![job_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("读取终态失败")
    };
    let entries_scanned_after: i64 = scalar(
        &state,
        "SELECT COUNT(*) FROM entries WHERE job_id=?1 AND status='scanned'",
        &job_id,
    );

    // 撤销资格绝不能被误开：没有任何一条执行过的条目。
    let precheck_error = match undo::precheck_undo(&state, &job_id, noop_channel()) {
        Ok(_) => String::new(),
        Err(e) => e.code,
    };

    // DB 没有被半截扫描卡死：重新发起一次扫描应正常完成。
    let job2 = format!("rescan_{}", uuid::Uuid::new_v4().simple());
    let rescan_ok = scan::start_scan(
        state.clone(),
        job2.clone(),
        root.clone(),
        true,
        None,
        noop_channel(),
    )
    .is_ok();
    if rescan_ok {
        wait_scan(&state, &job2, Duration::from_secs(600));
    }
    let rescan_total: i64 = scalar(&state, "SELECT total FROM jobs WHERE id=?1", &job2);
    let rescan_status = status_of(&state, &job2);

    // ---- 文件系统 ---------------------------------------------------------
    let (src_after, parts_after) = count_files(r);
    let orig_after = count_orig_names(r);
    let dst_after = count_renamed_names(r);

    let mut reasons: Vec<String> = Vec::new();
    let mut check = |ok: bool, msg: String| {
        if !ok {
            reasons.push(msg);
        }
    };
    check(pre_status == "scanning", format!("崩溃时状态应为 scanning，实际 {pre_status}"));
    check(entries_scanned > 0, "崩溃时没有已入库条目（杀点太早）".into());
    check(entries_scanned == entries_all, format!("扫描期混入非 scanned 条目：{entries_scanned}/{entries_all}"));
    check(post_status == "interrupted", format!("reconcile 后应为 interrupted，实际 {post_status}"));
    check(rec.status == "completed", format!("收尾状态异常：{}", rec.status));
    check(!rec.undo_available, "扫描期中断误开了撤销资格".into());
    check(undo_available == 0, format!("DB 中撤销资格未关闭：{undo_available}"));
    check(final_status == "completed", format!("终态异常：{final_status}"));
    check(
        rec.reconciled_done + rec.marked_skipped + rec.needs_review == 0,
        format!("扫描期不应反推执行条目：done={} skipped={} review={}",
                rec.reconciled_done, rec.marked_skipped, rec.needs_review),
    );
    check(entries_scanned_after == entries_scanned, "收尾误改了 scanned 条目状态".into());
    check(precheck_error == "UNDO_NOT_AVAILABLE", format!("撤销未被正确拒绝：{precheck_error:?}"));
    check(rescan_ok && rescan_status == "planned", format!("重新扫描失败：status={rescan_status}"));
    check(expected == 0 || rescan_total == expected, format!("重扫总数不符：{rescan_total} != {expected}"));
    check(src_before == src_after, format!("源文件数变化：{src_before} → {src_after}"));
    check(expected == 0 || orig_after == expected, format!("原始文件名不全：{orig_after} != {expected}"));
    check(orig_before == orig_after, "原始文件名数量在收尾中变化".into());
    check(dst_before == 0 && dst_after == 0, "扫描期不应产生重命名产物".into());
    check(parts_before == 0 && parts_after == 0, "扫描期不应产生 .part".into());

    let v: Value = json!({
        "round": round,
        "scenario": "scan_kill",
        "jobId": job_id,
        "preStatus": pre_status,
        "preTotal": pre_total,
        "entriesAll": entries_all,
        "entriesScanned": entries_scanned,
        "entriesScannedAfter": entries_scanned_after,
        "postStatus": post_status,
        "finalStatus": final_status,
        "undoAvailable": undo_available,
        "recover": {
            "partsRemoved": rec.parts_removed,
            "reconciledDone": rec.reconciled_done,
            "markedSkipped": rec.marked_skipped,
            "needsReview": rec.needs_review,
            "undoAvailable": rec.undo_available,
            "status": rec.status,
        },
        "precheckError": precheck_error,
        "rescanTotal": rescan_total,
        "rescanStatus": rescan_status,
        "srcBefore": src_before,
        "srcAfter": src_after,
        "origBefore": orig_before,
        "origAfter": orig_after,
        "dstAfter": dst_after,
        "partsBefore": parts_before,
        "partsAfter": parts_after,
        "expectedTotal": expected,
        "pass": reasons.is_empty(),
        "reasons": reasons,
    });
    println!("VERIFY_JSON={v}");
}

// ---------------------------------------------------------------------------
// v1.20 相位：两阶段临时名的真实强杀注入（§12.3 ⑪ 前置验证）
// ---------------------------------------------------------------------------
//
// 为什么不能沿用 M2 的「按 JobRuntime.done 比例」杀点：
//   `stage_blockers()` 成功让位时**不增加 rt.done**（只有失败才计），阶段 1
//   全程 done 恒为 0；阶段 2 也要等 500 条批量回写才跳变一次。因此这里改用
//   **文件系统探针**：第 K 个临时名的「出现」与「消失」分别对应阶段 1、
//   阶段 2 推进到第 K 条，二者都是单条 rename 的即时效果，无批量滞后。
//
// 样本是确定性的链式移位（s_000000 → s_000001 → … → free.dat），除第一条外
// 每个源名都坐在另一个条目的目标位上，故 N-1 条都是挡路者，阶段 1 有足够长
// 的窗口让杀点稳定命中。普通批量改名挡路者恒为 0，本样本只用于崩溃验证。
//
// 计划手工写入而不经 `build_plan`：jwalk 并行扫描不保证 seq 与文件名字典序
// 一致，而探针必须能精确预知第 K 个临时名的路径。**执行与恢复全部走产品代码**
// （`execute::start_execute` / `reconcile_on_startup` / `resume_interrupted_impl`）。

fn stage_payload(i: i64) -> String {
    format!("payload-{i:06}")
}

fn stage_src_name(i: i64) -> String {
    format!("s_{i:06}.dat")
}

fn stage_dst_name(i: i64, n: i64) -> String {
    if i + 1 < n {
        stage_src_name(i + 1)
    } else {
        "free.dat".to_string()
    }
}

fn stage_tmp_path(root: &Path, job_id: &str, seq: i64) -> PathBuf {
    root.join(format!("__gre_{}_{seq}.tmp", &job_id[..8.min(job_id.len())]))
}

/// 造文件 + 落库，返回挡路者条数。
fn stage_seed(state: &AppState, root: &Path, job_id: &str, n: i64) -> i64 {
    fs::create_dir_all(root).expect("创建样本目录失败");
    let conn = state.db.lock();
    conn.execute(
        "INSERT INTO jobs (id, session_id, status, root_path, recursive, output_mode,
                           output_dir, total, bytes_total, created_at, undo_available)
         VALUES (?1, 'm4c', 'planned', ?2, 0, 'in_place', NULL, ?3, 0, 0, 0)",
        params![
            job_id,
            root.to_string_lossy().to_string(),
            n
        ],
    )
    .expect("插入 job 失败");

    for i in 0..n {
        let body = stage_payload(i);
        let src_name = stage_src_name(i);
        let dst_name = stage_dst_name(i, n);
        let src = root.join(&src_name);
        let dst = root.join(&dst_name);
        fs::write(&src, &body).expect("写样本文件失败");
        let seq = i + 1;
        // 源名 s_j (j>=1) 落在目标集合 {s_1..s_(n-1), free} 里 → 挡路者。
        let tmp: Option<String> = if seq >= 2 {
            Some(stage_tmp_path(root, job_id, seq).to_string_lossy().into_owned())
        } else {
            None
        };
        conn.execute(
            "INSERT INTO entries (job_id, seq, src_path, src_name, src_size, rel_dir,
                                  status, dst_name, dst_path, dst_key, tmp_path)
             VALUES (?1, ?2, ?3, ?4, ?5, '', 'planned', ?6, ?7, ?8, ?9)",
            params![
                job_id,
                seq,
                src.to_string_lossy().to_string(),
                src_name,
                body.len() as i64,
                dst_name,
                dst.to_string_lossy().to_string(),
                format!("k{seq:06}"),
                tmp
            ],
        )
        .expect("插入 entry 失败");
    }
    n - 1
}

/// 扫一遍目录：普通文件数、临时名数、内容 multiset。
fn stage_snapshot(root: &Path, n: i64) -> (i64, i64, std::collections::HashMap<String, i64>) {
    let mut files = 0i64;
    let mut tmps = 0i64;
    let mut contents: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    for e in fs::read_dir(root).expect("读取样本目录失败") {
        let p = e.expect("目录项").path();
        if !p.is_file() {
            continue;
        }
        let name = p.file_name().unwrap_or_default().to_string_lossy().to_string();
        if name.starts_with("__gre_") {
            tmps += 1;
        } else {
            files += 1;
        }
        let body = fs::read_to_string(&p).unwrap_or_else(|_| format!("<binary-{name}>"));
        *contents.entry(body).or_insert(0) += 1;
    }
    let _ = n;
    (files, tmps, contents)
}

/// 内容守恒：每个 payload 恰好出现一次，且没有额外内容。
fn stage_content_ok(contents: &std::collections::HashMap<String, i64>, n: i64) -> bool {
    if contents.len() != n as usize {
        return false;
    }
    (0..n).all(|i| contents.get(&stage_payload(i)).copied().unwrap_or(0) == 1)
}

/// self 模式：同进程高频监视探针，命中即刻硬退出（无析构、无 flush、无 status 回写）。
fn stage_watch_and_exit(root: &Path, job_id: &str, n: i64, stage: i64, kill_at: i64) -> ! {
    let probe = stage_tmp_path(root, job_id, kill_at + 1);
    let tail = stage_tmp_path(root, job_id, (kill_at + 41).min(n));
    let mut seen = false;
    let started = Instant::now();
    loop {
        let exists = probe.exists();
        let hit = if stage == 1 {
            exists
        } else {
            if exists {
                seen = true;
            }
            seen && !exists && tail.exists()
        };
        if hit {
            eprintln!(
                "KILL_AT stage={stage} kill_at={kill_at} probe={} elapsed_ms={}",
                probe.file_name().unwrap_or_default().to_string_lossy(),
                started.elapsed().as_millis()
            );
            process::exit(137);
        }
        if started.elapsed() > Duration::from_secs(300) {
            eprintln!("KILL_MISSED stage={stage} kill_at={kill_at}");
            process::exit(139);
        }
        thread::sleep(Duration::from_millis(1));
    }
}

fn phase_crash_stage() -> ! {
    let root = PathBuf::from(arg("root"));
    let db_path = arg("db");
    let n: i64 = opt("count", "2000").parse().expect("count 非法");
    let stage: i64 = opt("stage", "1").parse().expect("stage 非法");
    let kill_at: i64 = opt("kill-at", "400").parse().expect("kill-at 非法");
    let kill_mode = opt("kill-mode", "self");
    let manifest_path = arg("manifest");
    assert!(n >= 100, "count 至少 100，否则阶段 1 窗口太短");
    assert!(stage == 1 || stage == 2, "stage 只能是 1 或 2");
    assert!(kill_at >= 1 && kill_at + 41 < n, "kill-at 必须留出后续探针余量");

    let db_file = PathBuf::from(&db_path);
    if let Some(p) = db_file.parent() {
        fs::create_dir_all(p).expect("创建 DB 目录失败");
    }
    if db_file.exists() {
        fs::remove_file(&db_file).expect("清理旧 DB 失败");
    }
    let job_id = uuid::Uuid::new_v4().simple().to_string();
    let state = AppState::new(Db::open(&db_file).expect("打开 DB 失败"));
    state.reconcile_on_startup();
    let blockers = stage_seed(&state, &root, &job_id, n);

    let manifest = json!({
        "jobId": job_id,
        "root": root.to_string_lossy().to_string(),
        "db": db_path,
        "count": n,
        "blockers": blockers,
        "tmpPrefix": format!("__gre_{}_", &job_id[..8.min(job_id.len())]),
        "stageGoal": stage,
        "killAt": kill_at,
        "killMode": kill_mode,
    });
    fs::write(
        &manifest_path,
        serde_json::to_string_pretty(&manifest).expect("序列化 manifest 失败"),
    )
    .expect("写 manifest 失败");

    println!(
        "STAGE_READY pid={} job={} n={} blockers={} stage={} kill_at={} mode={}",
        process::id(), job_id, n, blockers, stage, kill_at, kill_mode
    );
    let _ = std::io::stdout().flush();

    // external 模式不做任何挂起/钩子：Rust 侧照常跑，杀点完全由 Python 侧
    // 用同一组探针判定后 taskkill /F（见 O-3：挂起主线程会让执行线程继续跑完）。
    if kill_mode == "self" {
        let r = root.clone();
        let id = job_id.clone();
        let handle = thread::spawn(move || stage_watch_and_exit(&r, &id, n, stage, kill_at));
        // 监视线程会 exit 整个进程；这里只是防止它被提前 join。
        drop(handle);
    }

    execute::start_execute(state.clone(), job_id.clone(), noop_channel())
        .expect("start_execute 失败");

    let started = Instant::now();
    loop {
        let st: String = scalar(&state, "SELECT status FROM jobs WHERE id=?1", &job_id);
        if st == "completed" || st == "failed" || started.elapsed() > Duration::from_secs(300) {
            break;
        }
        thread::sleep(Duration::from_millis(2));
    }
    println!("STAGE_FINISHED job={job_id}");
    let _ = std::io::stdout().flush();
    // external 模式走到这里说明 Python 没杀中，退出码 141 让驱动判「未命中」。
    process::exit(if kill_mode == "external" { 141 } else { 0 });
}

fn phase_verify_stage() {
    let manifest_path = arg("manifest");
    let stage_goal: i64 = opt("stage", "1").parse().expect("stage 非法");
    let m: Value = serde_json::from_str(
        &fs::read_to_string(&manifest_path).expect("读 manifest 失败"),
    )
    .expect("解析 manifest 失败");
    let job_id = m["jobId"].as_str().expect("缺 jobId").to_string();
    let root = PathBuf::from(m["root"].as_str().expect("缺 root"));
    let db_path = m["db"].as_str().expect("缺 db");
    let n = m["count"].as_i64().expect("缺 count");

    // ① 崩溃现场快照（resume 之前）：内容必须守恒，一个不丢、一个不重。
    let (files_before, tmp_before, contents_before) = stage_snapshot(&root, n);
    let content_before_ok = stage_content_ok(&contents_before, n);

    let state = AppState::new(Db::open(Path::new(db_path)).expect("重开 DB 失败"));
    let before_status: String =
        scalar(&state, "SELECT status FROM jobs WHERE id=?1", &job_id);

    // 逐状态计数（resume 前）。
    let count_status = |s: &str| -> i64 {
        let conn = state.db.lock();
        conn.query_row(
            "SELECT COUNT(*) FROM entries WHERE job_id=?1 AND status=?2",
            params![job_id, s],
            |r| r.get(0),
        )
        .unwrap_or(0)
    };
    let done_before = count_status("done");
    let planned_before = count_status("planned");

    // ② 产品收敛链路：启动标 interrupted → resume 按三者存在性分派。
    state.reconcile_on_startup();
    let interrupted_ok: bool = {
        let conn = state.db.lock();
        conn.query_row(
            "SELECT status='interrupted' FROM jobs WHERE id=?1",
            params![job_id],
            |r| r.get(0),
        )
        .unwrap_or(false)
    };
    let rec = maintenance::resume_interrupted_impl(&state, &job_id);

    // 杀点有效性闸门：现场必须真的停在目标阶段，而不是「已经跑完」。
    let kill_hit = if stage_goal == 1 {
        tmp_before > 0 && done_before == 0 && tmp_before < n - 1
    } else {
        tmp_before > 0 && (done_before > 0 || tmp_before < n - 1) && planned_before > 0
    };

    // ③ 恢复后文件系统：无临时名、内容仍守恒、文件数不变。
    let (files_after, tmp_after, contents_after) = stage_snapshot(&root, n);
    let content_after_ok = stage_content_ok(&contents_after, n);

    // ④ DB 一致性：逐条状态与文件形态对齐，tmp_path 全清。
    let (db_done, db_skipped, db_review, db_failed, db_planned, tmp_left) = {
        let conn = state.db.lock();
        let one = |sql: &str, p: &str| -> i64 {
            conn.query_row(sql, params![job_id, p], |r| r.get(0)).unwrap_or(0)
        };
        let s = "SELECT COUNT(*) FROM entries WHERE job_id=?1 AND status=?2";
        let leftover: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM entries WHERE job_id=?1
                 AND tmp_path IS NOT NULL AND tmp_path<>''",
                params![job_id],
                |r| r.get(0),
            )
            .unwrap_or(-1);
        (
            one(s, "done"),
            one(s, "skipped"),
            one(s, "needs_review"),
            one(s, "failed"),
            one(s, "planned"),
            leftover,
        )
    };

    // 逐条核验：done → 终名在位且内容对位；skipped → 源名在位且内容对位。
    let mut misplaced = 0i64;
    {
        let conn = state.db.lock();
        let mut stmt = conn
            .prepare(
                "SELECT seq, src_path, dst_path, status FROM entries
                 WHERE job_id=?1 ORDER BY seq",
            )
            .expect("prepare entries");
        let rows = stmt
            .query_map(params![job_id], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            })
            .expect("query entries")
            .collect::<Result<Vec<_>, _>>()
            .expect("collect entries");
        for (seq, src, dst, status) in rows {
            let want = stage_payload(seq - 1);
            let path = match status.as_str() {
                "done" => dst,
                "skipped" => src,
                _ => continue,
            };
            match fs::read_to_string(&path) {
                Ok(body) if body == want => {}
                _ => misplaced += 1,
            }
        }
    }

    let job_status: String = scalar(&state, "SELECT status FROM jobs WHERE id=?1", &job_id);
    let undo_flag: i64 = {
        let conn = state.db.lock();
        conn.query_row(
            "SELECT undo_available FROM jobs WHERE id=?1",
            params![job_id],
            |r| r.get(0),
        )
        .unwrap_or(0)
    };

    let (reconciled_done, marked_skipped, needs_review, undo_available, rec_status) = match &rec {
        Ok(r) => (
            r.reconciled_done,
            r.marked_skipped,
            r.needs_review,
            r.undo_available,
            r.status.clone(),
        ),
        Err(e) => (-1, -1, -1, false, format!("ERR:{}", e.code)),
    };

    // 每个实际残留的临时名，都必须在 DB 里对应一条 needs_review 且 tmp_path
    // 仍指向它（人工核查线索不能被抹掉）；反之 DB 保留线索的条目数应与之一致。
    let mut orphan_tmp = 0i64;
    {
        let conn = state.db.lock();
        for e in fs::read_dir(&root).expect("读取样本目录失败") {
            let path = e.expect("目录项").path();
            let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
            if !name.starts_with("__gre_") || !name.ends_with(".tmp") {
                continue;
            }
            let found: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM entries WHERE job_id=?1 AND tmp_path=?2
                     AND status='needs_review'",
                    params![job_id, path.to_string_lossy().to_string()],
                    |r| r.get(0),
                )
                .unwrap_or(0);
            if found == 0 {
                orphan_tmp += 1;
            }
        }
    }

    // 强判据：阶段 1 崩溃时阶段 2 根本没开始，任何 done 都只能是误判。
    // 这正是 v1.20 修复的缺陷（把仍在原位的源文件补记成产物并开放撤销）。
    let no_false_done = if stage_goal == 1 { db_done == 0 } else { true };
    let expected_undo_flag: i64 = if db_done > 0 { 1 } else { 0 };
    let gates = json!({
        "killHit": kill_hit && before_status != "completed",
        "contentBeforeOk": content_before_ok,
        "contentAfterOk": content_after_ok,
        "noFalseDone": no_false_done,
        "reviewHonest": needs_review == db_review && tmp_left == db_review,
        "noOrphanTmp": orphan_tmp == 0,
        "fileCountStable": files_after + tmp_after == n && files_before + tmp_before == n,
        "interruptedOk": interrupted_ok,
        "resumeOk": rec.is_ok(),
        "noPlannedLeft": db_planned == 0,
        "noMisplaced": misplaced == 0,
        "countsMatch": reconciled_done == db_done.saturating_sub(done_before)
            && marked_skipped == db_skipped,
        "jobCompleted": job_status == "completed" && rec_status == "completed",
        "undoFlagOk": undo_available == (db_done > 0) && undo_flag == expected_undo_flag,
    });
    let all_ok = gates
        .as_object()
        .expect("gates 是对象")
        .values()
        .all(|v| v.as_bool().unwrap_or(false));

    let out = json!({
        "jobId": job_id,
        "stageGoal": stage_goal,
        "n": n,
        "beforeStatus": before_status,
        "filesBefore": files_before,
        "tmpBefore": tmp_before,
        "doneBefore": done_before,
        "plannedBefore": planned_before,
        "filesAfter": files_after,
        "tmpAfter": tmp_after,
        "dbDone": db_done,
        "dbSkipped": db_skipped,
        "dbReview": db_review,
        "dbFailed": db_failed,
        "dbPlanned": db_planned,
        "tmpLeft": tmp_left,
        "orphanTmp": orphan_tmp,
        "misplaced": misplaced,
        "reconciledDone": reconciled_done,
        "markedSkipped": marked_skipped,
        "needsReview": needs_review,
        "undoAvailable": undo_available,
        "undoFlag": undo_flag,
        "jobStatus": job_status,
        "recoverStatus": rec_status,
        "gates": gates,
        "verdict": if all_ok { "PASS" } else { "FAIL" },
    });
    if let Ok(p) = std::env::var("GRE_STAGE_VERIFY_OUT") {
        let _ = fs::write(p, serde_json::to_string_pretty(&out).unwrap_or_default());
    }
    println!("VERIFY_STAGE_JSON={out}");
    let _ = std::io::stdout().flush();
}

fn main() {
    match arg("phase").as_str() {
        // 原有 M2 基线相位，10/10 用例继续可复跑。
        "crash" => phase_crash(),
        "verify" => phase_verify(),
        // §12.3 第 ③ 项扩展场景。
        "crash_undo" => phase_crash_undo(),
        "verify_undo" => phase_verify_undo(),
        "crash_scan" => phase_crash_scan(),
        "verify_scan" => phase_verify_scan(),
        // v1.20：两阶段临时名的真实强杀注入。
        "crash_stage" => phase_crash_stage(),
        "verify_stage" => phase_verify_stage(),
        other => panic!("未知 phase：{other}"),
    }
}
