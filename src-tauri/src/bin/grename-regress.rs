//! M3 后端命令级自动回归（设计文档 §12.3 第 ② 项）。
//!
//! 与 grename-bench / grename-crash 一样，用 `Channel::new` 在无 GUI / 无 WebView
//! 环境驱动**产品自身的后端模块**，不是另写的模拟实现。两个相位由 Python 驱动
//! 分两次进程调用，共用同一个 DB 文件，中间不传递任何内存句柄，从而构成真正的
//! 「跨会话」验证：进程 A 写 → 进程正常退出（Db/Connection drop）→ 进程 B 重开。
//!
//! - `seed`：造样本 → 设置写入与校验 → 完整跑 `scan → plan → execute` →
//!   CSV 导出与内容校验 → `precheck_undo` 把复核令牌写入 DB →
//!   再造一个 `to_dir` 任务并手工置为 `interrupted` + 注入 `.part`。
//! - `verify`：重开 DB + `reconcile_on_startup()` → 设置持久化 / 原始编码 /
//!   TTL 与保留期联动 / 失败路径 / CSV 二次导出 → 跨会话撤销闭环 →
//!   TTL 清理 → `resume_interrupted` 收尾后仍可撤销 → 文件系统一致性。
//!
//! 每次断言都记入 `checks` 数组，最后输出单行 `SEED_JSON=` / `REGRESS_JSON=`
//! 供 `done/tools/run_regress.py` 汇总。任一断言失败不改变进程退出码，
//! 便于一次跑完全部用例、拿到完整失败清单。

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use rusqlite::params;
use serde_json::{json, Value};
use tauri::ipc::Channel;
use tauri_app_lib::db::Db;
use tauri_app_lib::maintenance::{self, Settings};
use tauri_app_lib::model::{AppResult, Progress};
use tauri_app_lib::rule::RuleSpec;
use tauri_app_lib::state::{now_ms, AppState};
use tauri_app_lib::{execute, plan, scan, undo};

const CSV_HEADER: &str =
    "job_id,seq,src_path,src_name,src_size,rel_dir,dst_path,dst_name,status,err_code,err_msg";
const INJECT_PARTS: i64 = 3;
const DAY_MS: i64 = 24 * 3_600_000;

fn arg(name: &str) -> String {
    let key = format!("--{name}");
    let a: Vec<String> = env::args().collect();
    for i in 0..a.len().saturating_sub(1) {
        if a[i] == key {
            return a[i + 1].clone();
        }
    }
    panic!("缺少参数 {key}");
}

fn opt(name: &str, default: &str) -> String {
    let key = format!("--{name}");
    let a: Vec<String> = env::args().collect();
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

fn code_of<T>(r: AppResult<T>) -> String {
    match r {
        Ok(_) => String::new(),
        Err(e) => e.code,
    }
}

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

/// 后台任务以 `clear_runtime` 作为收尾信号；同时确认 status 已离开运行态。
fn wait_done(state: &AppState, job_id: &str, timeout: Duration) -> String {
    let t0 = Instant::now();
    loop {
        let s = status_of(state, job_id);
        let running = state.get_runtime(job_id).is_some();
        let in_flight = matches!(
            s.as_str(),
            "scanning" | "executing" | "undo_prechecking" | "undoing"
        );
        if !running && !in_flight {
            return s;
        }
        if t0.elapsed() > timeout {
            panic!("等待任务收尾超时：status={s}");
        }
        thread::sleep(Duration::from_millis(5));
    }
}

fn count_files(dir: &Path) -> i64 {
    if !dir.is_dir() {
        return 0;
    }
    jwalk::WalkDir::new(dir)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .count() as i64
}

fn count_orig_names(dir: &Path) -> i64 {
    if !dir.is_dir() {
        return 0;
    }
    jwalk::WalkDir::new(dir)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| {
            e.file_type().is_file()
                && e.file_name().to_string_lossy().starts_with("f_")
                && e.file_name().to_string_lossy().ends_with(".dat")
        })
        .count() as i64
}

fn count_job_parts(dir: &Path, prefix: &str) -> i64 {
    if !dir.is_dir() {
        return 0;
    }
    jwalk::WalkDir::new(dir)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| {
            e.file_type().is_file()
                && e.file_name().to_string_lossy().starts_with(prefix)
                && e.file_name().to_string_lossy().ends_with(".part")
        })
        .count() as i64
}

/// 只用于 CSV 内容校验的最小解析器：支持双引号包裹与 `""` 转义。
fn csv_fields(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_q = false;
    let mut it = line.chars().peekable();
    while let Some(c) = it.next() {
        if in_q {
            if c == '"' {
                if it.peek() == Some(&'"') {
                    it.next();
                    cur.push('"');
                } else {
                    in_q = false;
                }
            } else {
                cur.push(c);
            }
        } else if c == '"' {
            in_q = true;
        } else if c == ',' {
            out.push(std::mem::take(&mut cur));
        } else {
            cur.push(c);
        }
    }
    out.push(cur);
    out
}

