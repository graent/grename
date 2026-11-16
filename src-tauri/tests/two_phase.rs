//! M4-2 行为验证：链式 / 交换 / 仅改大小写的两阶段临时名。
//!
//! 直接驱动产品同一份代码（`plan::build_plan` → `execute::run_execute` →
//! `maintenance::resume_interrupted_impl`），不另写模拟实现。这是静态核查
//! 无法替代的部分：它真正在临时目录里造文件、真的改名、真的检查残留。

use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::params;
use tauri_app_lib::db::Db;
use tauri_app_lib::execute::run_execute;
use tauri_app_lib::maintenance::resume_interrupted_impl;
use tauri_app_lib::plan::build_plan;
use tauri_app_lib::rule::RuleSpec;
use tauri_app_lib::stage;
use tauri_app_lib::state::{AppState, JobRuntime};

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "grename-m4b-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("create fixture dir");
        Self { root }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    fn write(&self, name: &str, body: &str) -> PathBuf {
        let p = self.path(name);
        fs::write(&p, body).expect("write fixture file");
        p
    }

    fn state(&self) -> AppState {
        AppState::new(Db::open(&self.path("grename.db")).expect("open fixture db"))
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// 只插库，不碰文件——让每个用例自己决定磁盘上留下什么状态。
fn seed(state: &AppState, job_id: &str, root: &Path, files: &[(&str, i64)]) {
    let conn = state.db.lock();
    conn.execute(
        "INSERT INTO jobs (id, session_id, status, root_path, recursive, output_mode,
                           output_dir, total, bytes_total, created_at)
         VALUES (?1, 'test', 'scanned', ?2, 0, 'in_place', NULL, ?3, 0, 0)",
        params![job_id, root.to_string_lossy().to_string(), files.len() as i64],
    )
    .expect("insert job");
    for (name, seq) in files {
        let p = root.join(name);
        // 必须用真实字节数：执行器会拿它做 TOCTOU 校验，写 0 会让每条都判 NOT_FOUND。
        let size = fs::metadata(&p).map(|m| m.len() as i64).unwrap_or(0);
        conn.execute(
            "INSERT INTO entries (job_id, seq, src_path, src_name, src_size, rel_dir, status)
             VALUES (?1, ?2, ?3, ?4, ?5, '', 'scanned')",
            params![job_id, seq, p.to_string_lossy().to_string(), name, size],
        )
        .expect("insert entry");
    }
}

fn tmp_of(state: &AppState, job_id: &str, src_name: &str) -> Option<String> {
    let conn = state.db.lock();
    conn.query_row(
        "SELECT tmp_path FROM entries WHERE job_id=?1 AND src_name=?2",
        params![job_id, src_name],
        |r| r.get(0),
    )
    .expect("query tmp_path")
}

