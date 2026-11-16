//! 计划生成（M1）：按规则计算目标名、输出目录硬校验、冲突检测下推 SQLite。
//! 设计文档 §6.0.3（嵌套拒绝）、§6.0.4（冲突不占内存）、§6.2（单一规则引擎）、§6.3。
//!
//! - 规则在**计划期**分批计算，内存 O(批)；改规则无需重扫。
//! - `build_plan` 与 `preview_rule` 共用 `CompiledRule::compute`，预览与实际执行必然一致。
//! - M0 的「扫描期占位目标名」已移除，`entries.dst_name` 由本模块写入。

use std::path::{Path, PathBuf};

use rusqlite::params;

use crate::model::{AppError, AppResult, PlanSummary, PreviewItem};
use crate::pathpolicy::PathPolicy;
use crate::rule::{CompiledRule, RuleSpec};
use crate::stage;
use crate::state::AppState;

const PLAN_BATCH: i64 = 1_000;

/// 目标路径计算：`build_plan` 与 `preview_rule` 共用，避免两处漂移导致
/// 「预览通过、执行时被判超长」这种违背「预览 = 执行」的偏差。
fn dst_path_of(
    src_path: &str,
    rel_dir: &str,
    dst_name: &str,
    output_mode: &str,
    output_dir: Option<&str>,
) -> PathBuf {
    if output_mode == "in_place" {
        Path::new(src_path)
            .parent()
            .unwrap_or_else(|| Path::new(""))
            .join(dst_name)
    } else {
        let base = output_dir.unwrap_or("");
        let rel = if rel_dir.is_empty() {
            PathBuf::new()
        } else {
            PathBuf::from(rel_dir.replace('/', std::path::MAIN_SEPARATOR_STR))
        };
        Path::new(base).join(rel).join(dst_name)
    }
}

