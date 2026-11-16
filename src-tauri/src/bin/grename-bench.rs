//! Grename M0 阶梯基准采集（独立 bin，**不进产品运行逻辑**）。
//!
//! 设计文档 v1.5 §12.1 第 11 项。作用见 `done/tools/run_bench.py` 顶部说明。
//!
//! 关键点：用 `Channel::new` 在无 GUI / 无 WebView 环境构造进度通道，
//! 直接驱动产品代码里的 `scan → build_plan → start_execute → precheck_undo` 真实链路，
//! 因此测得的是**产品后端本身**的性能，不是另写的一套模拟实现。
//!
//! 采集口径：
//!   - 分阶段耗时与吞吐（扫描、计划、执行、撤销复核），耗时含后台线程收敛完成；
//!   - 进度事件总数与相邻间隔 p50/p95/max —— 验证「事件数 = O(运行时长) 而非 O(N)」；
//!   - 进程工作集 RSS 峰值 / 相对基线增量 —— 验证内存非 O(N)；
//!   - 冲突数、失败数、跳过数、字节吞吐。
//!
//! 输出：stdout 单行 `BENCH,<csv...>`，由驱动脚本汇总。诊断信息一律走 stderr。

use std::env;
use std::ffi::c_void;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rusqlite::params;
use tauri::ipc::Channel;
use tauri_app_lib::db::Db;
use tauri_app_lib::model::Progress;
use tauri_app_lib::rule::RuleSpec;
use tauri_app_lib::state::AppState;
use tauri_app_lib::{execute, plan, scan, undo};

const MB: f64 = 1024.0 * 1024.0;

// ---------------------------------------------------------------------------
// Windows 进程工作集（零新增 crate 依赖，kernel32 由标准库默认链接）
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Clone, Copy)]
struct ProcessMemoryCounters {
    cb: u32,
    page_fault_count: u32,
    peak_working_set_size: usize,
    working_set_size: usize,
    quota_peak_paged_pool_usage: usize,
    quota_paged_pool_usage: usize,
    quota_peak_non_paged_pool_usage: usize,
    quota_non_paged_pool_usage: usize,
    pagefile_usage: usize,
    peak_pagefile_usage: usize,
}

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetCurrentProcess() -> *mut c_void;
    fn K32GetProcessMemoryInfo(
        process: *mut c_void,
        counters: *mut ProcessMemoryCounters,
        size: u32,
    ) -> i32;
}

fn rss_bytes() -> u64 {
    #[cfg(windows)]
    unsafe {
        let mut c: ProcessMemoryCounters = std::mem::zeroed();
        c.cb = std::mem::size_of::<ProcessMemoryCounters>() as u32;
        if K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) != 0 {
            return c.working_set_size as u64;
        }
    }
    0
}

// ---------------------------------------------------------------------------
// 进度探针：每个 Channel 回调 = 一次聚合进度事件（产品里固定 80ms 一轮）
// ---------------------------------------------------------------------------

/// 执行期进度事件的结构化样本：用于 sized 补充压测验证「按字节推进」与 ETA 精度。
/// 常规五档压测不落盘，只在传入 --eta-out 时写出，CSV 38 列协议不变。
#[derive(serde::Serialize)]
struct EtaSample {
    bytes_done: u64,
    bytes_total: u64,
    eta_ms: u64,
    elapsed_ms: u64,
    done: u64,
    total: u64,
}

#[derive(Default)]
struct Probe {
    events: u64,
    last: Option<Instant>,
    intervals_ms: Vec<f64>,
    rss_peak: u64,
    rss_samples: Vec<u64>,
    baseline: u64,
    eta_samples: Vec<EtaSample>,
}

type SharedProbe = Arc<Mutex<Probe>>;

