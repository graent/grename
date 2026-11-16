//! M4 跨平台路径与文件系统策略（设计文档 §3.3）。
//!
//! 目的：把分散在各调用点的「写死单一 OS 规则」收敛为一组可参数化策略。
//! 改造前存在三处真实缺陷，均源于硬编码：
//!   1. `plan.rs` 冲突键用 `to_lowercase()` 无条件折叠 —— Linux（ext4 大小写敏感）
//!      会把 `a.txt` 与 `A.txt` 误判为冲突，属于假阳性。
//!   2. `plan.rs` 输出目录嵌套判定同样无条件折叠 —— Linux 上 `/data/out` 与
//!      `/data/OUT` 会被误判为互相包含，属于假阴性式误拒绝。
//!   3. `execute.rs` 用 `dst.exists()` 拦 `TARGET_EXISTS` —— 在大小写不敏感文件系统上
//!      「仅改大小写」（`a.txt → A.txt`）会命中源文件自身，被误判为目标已存在。
//!
//! 本模块只负责**判定**，不做任何 I/O、不修改文件系统，因此可在计划期、预览期
//! 与执行期反复调用。

use std::path::{Component, Path, PathBuf};

/// 文件系统的大小写语义。
///
/// 这是「平台默认」，不是「本卷实际」。Windows 的 NTFS、macOS 的 APFS/HFS+ 默认
/// 不敏感；Linux 的 ext4/xfs/btrfs 敏感。可配置为敏感的卷（如 macOS 上格式化为
/// 区分大小写的 APFS）不在默认探测范围内，属已知限制，见报告 §5。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CaseSensitivity {
    Sensitive,
    Insensitive,
}

/// Windows 路径中不允许出现的字符（含两种分隔符，文件名内不得含任一）。
const WINDOWS_FORBIDDEN: &[char] = &['<', '>', ':', '"', '|', '?', '*', '/', '\\'];

/// Unix 系只禁止 `/`；NUL 由 `char::is_control()` 一并覆盖。
const UNIX_FORBIDDEN: &[char] = &['/'];

/// Windows 保留设备名。Win32 对其判定与扩展名无关（`CON.txt` 同样被拒），
/// 故比较时取第一个 `.` 之前的部分。
const WINDOWS_RESERVED: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$", "COM1", "COM2", "COM3", "COM4", "COM5",
    "COM6", "COM7", "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7",
    "LPT8", "LPT9",
];

/// 单组件长度上限。ext4 / APFS / NTFS 均为 255，按 UTF-8 字节数判定，
/// 对 NTFS 的 UTF-16 计数口径偏严，但偏严只会提前拦截、不会漏放。
const MAX_COMPONENT: usize = 255;

/// 全路径长度上限。Windows 传统 MAX_PATH 为 260（含盘符与结尾 NUL，故可用 259）；
/// Unix 系 PATH_MAX 通常 4096。0 表示不限制。
const WINDOWS_MAX_PATH: usize = 259;
const UNIX_MAX_PATH: usize = 4096;

/// 一组平台判定策略。`const` 构造，供 `current()` 零成本返回。
#[derive(Clone, Copy, Debug)]
pub struct PathPolicy {
    pub case: CaseSensitivity,
    /// 文件名内禁止的字符（不含控制字符，控制字符统一额外判定）。
    pub forbidden: &'static [char],
    /// 保留设备名；空切片表示该平台无此概念。
    pub reserved: &'static [&'static str],
    /// 单组件最大字节数。
    pub max_component: usize,
    /// 全路径最大长度，0 = 不限制。
    pub max_path: usize,
    /// 全路径长度的计量单位是否为 UTF-16 code unit。
    /// Win32 的 MAX_PATH 按 UTF-16 计，Unix 的 PATH_MAX 按字节计。
    pub utf16_len: bool,
}

const fn windows_policy() -> PathPolicy {
    PathPolicy {
        case: CaseSensitivity::Insensitive,
        forbidden: WINDOWS_FORBIDDEN,
        reserved: WINDOWS_RESERVED,
        max_component: MAX_COMPONENT,
        max_path: WINDOWS_MAX_PATH,
        utf16_len: true,
    }
}

/// macOS 默认大小写不敏感，但**没有**保留设备名，也**不**禁止 `:`。
///
/// `:` 在 HFS+ 时代是路径分隔符，Finder 展示时仍会把 `:` 显示为 `/`；
/// 但 APFS 允许在 APFS 卷上创建含 `:` 的文件，且设计文档 §3.3 明确
/// 「macOS 仅 `/` 与 NUL」，故此处按文档口径不禁止。
const fn macos_policy() -> PathPolicy {
    PathPolicy {
        case: CaseSensitivity::Insensitive,
        forbidden: UNIX_FORBIDDEN,
        reserved: &[],
        max_component: MAX_COMPONENT,
        max_path: UNIX_MAX_PATH,
        utf16_len: false,
    }
}

const fn linux_policy() -> PathPolicy {
    PathPolicy {
        case: CaseSensitivity::Sensitive,
        forbidden: UNIX_FORBIDDEN,
        reserved: &[],
        max_component: MAX_COMPONENT,
        max_path: UNIX_MAX_PATH,
        utf16_len: false,
    }
}