pub fn build_plan(
    state: &AppState,
    job_id: &str,
    output_mode: &str,
    output_dir: Option<String>,
    rule: RuleSpec,
) -> AppResult<PlanSummary> {
    if output_mode != "in_place" && output_mode != "to_dir" {
        return Err(AppError::new(
            "OUTPUT_MODE_INVALID",
            "output_mode 只能是 in_place 或 to_dir",
        ));
    }
    let compiled = CompiledRule::compile(rule.clone())?;
    let policy = PathPolicy::current();

    let (root_path, total, bytes_total): (String, i64, i64) = {
        let conn = state.db.lock();
        conn.query_row(
            "SELECT root_path, total, bytes_total FROM jobs WHERE id=?1",
            params![job_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?
    };

    if output_mode == "to_dir" {
        let out = output_dir.as_deref().unwrap_or("");
        if out.trim().is_empty() {
            return Err(AppError::new(
                "OUTPUT_DIR_INVALID",
                "复制模式必须指定输出目录",
            ));
        }
        let out_path = PathBuf::from(out);
        // 重叠判定按当前平台的路径语义折叠（设计文档 §3.3）：
        // 大小写敏感平台上 `/data/out` 与 `/data/OUT` 是两个目录，不应误拒绝。
        if policy.paths_overlap(&out_path, Path::new(&root_path)) {
            return Err(AppError::new(
                "OUTPUT_DIR_NESTED",
                "输出目录不能与扫描目录互相包含，否则会把新生成的文件再次纳入扫描。请另选目录。",
            ));
        }
    }

    // 分批计算 dst_name / dst_path / dst_key，内存 O(批)。
    //
    // 先清空上一轮的两阶段标记：改规则重跑计划时，旧 tmp_path 会让
    // 「阶段 1 只处理 tmp_path 非空」误把新条目当成挡路者。
    {
        let conn = state.db.lock();
        conn.execute(
            "UPDATE entries SET tmp_path=NULL WHERE job_id=?1",
            params![job_id],
        )?;
    }
    let mut cursor: i64 = 0;
    loop {
        struct Row {
            id: i64,
            seq: i64,
            src_path: String,
            src_name: String,
            rel_dir: String,
        }
        let rows: Vec<Row> = {
            let conn = state.db.lock();
            let mut stmt = conn.prepare(
                "SELECT id, seq, src_path, src_name, rel_dir FROM entries
                 WHERE job_id=?1 AND id>?2 ORDER BY id LIMIT ?3",
            )?;
            let mapped = stmt.query_map(params![job_id, cursor, PLAN_BATCH], |r| {
                Ok(Row {
                    id: r.get(0)?,
                    seq: r.get(1)?,
                    src_path: r.get(2)?,
                    src_name: r.get(3)?,
                    rel_dir: r.get(4)?,
                })
            })?;
            mapped.collect::<Result<Vec<_>, _>>()?
        };
        if rows.is_empty() {
            break;
        }
        cursor = rows.last().map(|r| r.id).unwrap_or(cursor);

        let mut conn = state.db.lock();
        let tx = conn.transaction()?;
        {
            let mut ok_stmt = tx.prepare(
                "UPDATE entries SET dst_name=?1, dst_path=?2, dst_key=?3,
                 status='planned', err_code=NULL, err_msg=NULL WHERE id=?4",
            )?;
            let mut skip_stmt = tx.prepare(
                "UPDATE entries SET dst_name='', dst_path='', dst_key='',
                 status='skipped', err_code=?1, err_msg=NULL WHERE id=?2",
            )?;
            for row in rows.iter() {
                let r = compiled.compute(&row.src_name, row.seq);
                let Some(dst_name) = r.dst_name else {
                    skip_stmt.execute(params![r.reason.as_deref().unwrap_or("NO_CHANGE"), row.id])?;
                    continue;
                };
                let dst_path = dst_path_of(
                    &row.src_path,
                    &row.rel_dir,
                    &dst_name,
                    output_mode,
                    output_dir.as_deref(),
                );
                // 计划期校验全路径长度（§3.3「Windows MAX_PATH 需在计划期校验」）。
                // 单组件长度已由 compute → check_name 拦截，这里补的是拼接后的整体长度：
                // 深层目录 + 长文件名可能在组件都合法时仍越过 259 字符上限。
                if let Err(code) = policy.check_path(&dst_path) {
                    skip_stmt.execute(params![code, row.id])?;
                    continue;
                }
                // 归一化键：相对目录 + 按当前平台大小写语义折叠后的目标名（§3.3）。
                // 折叠策略由 PathPolicy 决定，不再无条件 to_lowercase()。
                let dst_key = policy.fold(&format!("{}/{}", row.rel_dir, dst_name));
                ok_stmt.execute(params![
                    dst_name,
                    dst_path.to_string_lossy().into_owned(),
                    dst_key,
                    row.id
                ])?;
            }
        }
        tx.commit()?;
    }

    // 冲突检测下推 SQLite：由磁盘索引承担 O(N) 状态，内存恒定（§6.0.4）。
    let conflicted: i64 = {
        let conn = state.db.lock();
        conn.execute(
            "UPDATE entries SET status='conflict', err_code='CONFLICT'
             WHERE job_id=?1 AND status='planned' AND dst_key IN (
               SELECT dst_key FROM entries WHERE job_id=?1 AND status='planned'
               GROUP BY dst_key HAVING COUNT(*) > 1
             )",
            params![job_id],
        )?;
        conn.query_row(
            "SELECT COUNT(*) FROM entries WHERE job_id=?1 AND status='conflict'",
            params![job_id],
            |r| r.get(0),
        )?
    };

    // 规则快照：模板后续改动不影响历史任务的可解释性（§3.2 第 9 条）。
    let rule_json = serde_json::to_string(&rule).unwrap_or_default();
    let planned: i64 = {
        let conn = state.db.lock();
        conn.execute(
            "UPDATE jobs SET output_mode=?1, output_dir=?2, conflicted=?3, rule_json=?4 WHERE id=?5",
            params![output_mode, output_dir.as_deref(), conflicted, rule_json, job_id],
        )?;
        conn.query_row(
            "SELECT COUNT(*) FROM entries WHERE job_id=?1 AND status='planned'",
            params![job_id],
            |r| r.get(0),
        )?
    };
    {
        let conn = state.db.lock();
        conn.execute(
            "UPDATE jobs SET skipped=(SELECT COUNT(*) FROM entries WHERE job_id=?1 AND status='skipped') WHERE id=?1",
            params![job_id],
        )?;
    }

    // §6.3 两阶段临时名：只有 ① 需要。② 的源文件全程只读，目标位不可能被
    // 本次参与改名的源文件占用，因此不标记、执行期也就完全走原路径。
    let staged = if output_mode == "in_place" {
        mark_staged(state, job_id, &policy)?
    } else {
        0
    };
    if staged > 0 {
        eprintln!("[grename] job {job_id} 有 {staged} 条经两阶段临时名让位");
    }

    Ok(PlanSummary {
        job_id: job_id.to_string(),
        total,
        planned,
        conflicted,
        bytes_total,
        output_dir,
    })
}

/// 标记「挡路者」（§6.3）：源路径折叠后落在本次某个待执行目标路径上的条目。
///
/// 典型来源是链式 `a→b` + `b→c`、交换 `a↔b`，以及仅改大小写 `a.txt→A.txt`
/// （大小写不敏感文件系统上目标位就是源文件自身）。这些条目的目标位上坐着
/// 另一个参与改名的文件，直接 `rename` 必然失败，必须先经临时名让位。
///
/// 集合只驻留 64 位哈希（约 8 字节/条，10 万条约 0.8 MB），不驻留路径字符串。
/// 哈希碰撞的方向是「多判一个挡路者」——该条目只是多走一次同目录 `rename`，
/// 结果仍然正确，所以不需要精确的字符串集合。
fn mark_staged(state: &AppState, job_id: &str, policy: &PathPolicy) -> AppResult<i64> {
    use std::collections::hash_map::DefaultHasher;
    use std::collections::HashSet;
    use std::hash::{Hash, Hasher};

    fn fold_hash(policy: &PathPolicy, s: &str) -> u64 {
        let mut h = DefaultHasher::new();
        policy.fold(s).hash(&mut h);
        h.finish()
    }

    // 第一遍：收集全部待执行目标的折叠哈希。
    let mut dst_hashes: HashSet<u64> = HashSet::new();
    let mut cursor: i64 = 0;
    loop {
        let rows: Vec<(i64, String)> = {
            let conn = state.db.lock();
            let mut stmt = conn.prepare(
                "SELECT id, dst_path FROM entries
                 WHERE job_id=?1 AND status='planned' AND id>?2 ORDER BY id LIMIT ?3",
            )?;
            stmt.query_map(params![job_id, cursor, PLAN_BATCH], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?
        };
        if rows.is_empty() {
            break;
        }
        cursor = rows.last().map(|r| r.0).unwrap_or(cursor);
        for (_, dst) in rows.iter() {
            dst_hashes.insert(fold_hash(policy, dst));
        }
    }
    if dst_hashes.is_empty() {
        return Ok(0);
    }

    // 第二遍：源命中集合的就是挡路者，顺带算出它的同目录临时名。
    let mut staged: i64 = 0;
    let mut cursor: i64 = 0;
    loop {
        let rows: Vec<(i64, i64, String)> = {
            let conn = state.db.lock();
            let mut stmt = conn.prepare(
                "SELECT id, seq, src_path FROM entries
                 WHERE job_id=?1 AND status='planned' AND id>?2 ORDER BY id LIMIT ?3",
            )?;
            stmt.query_map(params![job_id, cursor, PLAN_BATCH], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })?
            .collect::<Result<Vec<_>, _>>()?
        };
        if rows.is_empty() {
            break;
        }
        cursor = rows.last().map(|r| r.0).unwrap_or(cursor);

        let mut marked: Vec<(i64, String)> = Vec::new();
        for (id, seq, src_path) in rows.iter() {
            if !dst_hashes.contains(&fold_hash(policy, src_path)) {
                continue;
            }
            let tmp = stage::tmp_path_for(Path::new(src_path), job_id, *seq);
            marked.push((*id, tmp.to_string_lossy().into_owned()));
        }
        if marked.is_empty() {
            continue;
        }
        {
            let mut conn = state.db.lock();
            let tx = conn.transaction()?;
            {
                let mut stmt = tx.prepare("UPDATE entries SET tmp_path=?1 WHERE id=?2")?;
                for (id, tmp) in marked.iter() {
                    stmt.execute(params![tmp, id])?;
                }
            }
            tx.commit()?;
        }
        staged += marked.len() as i64;
    }
    Ok(staged)
}

/// 实时预览：与 build_plan 共用 compute **和** 同一套平台校验，但只读当前页、
/// 不写库、不重扫。
///
/// 路径长度校验必须在此重现：单条 `dst_name` 合法不代表拼接后仍合法，
/// 若预览只查名字、计划期才查全路径，用户会看到「预览全是 planned、
/// 执行后一片 skipped」的偏差（违背 §3.2 第 2 条）。
pub fn preview_rule(
    state: &AppState,
    job_id: &str,
    rule: RuleSpec,
    cursor: i64,
    limit: i64,
) -> AppResult<Vec<PreviewItem>> {
    let compiled = CompiledRule::compile(rule)?;
    let policy = PathPolicy::current();
    let conn = state.db.lock();
    // 建计划前的预览，jobs.output_mode 尚未写入，按 in_place 语义算路径
    // （与主界面默认一致；两者都只会拼到源文件所在目录，长度口径不会更宽松）。
    let (raw_mode, out_dir): (String, Option<String>) = conn.query_row(
        "SELECT output_mode, output_dir FROM jobs WHERE id=?1",
        params![job_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let output_mode = if raw_mode.is_empty() { "in_place" } else { raw_mode.as_str() };
    let mut stmt = conn.prepare(
        "SELECT id, seq, src_name, src_path, rel_dir FROM entries
         WHERE job_id=?1 AND id>?2 ORDER BY id LIMIT ?3",
    )?;
    let rows = stmt
        .query_map(params![job_id, cursor, limit], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows
        .into_iter()
        .map(|(id, seq, src_name, src_path, rel_dir)| {
            let r = compiled.compute(&src_name, seq);
            let Some(dst_name) = r.dst_name else {
                return PreviewItem {
                    id,
                    src_name,
                    dst_name: None,
                    status: "skipped".to_string(),
                    reason: r.reason,
                };
            };
            let path = dst_path_of(&src_path, &rel_dir, &dst_name, output_mode, out_dir.as_deref());
            match policy.check_path(&path) {
                Ok(()) => PreviewItem {
                    id,
                    src_name,
                    dst_name: Some(dst_name),
                    status: "planned".to_string(),
                    reason: None,
                },
                Err(code) => PreviewItem {
                    id,
                    src_name,
                    dst_name: None,
                    status: "skipped".to_string(),
                    reason: Some(code.to_string()),
                },
            }
        })
        .collect())
}