struct Report {
    checks: Vec<(String, bool, String)>,
}

impl Report {
    fn new() -> Self {
        Self { checks: Vec::new() }
    }

    fn check(&mut self, name: &str, ok: bool, detail: impl Into<String>) {
        self.checks.push((name.to_string(), ok, detail.into()));
    }

    fn finish(self, phase: &str, details: Value) -> Value {
        let passed = self.checks.iter().filter(|c| c.1).count();
        let total = self.checks.len();
        let checks: Vec<Value> = self
            .checks
            .iter()
            .map(|(n, ok, d)| json!({"name": n, "pass": ok, "detail": d}))
            .collect();
        json!({
            "phase": phase,
            "pass": passed == total,
            "total": total,
            "passed": passed,
            "checks": checks,
            "details": details,
        })
    }
}

/// CSV 导出校验：BOM、精确表头、行数、11 列、首列 job_id、
/// 数值列裸写、逗号/双引号转义。两个相位各调用一次。
fn check_csv(rep: &mut Report, label: &str, path: &Path, job_id: &str, expected_rows: i64) {
    let bytes = fs::read(path).unwrap_or_default();
    let bom = bytes.starts_with(&[0xEF, 0xBB, 0xBF]);
    rep.check(
        &format!("{label}.bom"),
        bom,
        format!("前 3 字节 = {:?}", &bytes[..3.min(bytes.len())]),
    );
    let text = String::from_utf8_lossy(&bytes[3.min(bytes.len())..]).into_owned();
    let mut lines = text.lines();
    let head = lines.next().unwrap_or("");
    rep.check(
        &format!("{label}.header"),
        head == CSV_HEADER,
        format!("实际表头 = {head}"),
    );
    let data: Vec<&str> = lines.filter(|l| !l.is_empty()).collect();
    rep.check(
        &format!("{label}.row_count"),
        data.len() as i64 == expected_rows,
        format!("CSV 数据行 {} / 预计 {}", data.len(), expected_rows),
    );
    let mut col_ok = true;
    let mut first_ok = true;
    let mut num_ok = true;
    let mut quoted_name = false;
    for line in &data {
        let f = csv_fields(line);
        if f.len() != 11 {
            col_ok = false;
            continue;
        }
        if f[0] != job_id {
            first_ok = false;
        }
        if f[1].parse::<i64>().is_err() || f[4].parse::<i64>().is_err() {
            num_ok = false;
        }
        if f[3] == "q\"uote,comma.dat" {
            quoted_name = true;
        }
    }
    rep.check(&format!("{label}.columns"), col_ok, "每行应恰好 11 列");
    rep.check(&format!("{label}.job_id_col"), first_ok, "首列恒为 job_id");
    rep.check(
        &format!("{label}.numeric_cols"),
        num_ok,
        "seq / src_size 为裸数字",
    );
    rep.check(
        &format!("{label}.quote_escape"),
        text.contains("\"q\"\"uote,comma.dat\""),
        "含逗号与双引号的 src_name 应被正确转义",
    );
    rep.check(
        &format!("{label}.quote_roundtrip"),
        quoted_name,
        "解析回原值 q\"uote,comma.dat",
    );
}

fn custom_settings() -> Settings {
    Settings {
        history_retention_days: 7,
        undo_precheck_ttl_sec: 30,
        // 特意开启：让 seed 阶段的 to_dir 任务真实写入内容指纹，
        // verify 阶段据此验证列写入、跨会话持久化与开关判别力。
        undo_verify_content: true,
        theme: "dark".to_string(),
        locale: "en-US".to_string(),
        // v1.17：默认值也走同一套持久化，这里取非默认值以证明真的写进了库。
        default_recursive: true,
        default_output_mode: "to_dir".to_string(),
    }
}

fn settings_equal(a: &Settings, b: &Settings) -> bool {
    a.history_retention_days == b.history_retention_days
        && a.undo_precheck_ttl_sec == b.undo_precheck_ttl_sec
        && a.undo_verify_content == b.undo_verify_content
        && a.theme == b.theme
        && a.locale == b.locale
        && a.default_recursive == b.default_recursive
        && a.default_output_mode == b.default_output_mode
}

fn make_samples(dir: &Path, total: i64) {
    fs::create_dir_all(dir).expect("创建样本目录失败");
    for e in fs::read_dir(dir).into_iter().flatten().flatten() {
        if e.path().is_file() {
            let _ = fs::remove_file(e.path());
        }
    }
    for i in 0..total {
        // 一个含逗号的文件名，用于验证 CSV 的引号包裹与逗号转义。
        let name = if i == 7 {
            "f_00007,comma.dat".to_string()
        } else {
            format!("f_{i:05}.dat")
        };
        fs::write(dir.join(name), b"").expect("造样本失败");
    }
}