impl PathPolicy {
    /// 当前编译目标的平台默认策略。
    pub const fn current() -> PathPolicy {
        if cfg!(windows) {
            windows_policy()
        } else if cfg!(target_os = "macos") {
            macos_policy()
        } else {
            linux_policy()
        }
    }

    /// 名前折叠：大小写敏感时原样返回，不敏感时转小写。
    ///
    /// 这是**冲突键与嵌套判定**共用的唯一折叠入口。改造前 `plan.rs` 直接写
    /// `to_lowercase()`，导致 Linux 上出现假冲突。
    pub fn fold(&self, name: &str) -> String {
        match self.case {
            CaseSensitivity::Sensitive => name.to_string(),
            CaseSensitivity::Insensitive => name.to_lowercase(),
        }
    }

    /// 两个文件名在该策略下是否指向同一个目录项。
    pub fn same_name(&self, a: &str, b: &str) -> bool {
        self.fold(a) == self.fold(b)
    }

    /// 校验单个文件名。返回 `INVALID_NAME` / `NAME_TOO_LONG`，合法则 `Ok(())`。
    ///
    /// 判定顺序按「先廉价的字符串检查、后依赖平台表的检查」排列，
    /// 热路径（每条 entry 算一次目标名）上绝大多数非法输入在第一段就被拦下。
    pub fn check_name(&self, name: &str) -> Result<(), &'static str> {
        if name.is_empty() {
            return Err("INVALID_NAME");
        }
        // 全由点与空格构成的名称（`.`、`..`、`...`、`   `）一律拒绝：
        // `.`/`..` 有特殊语义，其余在 Windows 上会被静默剥除尾部而变成空名。
        if name.trim_end_matches(['.', ' ']).is_empty() {
            return Err("INVALID_NAME");
        }
        if name.ends_with('.') || name.ends_with(' ') {
            return Err("INVALID_NAME");
        }
        if name
            .chars()
            .any(|c| c.is_control() || self.forbidden.contains(&c))
        {
            return Err("INVALID_NAME");
        }
        if self.max_component > 0 && name.len() > self.max_component {
            return Err("NAME_TOO_LONG");
        }
        if !self.reserved.is_empty() {
            // 保留名与扩展名无关：`CON.txt` 在 Win32 下同样被拒。
            let head = name.split('.').next().unwrap_or(name).to_ascii_uppercase();
            if self.reserved.contains(&head.as_str()) {
                return Err("INVALID_NAME");
            }
        }
        Ok(())
    }

    /// 校验完整目标路径：末段组件合法性 + 全路径长度。
    pub fn check_path(&self, path: &Path) -> Result<(), &'static str> {
        let text = path.to_string_lossy();
        if let Some(name) = path.file_name() {
            self.check_name(&name.to_string_lossy())?;
        } else if text.trim_end_matches(['/', '\\']).is_empty() {
            return Err("INVALID_NAME");
        }
        if self.max_path > 0 {
            let units = if self.utf16_len {
                text.encode_utf16().count()
            } else {
                text.len()
            };
            if units > self.max_path {
                return Err("NAME_TOO_LONG");
            }
        }
        Ok(())
    }

    /// 输出目录与扫描根是否互相重叠（任一方向包含，或完全相等）。
    ///
    /// 递归扫描时若输出目录落在扫描根内，产物会被再次扫入；若扫描根落在输出目录内，
    /// 重建相对目录后产物会落回扫描根。两个方向都必须拒绝（§6.0.3）。
    pub fn paths_overlap(&self, a: &Path, b: &Path) -> bool {
        let fa = self.fold(&strip_verbatim(&normalize(a)).to_string_lossy());
        let fb = self.fold(&strip_verbatim(&normalize(b)).to_string_lossy());
        let ta = fa.trim_end_matches(['/', '\\']);
        let tb = fb.trim_end_matches(['/', '\\']);
        if ta == tb {
            return true;
        }
        ta.starts_with(&format!("{tb}/"))
            || ta.starts_with(&format!("{tb}\\"))
            || tb.starts_with(&format!("{ta}/"))
            || tb.starts_with(&format!("{ta}\\"))
    }
}