fn status_of(state: &AppState, job_id: &str, src_name: &str) -> (String, Option<String>) {
    let conn = state.db.lock();
    conn.query_row(
        "SELECT status, err_code FROM entries WHERE job_id=?1 AND src_name=?2",
        params![job_id, src_name],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .expect("query status")
}

/// 任何 `__gre_` 前缀的残留都算失败：系统承诺「不留临时名」。
fn assert_no_residue(root: &Path) {
    for e in fs::read_dir(root).expect("read fixture dir") {
        let name = e.expect("dir entry").file_name().to_string_lossy().to_string();
        assert!(!name.starts_with("__gre_"), "残留临时文件：{name}");
    }
}

/// 全链路：`1.txt→0.txt` 且 `2.txt→1.txt`，`1.txt` 的位置正被 `2.txt` 需要。
/// 计划期必须只把 `1.txt` 标成挡路者，执行后两条都到位。
#[test]
fn plan_marks_blocker_then_execute_shifts_chain() {
    let fx = Fixture::new("chain");
    fx.write("1.txt", "one");
    fx.write("2.txt", "two");
    let state = fx.state();
    seed(&state, "job-chain", &fx.root, &[("1.txt", 1), ("2.txt", 2)]);

    // n = seq_start + seq → 1.txt 得 "0"、2.txt 得 "1"。
    let rule = RuleSpec {
        pattern: r"^\d$".to_string(),
        replacement: "{n}".to_string(),
        seq_start: -1,
        ..Default::default()
    };
    let summary = build_plan(&state, "job-chain", "in_place", None, rule).expect("build plan");
    assert_eq!(summary.planned, 2, "两条都应待执行");
    assert_eq!(summary.conflicted, 0, "目标名不同，不构成重名冲突");

    assert!(
        tmp_of(&state, "job-chain", "1.txt").is_some(),
        "1.txt 的目标位被 2.txt 需要，应标为挡路者"
    );
    assert!(
        tmp_of(&state, "job-chain", "2.txt").is_none(),
        "2.txt 的源名不在任何目标位上，不该多付一次 rename"
    );

    let rt = JobRuntime::new();
    run_execute(&state, "job-chain", "in_place", &rt, false).expect("run execute");

    assert_eq!(fs::read_to_string(fx.path("0.txt")).expect("0.txt"), "one");
    assert_eq!(fs::read_to_string(fx.path("1.txt")).expect("1.txt"), "two");
    assert!(!fx.path("2.txt").exists(), "2.txt 应已让位");
    assert_no_residue(&fx.root);
}

/// 交换：两条互为对方目标。计划期由 `build_plan` 算不出来（规则是单射），
/// 这里手工写入互为目标的计划，验证**执行器**在两条都挡路时能正确交换。
#[test]
fn execute_swaps_two_files() {
    let fx = Fixture::new("swap");
    let a = fx.write("a.txt", "A");
    let b = fx.write("b.txt", "B");
    let state = fx.state();
    seed(&state, "job-swap", &fx.root, &[("a.txt", 1), ("b.txt", 2)]);

    {
        let conn = state.db.lock();
        let set = "UPDATE entries SET status='planned', dst_name=?1, dst_path=?2,
                   dst_key=?3, tmp_path=?4 WHERE job_id='job-swap' AND src_name=?5";
        conn.execute(
            set,
            params![
                "b.txt",
                b.to_string_lossy().to_string(),
                "b.txt",
                stage::tmp_path_for(&a, "job-swap", 1).to_string_lossy().to_string(),
                "a.txt"
            ],
        )
        .expect("plan a->b");
        conn.execute(
            set,
            params![
                "a.txt",
                a.to_string_lossy().to_string(),
                "a.txt",
                stage::tmp_path_for(&b, "job-swap", 2).to_string_lossy().to_string(),
                "b.txt"
            ],
        )
        .expect("plan b->a");
    }

    let rt = JobRuntime::new();
    run_execute(&state, "job-swap", "in_place", &rt, false).expect("run execute");

    assert_eq!(fs::read_to_string(fx.path("a.txt")).expect("a.txt"), "B");
    assert_eq!(fs::read_to_string(fx.path("b.txt")).expect("b.txt"), "A");
    assert_no_residue(&fx.root);
}

/// 普通加前缀改名：没有任何条目挡路，`tmp_path` 必须全空——
/// 这是「大多数任务完全走原路径、M0 基线不受影响」的直接证据。
#[test]
fn plain_rename_marks_no_blocker() {
    let fx = Fixture::new("plain");
    fx.write("1.txt", "one");
    fx.write("2.txt", "two");
    let state = fx.state();
    seed(&state, "job-plain", &fx.root, &[("1.txt", 1), ("2.txt", 2)]);

    let rule = RuleSpec {
        pattern: "^".to_string(),
        replacement: "pre_".to_string(),
        ..Default::default()
    };
    build_plan(&state, "job-plain", "in_place", None, rule).expect("build plan");

    assert!(tmp_of(&state, "job-plain", "1.txt").is_none());
    assert!(tmp_of(&state, "job-plain", "2.txt").is_none());

    let rt = JobRuntime::new();
    run_execute(&state, "job-plain", "in_place", &rt, false).expect("run execute");

    assert_eq!(fs::read_to_string(fx.path("pre_1.txt")).expect("pre_1"), "one");
    assert_eq!(fs::read_to_string(fx.path("pre_2.txt")).expect("pre_2"), "two");
    assert_no_residue(&fx.root);
}

/// 阶段 1 之后、阶段 2 之前崩溃：文件只在临时名上。
/// 收尾必须还原回源名——安全优先，回到执行前而不是猜该不该落终名。
#[test]
fn resume_restores_file_stuck_on_tmp() {
    let fx = Fixture::new("resume-tmp");
    let a = fx.path("a.txt");
    let tmp = stage::tmp_path_for(&a, "job-resume", 1);
    fs::write(&tmp, "A").expect("write tmp");
    assert!(!a.exists(), "造场景：源名已让位");

    let state = fx.state();
    seed(&state, "job-resume", &fx.root, &[("a.txt", 1)]);
    {
        let conn = state.db.lock();
        conn.execute(
            "UPDATE entries SET status='planned', dst_name='b.txt', dst_path=?1,
             dst_key='b.txt', tmp_path=?2 WHERE job_id='job-resume'",
            params![
                fx.path("b.txt").to_string_lossy().to_string(),
                tmp.to_string_lossy().to_string()
            ],
        )
        .expect("mark planned");
        conn.execute(
            "UPDATE jobs SET status='interrupted' WHERE id='job-resume'",
            [],
        )
        .expect("mark interrupted");
    }

    let r = resume_interrupted_impl(&state, "job-resume").expect("resume");
    assert!(a.exists(), "临时名应还原回源名");
    assert!(!tmp.exists(), "临时名应已消失");
    assert_eq!(r.marked_skipped, 1);
    assert_eq!(r.needs_review, 0);
    let (st, code) = status_of(&state, "job-resume", "a.txt");
    assert_eq!(st, "skipped");
    assert_eq!(code.as_deref(), Some("INTERRUPTED"));
    assert_no_residue(&fx.root);
}

/// 阶段 2 已成功、批量状态没回写就崩：终名在位、临时名已消失。
/// 收尾应补记 done 并保留撤销资格，而不是判成冲突。
#[test]
fn resume_marks_done_when_target_present() {
    let fx = Fixture::new("resume-done");
    fx.write("b.txt", "A");
    let state = fx.state();
    seed(&state, "job-done", &fx.root, &[("a.txt", 1)]);
    {
        let conn = state.db.lock();
        conn.execute(
            "UPDATE entries SET status='planned', dst_name='b.txt', dst_path=?1,
             dst_key='b.txt' WHERE job_id='job-done'",
            params![fx.path("b.txt").to_string_lossy().to_string()],
        )
        .expect("mark planned");
        conn.execute("UPDATE jobs SET status='interrupted' WHERE id='job-done'", [])
            .expect("mark interrupted");
    }

    let r = resume_interrupted_impl(&state, "job-done").expect("resume");
    assert_eq!(r.reconciled_done, 1);
    assert!(r.undo_available, "补记 done 后仍应保留撤销资格");
    let (st, _) = status_of(&state, "job-done", "a.txt");
    assert_eq!(st, "done");
}