fn rule() -> RuleSpec {
    RuleSpec {
        replacement: "b_{n}".to_string(),
        ..RuleSpec::default()
    }
}

/// 完整跑一条产品链路：scan → plan → execute，返回结束状态。
fn run_job(
    state: &AppState,
    job_id: &str,
    root: &str,
    mode: &str,
    outdir: Option<String>,
) -> String {
    scan::start_scan(
        state.clone(),
        job_id.to_string(),
        root.to_string(),
        true,
        None,
        noop_channel(),
    )
    .expect("start_scan 失败");
    wait_scan(state, job_id, Duration::from_secs(300));
    plan::build_plan(state, job_id, mode, outdir, rule()).expect("build_plan 失败");
    execute::start_execute(state.clone(), job_id.to_string(), noop_channel())
        .expect("start_execute 失败");
    wait_done(state, job_id, Duration::from_secs(600))
}

// ---------------------------------------------------------------------------
// 相位 1：写入与导出
// ---------------------------------------------------------------------------
fn phase_seed() {
    let root = arg("root");
    let root2 = arg("root2");
    let outdir = arg("outdir");
    let db_path = arg("db");
    let csv1 = arg("csv1");
    let total: i64 = opt("total", "60").parse().expect("total 非法");
    let mut rep = Report::new();

    let db_file = PathBuf::from(&db_path);
    if let Some(p) = db_file.parent() {
        fs::create_dir_all(p).expect("创建 DB 目录失败");
    }
    if db_file.exists() {
        fs::remove_file(&db_file).expect("清理旧 DB 失败");
    }
    // WAL 辅文件也要清掉，避免复用上一次的日志。
    for suffix in ["-wal", "-shm"] {
        let p = PathBuf::from(format!("{db_path}{suffix}"));
        if p.exists() {
            let _ = fs::remove_file(p);
        }
    }

    make_samples(Path::new(&root), total);
    make_samples(Path::new(&root2), total);
    fs::create_dir_all(&outdir).expect("创建输出目录失败");

    let state = AppState::new(Db::open(&db_file).expect("打开 DB 失败"));
    state.reconcile_on_startup();

    // ---- ① 设置：默认值 / 合法写入 / 非法值拦截 --------------------------
    let d = Settings::default();
    let s0 = maintenance::load_settings(&state).expect("读取默认设置失败");
    rep.check(
        "settings.default",
        settings_equal(&s0, &d),
        format!(
            "{} / {} / {} / {}",
            s0.history_retention_days, s0.undo_precheck_ttl_sec, s0.theme, s0.locale
        ),
    );
    rep.check(
        "settings.fingerprint_default_off",
        !d.undo_verify_content,
        format!("内容指纹开关默认应关闭，实际 {}", d.undo_verify_content),
    );
    let s1 = maintenance::set_settings_impl(&state, custom_settings())
        .expect("写入设置失败");
    rep.check(
        "settings.fingerprint_write_readback",
        s1.undo_verify_content,
        format!("写入 true 后读回 {}", s1.undo_verify_content),
    );
    rep.check(
        "settings.write_readback",
        settings_equal(&s1, &custom_settings()),
        format!(
            "{} / {} / {} / {}",
            s1.history_retention_days, s1.undo_precheck_ttl_sec, s1.theme, s1.locale
        ),
    );
    let mut bad = custom_settings();
    bad.history_retention_days = -2;
    let c1 = code_of(maintenance::set_settings_impl(&state, bad));
    rep.check("settings.invalid_retention", c1 == "SETTINGS_INVALID", c1);
    for (name, ttl) in [("low", 29i64), ("high", 86_401i64)] {
        let mut bad = custom_settings();
        bad.undo_precheck_ttl_sec = ttl;
        let c = code_of(maintenance::set_settings_impl(&state, bad));
        rep.check(
            &format!("settings.invalid_ttl_{name}"),
            c == "SETTINGS_INVALID",
            c,
        );
    }

    // ---- ② 主任务（in_place）：完整产品链路 -------------------------------
    let job1 = format!("regress_inplace_{}", uuid::Uuid::new_v4().simple());
    let st1 = run_job(&state, &job1, &root, "in_place", None);
    let n1: i64 = scalar(&state, "SELECT total FROM jobs WHERE id=?1", &job1);
    let done1: i64 = scalar(
        &state,
        "SELECT COUNT(*) FROM entries WHERE job_id=?1 AND status='done'",
        &job1,
    );
    let avail1: i64 = scalar(
        &state,
        "SELECT undo_available FROM jobs WHERE id=?1",
        &job1,
    );
    rep.check("job1.completed", st1 == "completed", st1);
    rep.check("job1.total", n1 == total, format!("total={n1} 预计 {total}"));
    rep.check("job1.entries_done", done1 == total, format!("done={done1}"));
    rep.check("job1.undo_available", avail1 == 1, format!("{avail1}"));

    // 注入一个含双引号的 src_name（Windows 文件名不允许 `"`，只能在 DB 层），
    // 专门验证 csv_quote 的 `"` → `""` 转义。不影响 dst_path 与撤销链路。
    {
        let conn = state.db.lock();
        conn.execute(
            "UPDATE entries SET src_name=?1 WHERE id=(SELECT MIN(id) FROM entries WHERE job_id=?2)",
            params!["q\"uote,comma.dat", &job1],
        )
        .expect("注入转义样本失败");
    }

    // ---- ③ CSV 导出（父目录不存在，应自动创建） ---------------------------
    let csv1_path = PathBuf::from(&csv1);
    let exp1 = maintenance::export_report_impl(&state, &job1, &csv1)
        .expect("第一次导出失败");
    let entries1: i64 = scalar(
        &state,
        "SELECT COUNT(*) FROM entries WHERE job_id=?1",
        &job1,
    );
    rep.check(
        "csv1.rows",
        exp1.rows == entries1,
        format!("rows={} entries={}", exp1.rows, entries1),
    );
    rep.check(
        "csv1.parent_created",
        csv1_path.parent().is_some_and(|p| p.is_dir()),
        "不存在的父目录应被 create_dir_all 建出",
    );
    rep.check(
        "csv1.path_echo",
        exp1.path.as_str() == csv1_path.to_string_lossy().as_ref(),
        exp1.path,
    );
    check_csv(&mut rep, "csv1", &csv1_path, &job1, entries1);

    // ---- ④ 复核令牌写入 DB，供 verify 跨会话读取 --------------------------
    let pre1 = undo::precheck_undo(&state, &job1, noop_channel())
        .expect("seed precheck 失败");
    rep.check(
        "seed.precheck_ok",
        pre1.ok_count == pre1.total && pre1.total == total,
        format!("ok={} total={}", pre1.ok_count, pre1.total),
    );

    // ---- ⑤ 第二个任务（to_dir）：造中断现场 -------------------------------
    let job2 = format!("regress_todir_{}", uuid::Uuid::new_v4().simple());
    let st2 = run_job(&state, &job2, &root2, "to_dir", Some(outdir.clone()));
    rep.check("job2.completed_before_break", st2 == "completed", st2);
    // §7.1：开关开启时，模式 ② 的每条 done 条目都应写入 32 字符（16 字节）摘要。
    let fp_written: i64 = scalar(
        &state,
        "SELECT COUNT(*) FROM entries WHERE job_id=?1 AND status='done'
         AND dst_fingerprint IS NOT NULL AND length(dst_fingerprint)=32",
        &job2,
    );
    rep.check(
        "fingerprint.column_written",
        fp_written == total,
        format!("to_dir 完成条目中带指纹的 {fp_written}/{total}"),
    );
    {
        let conn = state.db.lock();
        conn.execute(
            "UPDATE jobs SET status='interrupted', undo_precheck_token='stale_token',
             undo_precheck_at=?1 WHERE id=?2",
            params![now_ms(), &job2],
        )
        .expect("构造中断现场失败");
    }
    let prefix = format!("__gre_{}_", &job2[..8.min(job2.len())]);
    for k in 0..INJECT_PARTS {
        fs::write(
            Path::new(&outdir).join(format!("{prefix}9{k:05}.part")),
            b"stale",
        )
        .expect("注入 .part 失败");
    }
    fs::write(Path::new(&outdir).join("other_keep.part"), b"keep")
        .expect("注入无关 .part 失败");
    let injected = count_job_parts(Path::new(&outdir), &prefix);
    rep.check(
        "job2.parts_injected",
        injected >= INJECT_PARTS,
        format!("注入 {injected} 个本任务 .part"),
    );

    let details = json!({
        "job1": job1,
        "job2": job2,
        "total": total,
        "csv1": csv1,
        "csv1Rows": exp1.rows,
        "settings": {
            "historyRetentionDays": s1.history_retention_days,
            "undoPrecheckTtlSec": s1.undo_precheck_ttl_sec,
            "undoVerifyContent": s1.undo_verify_content,
            "theme": s1.theme,
            "locale": s1.locale,
        },
        "injectedParts": injected,
    });
    println!("SEED_JSON={}", rep.finish("seed", details));
}