/// 词法规范化（不解析符号链接），兼容「输出目录尚不存在」的场景。
///
/// 不解析符号链接是刻意的：计划期输出目录可能尚未创建，`canonicalize` 会失败；
/// 而符号链接造成的真实重叠由执行期的 `TARGET_EXISTS` 兜底。
pub fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// 剥掉 Windows 的 verbatim 前缀，使 `\\?\E:\a` 与 `E:\a` 比较时等价。
///
/// 未引入 `dunce` crate（避免为一条形式化处理新增依赖与下载），
/// 这里只做比较前的前缀剥离，不改写实际传给 Win32 的路径。
fn strip_verbatim(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        return PathBuf::from(rest);
    }
    p.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sensitive() -> PathPolicy {
        PathPolicy {
            case: CaseSensitivity::Sensitive,
            forbidden: UNIX_FORBIDDEN,
            reserved: &[],
            max_component: MAX_COMPONENT,
            max_path: UNIX_MAX_PATH,
            utf16_len: false,
        }
    }

    fn insensitive_winlike() -> PathPolicy {
        PathPolicy {
            case: CaseSensitivity::Insensitive,
            forbidden: WINDOWS_FORBIDDEN,
            reserved: WINDOWS_RESERVED,
            max_component: MAX_COMPONENT,
            max_path: WINDOWS_MAX_PATH,
            utf16_len: true,
        }
    }

    #[test]
    fn fold_respects_case_semantics() {
        assert_eq!(sensitive().fold("A.TXT"), "A.TXT");
        assert_eq!(insensitive_winlike().fold("A.TXT"), "a.txt");
        assert!(!sensitive().same_name("a.txt", "A.txt"));
        assert!(insensitive_winlike().same_name("a.txt", "A.txt"));
    }

    #[test]
    fn rejects_trailing_dot_and_space() {
        for p in [sensitive(), insensitive_winlike()] {
            assert_eq!(p.check_name("a."), Err("INVALID_NAME"));
            assert_eq!(p.check_name("a "), Err("INVALID_NAME"));
            assert_eq!(p.check_name("..."), Err("INVALID_NAME"));
            assert_eq!(p.check_name(""), Err("INVALID_NAME"));
            assert!(p.check_name("a.b.txt").is_ok());
        }
    }

    #[test]
    fn forbidden_chars_are_platform_scoped() {
        // 冒号在 Windows 上非法，在 Unix 上合法（macOS 按 §3.3 口径亦合法）。
        assert_eq!(insensitive_winlike().check_name("a:b"), Err("INVALID_NAME"));
        assert!(sensitive().check_name("a:b").is_ok());
        // 斜杠三平台一律非法。
        assert_eq!(sensitive().check_name("a/b"), Err("INVALID_NAME"));
        assert_eq!(insensitive_winlike().check_name("a/b"), Err("INVALID_NAME"));
        // 控制字符一律非法。
        assert_eq!(sensitive().check_name("a\u{1}b"), Err("INVALID_NAME"));
    }

    #[test]
    fn reserved_names_only_on_windows() {
        assert_eq!(insensitive_winlike().check_name("CON"), Err("INVALID_NAME"));
        assert_eq!(
            insensitive_winlike().check_name("con.tar.gz"),
            Err("INVALID_NAME")
        );
        assert_eq!(
            insensitive_winlike().check_name("COM1.log"),
            Err("INVALID_NAME")
        );
        // Unix 允许这些名字（它们是普通文件名）。
        assert!(sensitive().check_name("CON").is_ok());
        assert!(sensitive().check_name("COM1.log").is_ok());
        // 非保留名不受影响。
        assert!(insensitive_winlike().check_name("CONSOLE.txt").is_ok());
    }

    #[test]
    fn length_limits_are_reported_separately() {
        let long = "a".repeat(MAX_COMPONENT + 1);
        assert_eq!(sensitive().check_name(&long), Err("NAME_TOO_LONG"));
        assert_eq!(insensitive_winlike().check_name(&long), Err("NAME_TOO_LONG"));
        let ok = "a".repeat(MAX_COMPONENT);
        assert!(sensitive().check_name(&ok).is_ok());
    }

    #[test]
    fn path_length_limit_applies_to_full_path() {
        let p = sensitive();
        let deep = format!("/{}", vec!["d".repeat(200); 30].join("/"));
        assert_eq!(p.check_path(Path::new(&deep)), Err("NAME_TOO_LONG"));
    }

    #[test]
    fn overlap_is_case_aware() {
        // Linux：大小写不同即不同目录，不应判重叠。
        let s = sensitive();
        assert!(!s.paths_overlap(Path::new("/data/out"), Path::new("/data/OUT")));
        assert!(s.paths_overlap(Path::new("/data/out"), Path::new("/data")));
        // Windows/macOS：折叠后相同，判重叠。
        let i = insensitive_winlike();
        assert!(i.paths_overlap(Path::new(r"E:\Data\Out"), Path::new(r"e:\data\OUT")));
        // 任一方向包含都拒绝。
        assert!(i.paths_overlap(Path::new(r"E:\data"), Path::new(r"E:\data\out")));
        assert!(i.paths_overlap(Path::new(r"E:\data\out"), Path::new(r"E:\data")));
        // 同名前缀但不是子目录，不算重叠。
        assert!(!i.paths_overlap(Path::new(r"E:\data2"), Path::new(r"E:\data")));
    }

    #[test]
    fn overlap_ignores_dot_segments() {
        let p = sensitive();
        assert!(p.paths_overlap(Path::new("/data/./out/../out"), Path::new("/data/out")));
    }

    #[test]
    fn verbatim_prefix_is_stripped_for_comparison() {
        let p = insensitive_winlike();
        assert!(p.paths_overlap(Path::new(r"\\?\E:\data\out"), Path::new(r"E:\data")));
    }
}
