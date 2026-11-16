//! SQLite 单写连接 + M0 最小 DDL。
//! 设计文档 §7。M0 只建 jobs / entries，settings / templates 留到 M3。
//! 注意：v1.3 起 jobs.undo_precheck_* 与 entries.dst_mtime_ns / undo_check 已纳入，
//! 用于跨会话撤销的「全量复核 + 短期令牌」。

use std::fs;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use rusqlite::Connection;

use crate::model::AppResult;

const SCHEMA: &str = r#"
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA foreign_keys = ON;
PRAGMA busy_timeout = 5000;
PRAGMA temp_store = MEMORY;
PRAGMA cache_size = -65536;

-- M3 设置（§7.1）：value 为 JSON 编码，逐键 upsert，旧库无需迁移。
CREATE TABLE IF NOT EXISTS settings (
  key         TEXT PRIMARY KEY,
  value       TEXT NOT NULL,
  updated_at  INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS jobs (
  id                  TEXT PRIMARY KEY,
  session_id          TEXT NOT NULL,          -- v1.3: 仅审计归属，不再是撤销边界
  status              TEXT NOT NULL,
  root_path           TEXT NOT NULL,
  recursive           INTEGER NOT NULL DEFAULT 0,
  output_mode         TEXT NOT NULL DEFAULT 'in_place',
  output_dir          TEXT,
  total               INTEGER NOT NULL DEFAULT 0,
  bytes_total         INTEGER NOT NULL DEFAULT 0,
  done                INTEGER NOT NULL DEFAULT 0,
  bytes_done          INTEGER NOT NULL DEFAULT 0,
  failed              INTEGER NOT NULL DEFAULT 0,
  skipped             INTEGER NOT NULL DEFAULT 0,
  conflicted          INTEGER NOT NULL DEFAULT 0,
  undo_available      INTEGER NOT NULL DEFAULT 0,  -- v1.3: 跨重启保留
  undo_precheck_at    INTEGER,
  undo_precheck_token TEXT,
  rule_json           TEXT,                   -- M1：规则快照，历史任务可解释性
  created_at          INTEGER NOT NULL,
  finished_at         INTEGER
);
CREATE INDEX IF NOT EXISTS idx_jobs_created ON jobs(created_at);
CREATE INDEX IF NOT EXISTS idx_jobs_status  ON jobs(status);

CREATE TABLE IF NOT EXISTS entries (
  id              INTEGER PRIMARY KEY,        -- 单调递增，兼作分页游标
  job_id          TEXT NOT NULL,
  seq             INTEGER NOT NULL,
  src_path        TEXT NOT NULL,
  src_name        TEXT NOT NULL,
  src_size        INTEGER NOT NULL DEFAULT 0,
  rel_dir         TEXT NOT NULL DEFAULT '',   -- 相对扫描根的父目录（模式② 重建结构用）
  tmp_path        TEXT,                       -- v1.19 两阶段临时名（仅 ① 的挡路者非空）
  dst_path        TEXT NOT NULL DEFAULT '',
  dst_name        TEXT NOT NULL DEFAULT '',
  dst_key         TEXT NOT NULL DEFAULT '',
  dst_size        INTEGER,
  dst_mtime_ns    INTEGER,
  dst_fingerprint TEXT,
  undo_check      TEXT,
  status          TEXT NOT NULL DEFAULT 'scanned',
  err_code        TEXT,
  err_msg         TEXT,
  FOREIGN KEY(job_id) REFERENCES jobs(id) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS idx_entries_job_seq    ON entries(job_id, seq);
CREATE INDEX IF NOT EXISTS idx_entries_job_status ON entries(job_id, status);
CREATE INDEX IF NOT EXISTS idx_entries_job_id     ON entries(job_id, id);
CREATE INDEX IF NOT EXISTS idx_entries_job_dstkey ON entries(job_id, dst_key);
CREATE INDEX IF NOT EXISTS idx_entries_job_undock ON entries(job_id, undo_check, seq);

-- v1.15 自定义规则模板（§6.7）：不含绝对路径，可跨机器导入导出。
CREATE TABLE IF NOT EXISTS templates (
  id          TEXT PRIMARY KEY,
  name        TEXT NOT NULL UNIQUE,
  rule_json   TEXT NOT NULL,
  created_at  INTEGER NOT NULL,
  updated_at  INTEGER NOT NULL
);
"#;

/// 增量加列：`CREATE TABLE IF NOT EXISTS` 不会给旧库补列，
/// 已落盘的 grename.db 需要显式 ALTER（设计文档 §7 DDL 变更策略）。
///
/// 表名只来自本文件里的字面量，不拼外部输入，故 `format!` 进 PRAGMA 无注入面。
fn has_column(conn: &Connection, table: &str, col: &str) -> AppResult<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let cols: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(1))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(cols.iter().any(|c| c == col))
}

fn ensure_columns(conn: &Connection) -> AppResult<()> {
    if !has_column(conn, "jobs", "rule_json")? {
        conn.execute("ALTER TABLE jobs ADD COLUMN rule_json TEXT", [])?;
    }
    // v1.19：两阶段临时名。旧库补列后值为 NULL，等价于「无临时名」，与旧行为一致。
    if !has_column(conn, "entries", "tmp_path")? {
        conn.execute("ALTER TABLE entries ADD COLUMN tmp_path TEXT", [])?;
    }
    Ok(())
}

pub struct Db {
    conn: Mutex<Connection>,
}

impl Db {
    pub fn open(path: &Path) -> AppResult<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        conn.execute_batch(SCHEMA)?;
        ensure_columns(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// 单写者：所有读写共用一把锁（WAL 下 M0 足够，M1 再拆独立读连接池）。
    pub fn lock(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().expect("db mutex poisoned")
    }
}