// ---------------------------------------------------------------------------
// 相位 2：重开 DB 后的全部断言
// ---------------------------------------------------------------------------
fn phase_verify() {
    let root = arg("root");
    let root2 = arg("root2");
    let outdir = arg("outdir");
    let db_path = arg("db");
    let csv2 = arg("csv2");
    let total: i64 = opt("total", "60").parse().expect("total 非法");
    let mut rep = Report::new();

    // 故意不复用 seed 进程的任何内存状态：重新 Db::open == 应用重启。
    let state = AppState::new(Db::open(Path::new(&db_path)).expect("打开 DB 失败"));
    state.reconcile_on_startup();

    let job1: String = {
        let conn = state.db.lock();
        conn.query_row(
            "SELECT id FROM jobs WHERE output_mode='in_place' LIMIT 1",
            [],
            |r| r.get(0),
        )
        .expect("找不到 in_place 任务")
    };
    let job2: String = {
        let conn = state.db.lock();
        conn.query_row(
            "SELECT id FROM jobs WHERE output_mode='to_dir' LIMIT 1",
            [],
            |r| r.get(0),
        )
        .expect("找不到 to_dir 任务")
    };

    // ---- ① 设置：关库重开持久化 + 原始编码 -------------------------------
    let s = maintenance::load_settings(&state).expect("读取设置失败");
    rep.check(
        "settings.persist_across_sessions",
        settings_equal(&s, &custom_settings()),
        format!(
            "{} / {} / {} / {}",
            s.history_retention_days, s.undo_precheck_ttl_sec, s.theme, s.locale
        ),
    );
    rep.check(
        "settings.fingerprint_persist_across_sessions",
        s.undo_verify_content,
        format!("跨会话读回内容指纹开关 = {}", s.undo_verify_content),
    );
    let (raw_days, raw_theme): (String, String) = {
        let conn = state.db.lock();
        (
            conn.query_row(
                "SELECT value FROM settings WHERE key='history_retention_days'",
                [],
                |r| r.get(0),
            )
            .unwrap_or_default(),
            conn.query_row("SELECT value FROM settings WHERE key='theme'", [], |r| {
                r.get(0)
            })
            .unwrap_or_default(),
        )
    };
    rep.check(
        "settings.raw_encoding",
        raw_days == "7" && raw_theme == "\"dark\"",
        format!("数值={raw_days}（裸写） 字符串={raw_theme}（JSON 引号）"),
    );

    // ---- ② 跨会话状态：终态 job 的撤销资格与令牌保留，runtime 失效 -------
    let (st1, avail1, token1): (String, i64, Option<String>) = {
        let conn = state.db.lock();
        conn.query_row(
            "SELECT status, undo_available, undo_precheck_token FROM jobs WHERE id=?1",
            params![&job1],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .expect("读取 job1 失败")
    };
    rep.check("job1.status_after_restart", st1 == "completed", st1);
    rep.check(
        "job1.undo_available_after_restart",
        avail1 == 1,
        format!("undo_available={avail1}"),
    );
    rep.check(
        "job1.token_persisted",
        token1.as_deref().is_some_and(|t| !t.is_empty()),
        format!("token={:?}", token1.as_deref().unwrap_or("NULL")),
    );
    rep.check(
        "runtime.memory_state_dropped",
        state.get_runtime(&job1).is_none() && state.get_runtime(&job2).is_none(),
        "JobRuntime 是进程内存态，重启后应为空",
    );
    let undo_checks: i64 = {
        let conn = state.db.lock();
        conn.query_row(
            "SELECT COUNT(*) FROM entries WHERE undo_check IS NOT NULL",
            [],
            |r| r.get(0),
        )
        .unwrap_or(-1)
    };
    rep.check(
        "reconcile.clears_undo_check",
        undo_checks == 0,
        format!("残留 undo_check={undo_checks}；重启后必须重新复核"),
    );

    // ---- ③ 保留期（撤销窗口上限）联动 ------------------------------------
    let created_orig: i64 = scalar(&state, "SELECT created_at FROM jobs WHERE id=?1", &job1);
    let too_old = now_ms() - 8 * DAY_MS;
    {
        let conn = state.db.lock();
        conn.execute(
            "UPDATE jobs SET created_at=?1 WHERE id=?2",
            params![too_old, &job1],
        )
        .expect("回拨 created_at 失败");
    }
    let c_ret = code_of(undo::precheck_undo(&state, &job1, noop_channel()));
    rep.check(
        "retention.undo_window_closed",
        c_ret == "UNDO_RETENTION_EXPIRED",
        format!("created_at 回拨 8 天（保留期 7 天）→ {c_ret}"),
    );
    // -1 = 永久保留：同样回拨后应放行，证明开关确实生效。
    let permanent = Settings {
        history_retention_days: -1,
        undo_precheck_ttl_sec: 30,
        undo_verify_content: true,
        theme: "dark".to_string(),
        locale: "en-US".to_string(),
        default_recursive: true,
        default_output_mode: "to_dir".to_string(),
    };
    maintenance::set_settings_impl(&state, permanent).expect("切换永久保留失败");
    let pre = match undo::precheck_undo(&state, &job1, noop_channel()) {
        Ok(p) => p,
        Err(e) => {
            println!(
                "REGRESS_JSON={}",
                rep.finish(
                    "verify",
                    json!({"fatal": format!("永久保留下 precheck 仍失败：{} {}", e.code, e.message)})
                )
            );
            return;
        }
    };
    rep.check(
        "retention.permanent_allows_undo",
        pre.ok_count == pre.total,
        format!("-1 永久保留下复核 ok={} total={}", pre.ok_count, pre.total),
    );
    let ttl_ms = pre.expires_at - now_ms();
    rep.check(
        "ttl.setting_linked",
        (25_000..=30_500).contains(&ttl_ms),
        format!("expires_at-now={ttl_ms}ms，设置为 30s"),
    );
    // 恢复：保留期 7 天 + 原始 created_at。
    maintenance::set_settings_impl(&state, custom_settings()).expect("恢复设置失败");
    {
        let conn = state.db.lock();
        conn.execute(
            "UPDATE jobs SET created_at=?1 WHERE id=?2",
            params![created_orig, &job1],
        )
        .expect("恢复 created_at 失败");
    }

    // ---- ④ 复核令牌失败路径 ----------------------------------------------
    let wrong = code_of(undo::undo_job(&state, &job1, "wrong_token", noop_channel()));
    rep.check(
        "token.mismatch_rejected",
        wrong == "UNDO_PRECHECK_REQUIRED",
        wrong,
    );
    {
        let conn = state.db.lock();
        conn.execute(
            "UPDATE jobs SET undo_precheck_at=undo_precheck_at-31000 WHERE id=?1",
            params![&job1],
        )
        .expect("回拨令牌时间失败");
    }
    let expired = code_of(undo::undo_job(&state, &job1, &pre.token, noop_channel()));
    rep.check(
        "token.expired_rejected",
        expired == "UNDO_PRECHECK_REQUIRED",
        expired,
    );
    {
        let conn = state.db.lock();
        conn.execute(
            "UPDATE jobs SET undo_precheck_at=undo_precheck_at+31000 WHERE id=?1",
            params![&job1],
        )
        .expect("恢复令牌时间失败");
    }
    {
        let conn = state.db.lock();
        conn.execute(
            "UPDATE jobs SET undo_precheck_token=NULL WHERE id=?1",
            params![&job1],
        )
        .expect("清空令牌失败");
    }
    let missing = code_of(undo::undo_job(&state, &job1, &pre.token, noop_channel()));
    rep.check(
        "token.missing_rejected",
        missing == "UNDO_PRECHECK_REQUIRED",
        missing,
    );
    {
        let conn = state.db.lock();
        conn.execute(
            "UPDATE jobs SET undo_precheck_token=?1 WHERE id=?2",
            params![&pre.token, &job1],
        )
        .expect("恢复令牌失败");
    }

    // ---- ⑤ CSV 二次导出（跨会话后仍正确） ---------------------------------
    let exp2 = maintenance::export_report_impl(&state, &job1, &csv2)
        .expect("第二次导出失败");
    let entries2: i64 = scalar(
        &state,
        "SELECT COUNT(*) FROM entries WHERE job_id=?1",
        &job1,
    );
    rep.check(
        "csv2.rows",
        exp2.rows == entries2 && entries2 == total,
        format!("rows={} entries={} total={}", exp2.rows, entries2, total),
    );
    check_csv(&mut rep, "csv2", Path::new(&csv2), &job1, entries2);

    // ---- ⑥ 跨会话撤销闭环 ------------------------------------------------
    let und = match undo::undo_job(&state, &job1, &pre.token, noop_channel()) {
        Ok(u) => u,
        Err(e) => {
            println!(
                "REGRESS_JSON={}",
                rep.finish(
                    "verify",
                    json!({"fatal": format!("跨会话撤销失败：{} {}", e.code, e.message)})
                )
            );
            return;
        }
    };
    rep.check(
        "undo.cross_session_all",
        und.undone == total && und.conflicts == 0 && und.status == "undone",
        format!(
            "undone={} conflicts={} status={}",
            und.undone, und.conflicts, und.status
        ),
    );
    let again = code_of(undo::undo_job(&state, &job1, &pre.token, noop_channel()));
    rep.check(
        "undo.duplicate_rejected",
        again == "UNDO_NOT_AVAILABLE",
        again,
    );
    let restored = count_orig_names(Path::new(&root));
    rep.check(
        "undo.files_restored",
        restored == total,
        format!("原始文件名还原 {restored}/{total}"),
    );

    // ---- ⑦ TTL 清理（prune_history） --------------------------------------
    let stale_done = format!("expired_completed_{}", uuid::Uuid::new_v4().simple());
    let stale_int = format!("expired_interrupted_{}", uuid::Uuid::new_v4().simple());
    {
        let conn = state.db.lock();
        for (id, st) in [(&stale_done, "completed"), (&stale_int, "interrupted")] {
            conn.execute(
                "INSERT INTO jobs(id,session_id,status,root_path,recursive,output_mode,total,created_at)
                 VALUES(?1,'regress',?2,'x',0,'in_place',0,?3)",
                params![id, st, now_ms() - 30 * DAY_MS],
            )
            .expect("插入过期 job 失败");
        }
    }
    let removed = maintenance::prune_history(&state).expect("prune_history 失败");
    let done_left: i64 = scalar(&state, "SELECT COUNT(*) FROM jobs WHERE id=?1", &stale_done);
    let int_left: i64 = scalar(&state, "SELECT COUNT(*) FROM jobs WHERE id=?1", &stale_int);
    rep.check(
        "prune.removes_expired_terminal",
        removed >= 1 && done_left == 0,
        format!("删除 {removed} 行，过期 completed 残留 {done_left}"),
    );
    rep.check(
        "prune.keeps_interrupted",
        int_left == 1,
        format!("interrupted 任务残留 {int_left}（应保留）"),
    );

    // ---- ⑧ resume_interrupted 收尾后仍可撤销 ------------------------------
    let prefix = format!("__gre_{}_", &job2[..8.min(job2.len())]);
    let parts_before = count_job_parts(Path::new(&outdir), &prefix);
    let planned_before: i64 = scalar(
        &state,
        "SELECT COUNT(*) FROM entries WHERE job_id=?1 AND status='planned'",
        &job2,
    );
    let rec = match maintenance::resume_interrupted_impl(&state, &job2) {
        Ok(r) => r,
        Err(e) => {
            println!(
                "REGRESS_JSON={}",
                rep.finish(
                    "verify",
                    json!({"fatal": format!("resume 失败：{} {}", e.code, e.message)})
                )
            );
            return;
        }
    };
    rep.check(
        "resume.status_completed",
        rec.status == "completed",
        rec.status.clone(),
    );
    rep.check(
        "resume.undo_available",
        rec.undo_available,
        "收尾不得剥夺撤销资格",
    );
    rep.check(
        "resume.parts_removed",
        rec.parts_removed >= INJECT_PARTS && count_job_parts(Path::new(&outdir), &prefix) == 0,
        format!(
            "清理 {} 个（注入 {}，清理前 {}），残留 {}",
            rec.parts_removed,
            INJECT_PARTS,
            parts_before,
            count_job_parts(Path::new(&outdir), &prefix)
        ),
    );
    rep.check(
        "resume.stray_kept",
        Path::new(&outdir).join("other_keep.part").exists(),
        "无关前缀的 .part 不应被误删",
    );
    rep.check(
        "resume.entry_conservation",
        rec.reconciled_done + rec.marked_skipped + rec.needs_review == planned_before
            && rec.needs_review == 0,
        format!(
            "done={} skipped={} review={} / 收尾前 planned={}",
            rec.reconciled_done, rec.marked_skipped, rec.needs_review, planned_before
        ),
    );
    let token_cleared: bool = {
        let conn = state.db.lock();
        conn.query_row(
            "SELECT undo_precheck_token IS NULL FROM jobs WHERE id=?1",
            params![&job2],
            |r| r.get(0),
        )
        .unwrap_or(false)
    };
    rep.check(
        "resume.old_token_invalidated",
        token_cleared,
        "收尾必须清空旧复核令牌",
    );
    let stale = code_of(undo::undo_job(&state, &job2, "stale_token", noop_channel()));
    rep.check(
        "resume.stale_token_rejected",
        stale == "UNDO_PRECHECK_REQUIRED",
        stale,
    );
    let pre2 = match undo::precheck_undo(&state, &job2, noop_channel()) {
        Ok(p) => p,
        Err(e) => {
            println!(
                "REGRESS_JSON={}",
                rep.finish(
                    "verify",
                    json!({"fatal": format!("收尾后重新复核失败：{} {}", e.code, e.message)})
                )
            );
            return;
        }
    };
    let und2 = match undo::undo_job(&state, &job2, &pre2.token, noop_channel()) {
        Ok(u) => u,
        Err(e) => {
            println!(
                "REGRESS_JSON={}",
                rep.finish(
                    "verify",
                    json!({"fatal": format!("收尾后撤销失败：{} {}", e.code, e.message)})
                )
            );
            return;
        }
    };
    rep.check(
        "resume.undo_after_recovery",
        und2.undone == total && und2.conflicts == 0,
        format!(
            "收尾后撤销 undone={} conflicts={} status={}",
            und2.undone, und2.conflicts, und2.status
        ),
    );
    let out_left = count_files(Path::new(&outdir));
    rep.check(
        "resume.output_clean",
        out_left == 1,
        format!("输出目录仅剩 other_keep.part，实际 {out_left} 个文件"),
    );
    rep.check(
        "resume.sources_untouched",
        count_files(Path::new(&root2)) == total,
        format!("to_dir 源文件应保持只读，实际 {} 个", count_files(Path::new(&root2))),
    );

    // ---- ⑨ 内容指纹开关的判别力（§7.1）---------------------------------
    // 独立构造只有一个条目的 to_dir 任务，不干扰上面已完成的断言。
    // 指纹一致 → 放行；只把 DB 里的指纹改错（文件、size、mtime 全不动）
    // → 开关开启时应判冲突；关掉开关 → 回退 size+mtime，重新放行。
    // 三条合起来证明开关确实改变了撤销判据，而非恒真断言。
    let fp_job = format!("regress_fp_{}", uuid::Uuid::new_v4().simple());
    let probe = Path::new(&outdir).join("fp_probe.bin");
    fs::write(&probe, b"grename fingerprint probe").expect("写入指纹探针失败");
    let (probe_size, probe_mtime) =
        execute::stat_identity(&probe).expect("读取探针身份失败");
    let good_fp = execute::fingerprint(&probe).expect("计算探针指纹失败");
    let probe_str = probe.to_string_lossy().to_string();
    {
        let conn = state.db.lock();
        conn.execute(
            "INSERT INTO jobs(id,session_id,status,root_path,recursive,output_mode,
             total,done,undo_available,created_at)
             VALUES(?1,'regress','completed','x',0,'to_dir',1,1,1,?2)",
            params![fp_job, now_ms()],
        )
        .expect("插入指纹探针任务失败");
        conn.execute(
            "INSERT INTO entries(job_id,seq,src_path,src_name,src_size,rel_dir,
             dst_path,dst_name,dst_key,dst_size,dst_mtime_ns,dst_fingerprint,status)
             VALUES(?1,1,'__no_such_source__','src.bin',?2,'',?3,'fp_probe.bin','',?4,?5,?6,'done')",
            params![fp_job, probe_size, probe_str, probe_size, probe_mtime, good_fp],
        )
        .expect("插入指纹探针条目失败");
    }
    let fp_ok = undo::precheck_undo(&state, &fp_job, noop_channel())
        .expect("指纹一致时复核不应失败");
    rep.check(
        "fingerprint.match_allows_undo",
        fp_ok.ok_count == 1 && fp_ok.conflicts.is_empty(),
        format!("ok={} conflicts={:?}", fp_ok.ok_count, fp_ok.conflicts),
    );
    {
        let conn = state.db.lock();
        conn.execute(
            "UPDATE entries SET dst_fingerprint='00000000000000000000000000000000' WHERE job_id=?1",
            params![fp_job],
        )
        .expect("篡改指纹失败");
    }
    let fp_bad = undo::precheck_undo(&state, &fp_job, noop_channel())
        .expect("指纹不一致时应仍返回复核结果");
    rep.check(
        "fingerprint.mismatch_blocks_undo",
        fp_bad.ok_count == 0 && !fp_bad.conflicts.is_empty(),
        format!("ok={} conflicts={:?}", fp_bad.ok_count, fp_bad.conflicts),
    );
    let mut off = maintenance::load_settings(&state).expect("读取设置失败");
    off.undo_verify_content = false;
    maintenance::set_settings_impl(&state, off).expect("关闭指纹开关失败");
    let fp_off = undo::precheck_undo(&state, &fp_job, noop_channel())
        .expect("关闭开关后复核不应失败");
    rep.check(
        "fingerprint.off_falls_back_to_size_mtime",
        fp_off.ok_count == 1 && fp_off.conflicts.is_empty(),
        format!("ok={} conflicts={:?}", fp_off.ok_count, fp_off.conflicts),
    );
    let mut on = maintenance::load_settings(&state).expect("读取设置失败");
    on.undo_verify_content = true;
    maintenance::set_settings_impl(&state, on).expect("恢复指纹开关失败");

    let details = json!({
        "job1": job1,
        "job2": job2,
        "total": total,
        "csv2": csv2,
        "csv2Rows": exp2.rows,
        "undo": { "undone": und.undone, "conflicts": und.conflicts, "status": und.status },
        "resume": {
            "partsRemoved": rec.parts_removed,
            "reconciledDone": rec.reconciled_done,
            "markedSkipped": rec.marked_skipped,
            "needsReview": rec.needs_review,
            "undoAvailable": rec.undo_available,
            "undone": und2.undone,
        },
        "pruneRemoved": removed,
        "fingerprintProbe": {
            "matchOk": fp_ok.ok_count,
            "mismatchOk": fp_bad.ok_count,
            "offOk": fp_off.ok_count,
            "mismatchConflicts": fp_bad.conflicts.len(),
        },
    });
    println!("REGRESS_JSON={}", rep.finish("verify", details));
}

fn main() {
    match arg("phase").as_str() {
        "seed" => phase_seed(),
        "verify" => phase_verify(),
        other => panic!("未知 phase：{other}"),
    }
}