fn probe_channel(probe: &SharedProbe) -> Channel<Progress> {
    let p = probe.clone();
    Channel::new(move |body| -> tauri::Result<()> {
        // Channel 发送的是产品 Progress 的 JSON；解析失败的兜底不能影响计数。
        let v: serde_json::Value = body.deserialize().unwrap_or(serde_json::Value::Null);
        let mut g = p.lock().expect("probe mutex poisoned");
        let now = Instant::now();
        g.events += 1;
        if let Some(last) = g.last {
            g.intervals_ms
                .push(now.duration_since(last).as_secs_f64() * 1000.0);
        }
        g.last = Some(now);
        let rss = rss_bytes();
        if rss > 0 {
            g.rss_samples.push(rss);
            if rss > g.rss_peak {
                g.rss_peak = rss;
            }
        }
        // 上限 512 条，内存恒定；只取执行阶段，避免扫描 / 撤销样本混入。
        if g.eta_samples.len() < 512 {
            if let Some(o) = v.as_object() {
                if o.get("phase").and_then(|x| x.as_str()) == Some("executing") {
                    let u = |k: &str| o.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
                    g.eta_samples.push(EtaSample {
                        bytes_done: u("bytesDone"),
                        bytes_total: u("bytesTotal"),
                        eta_ms: u("etaMs"),
                        elapsed_ms: u("elapsedMs"),
                        done: u("done"),
                        total: u("total"),
                    });
                }
            }
        }
        Ok(())
    })
}

/// 阶段切换时断开时间基准，避免把「阶段之间的空档」算成一次进度延迟。
fn mark_phase(probe: &SharedProbe) {
    probe.lock().expect("probe mutex poisoned").last = None;
}

fn percentile(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let i = ((sorted.len() - 1) as f64 * q).round() as usize;
    sorted[i.min(sorted.len() - 1)]
}

// ---------------------------------------------------------------------------
// 参数与等待
// ---------------------------------------------------------------------------

fn arg(name: &str) -> String {
    let key = format!("--{name}");
    let args: Vec<String> = env::args().collect();
    for i in 0..args.len().saturating_sub(1) {
        if args[i] == key {
            return args[i + 1].clone();
        }
    }
    panic!("缺少参数 {key}");
}

/// 可选参数：缺失时返回 None（不 panic）。
fn opt_arg(name: &str) -> Option<String> {
    let key = format!("--{name}");
    let args: Vec<String> = env::args().collect();
    (0..args.len().saturating_sub(1))
        .find(|&i| args[i] == key)
        .map(|i| args[i + 1].clone())
}

