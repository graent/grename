//! 两阶段临时名（设计文档 §6.3 / §6.6.4）。
//!
//! 三类场景会让「直接 rename 到终名」失败：
//! - **链式** `a→b` 且 `b→c`：执行 `a→b` 时 `b` 还在原位；
//! - **交换** `a→b` 且 `b→a`：两条互相占着对方的目标位；
//! - **仅改大小写** `a.txt→A.txt`：大小写不敏感文件系统上目标位就是源文件自身。
//!
//! 三者的共同点是「目标位上坐着本次参与改名的另一个文件」。解法统一为：
//! 先把这些**挡路者**改到同目录的临时名，等全部让位后再落到终名。
//!
//! 本模块只放**纯判定**（临时名怎么起、崩溃后单条该往哪走）。真正的文件操作留在
//! `execute` / `maintenance`，这样判定逻辑可以脱离整棵依赖树、用
//! `rustc --test stage.rs` 独立覆盖。

use std::path::{Path, PathBuf};

/// 与复制模式的 `.part` 共用前缀，便于残留清理与人工识别口径统一。
pub const TMP_PREFIX: &str = "__gre_";
pub const TMP_SUFFIX: &str = ".tmp";

/// `job_id` 取前 8 位足以区分（uuid v4 碰撞概率可忽略），`seq` 保证任务内唯一。
pub fn tmp_name(job_id: &str, seq: i64) -> String {
    let short = &job_id[..8.min(job_id.len())];
    format!("{TMP_PREFIX}{short}_{seq}{TMP_SUFFIX}")
}

/// 临时名必须落在**源文件同目录**：跨卷 `rename` 会退化为「复制 + 删除」，
/// 既不原子，也会在大文件上引入一次完整读写。同目录同卷才保证是元数据操作。
pub fn tmp_path_for(src: &Path, job_id: &str, seq: i64) -> PathBuf {
    src.parent()
        .unwrap_or_else(|| Path::new("."))
        .join(tmp_name(job_id, seq))
}

/// 崩溃后单条的应处置动作。
///
/// 只由「目标 / 源 / 临时名」三者的存在性唯一确定，**不依赖任何内存状态**——
/// 这正是 §6.3「计划已持久化，状态可由文件系统重新推断」的落点。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recovery {
    /// 终名已就位：改名完成但批量状态没回写，补身份快照即可。
    Done,
    /// 只在临时名上：还没落到终名，还原回源名（安全优先，回到执行前）。
    RestoreTmp,
    /// 源还在原位：这条根本没动过。
    Skipped,
    /// 组合不自洽、或文件确实不见了：不猜，交人工核查。
    NeedsReview,
}

/// 八种存在性组合的真值表（终名 / 源名 / 临时名各 2 态）。
///
/// **v1.20 收紧 Done 判定**：原实现是「终名在、临时名不在 ⇒ Done」，隐含前提是
/// 终名不可能是别的源文件名。M4-2 支持链式 / 交换后这个前提不成立了 ——
/// `a→b` 且 `b→c` 时，`b` 既是前一条的终名又是后一条的源名。阶段 1 中途崩溃，
/// 大量 `b` 只是「尚未让位的源文件」，会被误判成已完成产物并开放撤销，
/// 撤销复核后删掉的将是用户原文件。因此 Done 必须同时要求**源名已不在原位**；
/// 源与终名并存时不敢猜，留给人工核查（§6.3「不猜」）。
/// 普通批量改名终名与源名集合不重叠，成功条目恒为 `(true,false,false)`，
/// M2 / M2-扩展的收敛语义不受影响。
pub fn recovery_action(dst_exists: bool, src_exists: bool, tmp_exists: bool) -> Recovery {
    match (dst_exists, src_exists, tmp_exists) {
        // 终名在、源名已不在、临时名不在 → 改名确实完成。
        (true, false, false) => Recovery::Done,
        // 源名与终名并存：可能是别的源文件占位，不猜。
        (true, true, false) => Recovery::NeedsReview,
        // 终名与临时名同时在：终名很可能是外部文件，动临时名会毁掉线索。
        (true, _, true) => Recovery::NeedsReview,
        // 源原位、无临时名 → 没开始过。
        (false, true, false) => Recovery::Skipped,
        // 只在临时名上 → 阶段 1 完成、阶段 2 未做，还原。
        (false, false, true) => Recovery::RestoreTmp,
        // 源与临时名同时存在：多出一份，可能是上次还原没删干净。
        (false, true, true) => Recovery::NeedsReview,
        (false, false, false) => Recovery::NeedsReview,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tmp_name_is_stable_and_prefixed() {
        let id = "0123456789abcdef";
        assert_eq!(tmp_name(id, 7), "__gre_01234567_7.tmp");
        // 与 .part 同前缀，便于统一清理。
        assert!(tmp_name(id, 7).starts_with(TMP_PREFIX));
        assert!(tmp_name(id, 7).ends_with(TMP_SUFFIX));
    }

    #[test]
    fn tmp_name_tolerates_short_job_id() {
        // 独立测试 / 手工构造的短 id 不能 panic。
        assert_eq!(tmp_name("abc", 1), "__gre_abc_1.tmp");
        assert_eq!(tmp_name("", 1), "__gre__1.tmp");
    }

    #[test]
    fn tmp_path_stays_in_source_dir() {
        let src = Path::new("/data/in/a.txt");
        let tmp = tmp_path_for(src, "0123456789abcdef", 3);
        // 跨目录会让 rename 变成复制 + 删除，这里断言同目录。
        assert_eq!(tmp.parent(), src.parent());
        assert_eq!(tmp.file_name().unwrap(), "__gre_01234567_3.tmp");
    }

    #[test]
    fn recovery_prefers_done_when_target_exists() {
        assert_eq!(recovery_action(true, false, false), Recovery::Done);
    }

    /// v1.20 回归：链式任务里终名与另一个源名重叠时，不得把仍在原位的
    /// 源文件补记成产物（否则撤销会删掉用户原文件）。
    #[test]
    fn recovery_does_not_mistake_another_source_for_a_done_target() {
        // `a→b` 且 `b→c`：阶段 1 中途崩溃时 b 是尚未让位的源文件。
        assert_eq!(recovery_action(true, true, false), Recovery::NeedsReview);
        // 源已不在，才是真正的「改名完成但状态没回写」。
        assert_eq!(recovery_action(true, false, false), Recovery::Done);
    }

    #[test]
    fn recovery_restores_only_tmp() {
        assert_eq!(recovery_action(false, false, true), Recovery::RestoreTmp);
    }

    #[test]
    fn recovery_skips_untouched() {
        assert_eq!(recovery_action(false, true, false), Recovery::Skipped);
    }

    #[test]
    fn recovery_flags_ambiguous_combinations() {
        // 终名 + 临时名：可能是外部文件占了位。
        assert_eq!(recovery_action(true, true, true), Recovery::NeedsReview);
        assert_eq!(recovery_action(true, false, true), Recovery::NeedsReview);
        // 源与临时名双份。
        assert_eq!(recovery_action(false, true, true), Recovery::NeedsReview);
        // 三者皆无。
        assert_eq!(recovery_action(false, false, false), Recovery::NeedsReview);
    }
}