/// 后台任务以 `clear_runtime` 作为收尾信号；同时确认 jobs.status 已离开运行态。
fn wait_done(state: &AppState, job_id: &str, timeout: Duration) -> Result<String, String> {
    let t0 = Instant::now();
    loop {
        let status: String = {
            let conn = state.db.lock();
            conn.query_row(
                "SELECT status FROM jobs WHERE id=?1",
                params![job_id],
                |r| r.get(0),
            )
            .unwrap_or_else(|_| "unknown".to_string())
        };
        let running = state.get_runtime(job_id).is_some();
        let in_flight = matches!(
            status.as_str(),
            "scanning" | "executing" | "undo_prechecking" | "undoing"
        );
        if !running && !in_flight {
            return Ok(status);
        }
        if t0.elapsed() > timeout {
            return Err(format!("timeout(status={status})"));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

type JobStats = (i64, i64, i64, i64, i64);

fn job_stats(state: &AppState, job_id: &str) -> JobStats {
    let conn = state.db.lock();
    conn.query_row(
        "SELECT total, done, failed, skipped, bytes_total FROM jobs WHERE id=?1",
        params![job_id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
    )
    .unwrap_or((0, 0, 0, 0, 0))
}

fn f(v: f64) -> String {
    format!("{v:.2}")
}

fn main() {
    let root = arg("root");
    let mode = arg("mode");
    let out_dir = arg("outdir");
    let db_path = arg("db");
    let tag = arg("tag");
    let tier: u64 = arg("tier").parse().expect("tier 必须是整数");
    let round: u32 = arg("round").parse().expect("round 必须是整数");

    if !Path::new(&root).is_dir() {
        panic!("样本目录不存在：{root}");
    }
    if mode != "in_place" && mode != "to_dir" {
        panic!("mode 只能是 in_place / to_dir");
    }

    let db_file = PathBuf::from(&db_path);
    if let Some(parent) = db_file.parent() {
        fs::create_dir_all(parent).expect("创建 db 目录失败");
    }
    // 每轮独立 DB：避免历史任务堆积让 SQLite 体积与索引影响后续档位。
    if db_file.exists() {
        fs::remove_file(&db_file).expect("清理旧 db 失败");
    }

    let probe: SharedProbe = Arc::new(Mutex::new(Probe::default()));
    probe.lock().expect("probe mutex poisoned").baseline = rss_bytes();

    let state = AppState::new(Db::open(&db_file).expect("打开 db 失败"));
    state.reconcile_on_startup();

    // sized 补充压测专用：把默认关闭的内容指纹开关写入本轮独立 DB，
    // 量化模式 ② 产物额外读一次盘的开销。**CSV 38 列协议不变**，
    // 开关状态由驱动编码进 tag；常规五档压测不带此 flag，行为逐字节不变。
    let verify_content = env::args().any(|a| a == "--verify-content");
    if verify_content {
        let mut s = tauri_app_lib::maintenance::load_settings(&state)
            .expect("读取默认设置失败");
        s.undo_verify_content = true;
        tauri_app_lib::maintenance::set_settings_impl(&state, s)
            .expect("写入 undo_verify_content 失败");
    }
    let job_id = format!("bench_{}", uuid::Uuid::new_v4().simple());

    // ---- ① 扫描 -----------------------------------------------------------
    mark_phase(&probe);
    let t_scan = Instant::now();
    let scan_res = scan::start_scan(
        state.clone(),
        job_id.clone(),
        root.clone(),
        true, // 样本分布在 d00000/ 等子目录，必须递归
        None,
        probe_channel(&probe),
    )
    .expect("start_scan 失败");
    let scan_status = wait_done(&state, &scan_res.job_id, Duration::from_secs(1800))
        .expect("扫描等待失败");
    let scan_ms = t_scan.elapsed().as_secs_f64() * 1000.0;
    let scan_events = probe.lock().expect("probe mutex poisoned").events;
    let (total, _, _, _, _) = job_stats(&state, &job_id);

    // ---- ② 生成计划 -------------------------------------------------------
    let rule = RuleSpec {
        // pattern 留空 = `^.*$` 整段替换；`{n}` 用扫描序号保证目标名全局唯一，
        // 不产生冲突，使 planned == total，吞吐统计才有可比性。
        replacement: "b_{n}".to_string(),
        ..RuleSpec::default()
    };
    let t_plan = Instant::now();
    let ps = plan::build_plan(
        &state,
        &job_id,
        &mode,
        if mode == "to_dir" {
            Some(out_dir.clone())
        } else {
            None
        },
        rule,
    )
    .expect("build_plan 失败");
    let plan_ms = t_plan.elapsed().as_secs_f64() * 1000.0;

    // ---- ③ 执行 -----------------------------------------------------------
    mark_phase(&probe);
    let t_exec = Instant::now();
    execute::start_execute(state.clone(), job_id.clone(), probe_channel(&probe))
        .expect("start_execute 失败");
    let exec_status =
        wait_done(&state, &job_id, Duration::from_secs(3600)).expect("执行等待失败");
    let exec_ms = t_exec.elapsed().as_secs_f64() * 1000.0;
    let exec_events = probe.lock().expect("probe mutex poisoned").events - scan_events;
    let (_, exec_done, exec_failed, _, bytes_total) = job_stats(&state, &job_id);

    // ---- ④⑤ 撤销复核 + 撤销执行 --------------------------------------------
    // 单轮档位不需要还原样本；`--skip-undo` 时只测主吞吐链路，
    // 避免 to_dir 删除产物在本机实时防护下出现异常慢的尾部耗时。
    // 小档仍跑完整链路，用于验证 §6.6 三段式与「零误删零覆盖」。
    let skip_undo = env::args().any(|a| a == "--skip-undo");
    let pre_events_before = probe.lock().expect("probe mutex poisoned").events;
    let (mut pre_ms, mut pre_ok, mut pre_conflict) = (0.0, 0i64, 0i64);
    let (mut pre_err, mut token) = (String::new(), String::new());
    let (mut undo_ms, mut undo_undone, mut undo_conflicts) = (0.0, 0i64, 0i64);
    let (mut undo_status, mut undo_err) = ("skipped".to_string(), String::new());
    let mut undo_events_before = pre_events_before;

    if !skip_undo {
        // ④ 全量状态复核（rayon 并行 stat + 令牌）。
        mark_phase(&probe);
        let t_pre = Instant::now();
        let pre_res = undo::precheck_undo(&state, &job_id, probe_channel(&probe));
        pre_ms = t_pre.elapsed().as_secs_f64() * 1000.0;
        match pre_res {
            Ok(r) => {
                pre_ok = r.ok_count;
                pre_conflict = r.conflicts.iter().map(|c| c.count).sum::<i64>();
                token = r.token;
            }
            Err(e) => pre_err = e.code,
        }

        // ⑤ 持令牌执行撤销：闭环还原样本，下一轮条件完全一致。
        mark_phase(&probe);
        undo_events_before = probe.lock().expect("probe mutex poisoned").events;
        if !token.is_empty() {
            let t_undo = Instant::now();
            match undo::undo_job(&state, &job_id, &token, probe_channel(&probe)) {
                Ok(r) => {
                    undo_undone = r.undone;
                    undo_conflicts = r.conflicts;
                    undo_status = r.status;
                }
                Err(e) => undo_err = e.code,
            }
            undo_ms = t_undo.elapsed().as_secs_f64() * 1000.0;
        } else {
            undo_err = "NO_TOKEN".to_string();
        }
    }

    // emitter 80ms 一轮，留一点余量收最后一帧后统一取快照。
    std::thread::sleep(Duration::from_millis(120));
    let g = probe.lock().expect("probe mutex poisoned");
    let events = g.events;
    let mut iv = g.intervals_ms.clone();
    let peak_mb = g.rss_peak as f64 / MB;
    let delta_mb = (g.rss_peak.saturating_sub(g.baseline)) as f64 / MB;
    // sized 补充压测：把执行期进度样本单独落 JSON，避免污染 38 列 CSV 与 stderr。
    if let Some(eta_out) = opt_arg("eta-out") {
        let payload = serde_json::json!({
            "execMs": exec_ms,
            "samples": g.eta_samples,
        });
        if let Some(parent) = Path::new(&eta_out).parent() {
            fs::create_dir_all(parent).ok();
        }
        fs::write(&eta_out, serde_json::to_string(&payload).unwrap_or_default())
            .expect("写 ETA 样本失败");
    }
    drop(g);
    let precheck_events = undo_events_before.saturating_sub(pre_events_before);
    let undo_events = events.saturating_sub(undo_events_before);
    iv.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let p50 = percentile(&iv, 0.50);
    let p95 = percentile(&iv, 0.95);
    let pmax = iv.last().copied().unwrap_or(0.0);

    let scan_thr = if scan_ms > 0.0 {
        total as f64 / (scan_ms / 1000.0)
    } else {
        0.0
    };
    let exec_thr = if exec_ms > 0.0 {
        exec_done as f64 / (exec_ms / 1000.0)
    } else {
        0.0
    };
    let bytes_per_sec = if exec_ms > 0.0 {
        bytes_total as f64 / (exec_ms / 1000.0)
    } else {
        0.0
    };

    eprintln!(
        "[bench] tag={tag} tier={tier} mode={mode} round={round} total={total} \
         verify_content={verify_content} \
         scan={}ms plan={}ms exec={}ms precheck={}ms undo={}ms({undo_status}) \
         status={scan_status}/{exec_status}",
        f(scan_ms),
        f(plan_ms),
        f(exec_ms),
        f(pre_ms),
        f(undo_ms),
    );

    // 列顺序须与 run_bench.py 的 CSV_HEADER 严格一致（38 列）。
    let fields: Vec<String> = vec![
        "BENCH".into(),
        tag,
        tier.to_string(),
        mode,
        round.to_string(),
        total.to_string(),
        f(scan_ms),
        f(scan_thr),
        f(plan_ms),
        ps.planned.to_string(),
        ps.conflicted.to_string(),
        f(exec_ms),
        exec_done.to_string(),
        exec_failed.to_string(),
        f(exec_thr),
        bytes_total.to_string(),
        f(bytes_per_sec),
        f(pre_ms),
        pre_ok.to_string(),
        pre_conflict.to_string(),
        pre_err,
        f(undo_ms),
        undo_undone.to_string(),
        undo_conflicts.to_string(),
        undo_status,
        undo_err,
        f(peak_mb),
        f(delta_mb),
        events.to_string(),
        scan_events.to_string(),
        exec_events.to_string(),
        precheck_events.to_string(),
        undo_events.to_string(),
        f(p50),
        f(p95),
        f(pmax),
        scan_status,
        exec_status,
    ];
    assert_eq!(fields.len(), 38, "CSV 列数与驱动脚本不一致");
    println!("{}", fields.join(","));
}
