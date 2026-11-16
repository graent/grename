//! M1 规则引擎（设计文档 §6.2）。
//!
//! 关键约束：**预览与执行共用本文件的同一个 `compute`**，不允许前端自己用 JS RegExp
//! 算预览——Rust `regex` 与 JS 正则语义不等价，会出现「预览改成 A、实际改成 B」。
//!
//! - 引擎：RE2 系 `regex`，线性时间、免疫 ReDoS；不支持环视/反向引用（v1 已确认不需要）。
//! - 作用范围：name（主名，扩展名不动）/ full（完整文件名）/ ext（仅扩展名）。
//! - 模板变量：`$1` `${name}` 由 regex 处理；`{n}` `{name}` `{stem}` `{ext}` 由本模块展开。
//! - 内存：每条独立计算，无 O(N) 常驻结构。

use regex::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};

use crate::model::{AppError, AppResult};
use crate::pathpolicy::PathPolicy;

fn default_scope() -> String {
    "name".to_string()
}
fn default_seq_start() -> i64 {
    1
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuleSpec {
    /// 匹配正则，作用于 scope 选出的部分；留空等价于 `^.*$`（整段替换/加前后缀）。
    #[serde(default)]
    pub pattern: String,
    /// 替换模板，支持 `$1`、`$0`、`${name}`，以及 `{n}` `{name}` `{stem}` `{ext}`。
    #[serde(default)]
    pub replacement: String,
    /// name | full | ext
    #[serde(default = "default_scope")]
    pub scope: String,
    /// 文件名过滤正则（对完整文件名），留空 = 全部；不匹配则跳过。
    #[serde(default)]
    pub filter: String,
    #[serde(default)]
    pub case_insensitive: bool,
    /// 全局替换；关闭则只替换首个匹配。
    #[serde(default = "default_match_all")]
    pub match_all: bool,
    /// `{n}` 起始值。
    #[serde(default = "default_seq_start")]
    pub seq_start: i64,
    /// `{n}` 左侧补零宽度，0 = 不补零。
    #[serde(default)]
    pub seq_width: usize,
}

fn default_match_all() -> bool {
    true
}

impl Default for RuleSpec {
    fn default() -> Self {
        Self {
            pattern: String::new(),
            replacement: String::new(),
            scope: default_scope(),
            filter: String::new(),
            case_insensitive: false,
            match_all: true,
            seq_start: default_seq_start(),
            seq_width: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NameResult {
    pub dst_name: Option<String>,
    pub skipped: bool,
    /// FILTERED | NO_CHANGE | INVALID_NAME
    pub reason: Option<String>,
}

impl NameResult {
    fn skip(reason: &str) -> Self {
        Self {
            dst_name: None,
            skipped: true,
            reason: Some(reason.to_string()),
        }
    }
    fn ok(name: String) -> Self {
        Self {
            dst_name: Some(name),
            skipped: false,
            reason: None,
        }
    }
}

pub struct CompiledRule {
    pub spec: RuleSpec,
    re: Regex,
    filter: Option<Regex>,
}

impl CompiledRule {
    pub fn compile(spec: RuleSpec) -> AppResult<Self> {
        if !matches!(spec.scope.as_str(), "name" | "full" | "ext") {
            return Err(AppError::new(
                "RULE_SCOPE_INVALID",
                "作用范围只能是 name / full / ext",
            ));
        }
        let pattern = if spec.pattern.is_empty() {
            "^.*$".to_string()
        } else {
            spec.pattern.clone()
        };
        let re = RegexBuilder::new(&pattern)
            .case_insensitive(spec.case_insensitive)
            .build()
            .map_err(|e| AppError::new("RULE_PATTERN_INVALID", format!("正则无效：{e}")))?;
        let filter = if spec.filter.trim().is_empty() {
            None
        } else {
            Some(
                RegexBuilder::new(&spec.filter)
                    .case_insensitive(spec.case_insensitive)
                    .build()
                    .map_err(|e| {
                        AppError::new("RULE_FILTER_INVALID", format!("过滤正则无效：{e}"))
                    })?,
            )
        };
        Ok(Self { spec, re, filter })
    }

    /// 单条计算。`seq` 来自扫描阶段落库的序号，保证 `{n}` 在批次间连续稳定。
    pub fn compute(&self, src_name: &str, seq: i64) -> NameResult {
        if let Some(f) = &self.filter {
            if !f.is_match(src_name) {
                return NameResult::skip("FILTERED");
            }
        }
        let (stem, ext) = split_name(src_name);
        let (prefix, subject, suffix): (&str, &str, String) = match self.spec.scope.as_str() {
            "full" => ("", src_name, String::new()),
            "ext" => (
                if ext.is_empty() { src_name } else { stem },
                ext,
                String::new(),
            ),
            _ => ("", stem, if ext.is_empty() { String::new() } else { format!(".{ext}") }),
        };
        // scope=ext 时需要在主名后补一个点。
        let prefix_owned: String = if self.spec.scope == "ext" && !ext.is_empty() {
            format!("{prefix}.")
        } else if self.spec.scope == "ext" {
            format!("{src_name}.")
        } else {
            prefix.to_string()
        };

        let n = self.spec.seq_start + seq;
        let seq_text = if self.spec.seq_width > 0 {
            format!("{n:0width$}", n = n, width = self.spec.seq_width)
        } else {
            n.to_string()
        };
        let vars = Vars {
            seq: seq_text.as_str(),
            name: src_name,
            stem,
            ext,
        };
        let template = expand_vars(&self.spec.replacement, &vars);
        let replaced = if self.spec.match_all {
            self.re.replace_all(subject, template.as_str()).into_owned()
        } else {
            self.re.replace(subject, template.as_str()).into_owned()
        };
        let dst = format!("{prefix_owned}{replaced}{suffix}");

        // 平台合法性校验统一走 PathPolicy（M4）：非法字符、保留名、长度上限
        // 都按**当前运行平台**判定，不再只对 Windows 分支硬编码。
        if let Err(code) = PathPolicy::current().check_name(&dst) {
            return NameResult::skip(code);
        }
        if dst == src_name {
            return NameResult::skip("NO_CHANGE");
        }
        NameResult::ok(dst)
    }
}

fn split_name(s: &str) -> (&str, &str) {
    match s.rfind('.') {
        Some(i) if i > 0 => (&s[..i], &s[i + 1..]),
        _ => (s, ""),
    }
}

struct Vars<'a> {
    seq: &'a str,
    name: &'a str,
    stem: &'a str,
    ext: &'a str,
}

/// 单遍扫描展开 `{n}` / `{name}` / `{stem}` / `{ext}`。
/// 变量值里的 `$` 转义成 `$$`，避免被 regex 的替换语法误解析。
fn expand_vars(t: &str, v: &Vars<'_>) -> String {
    let mut out = String::with_capacity(t.len());
    let bytes = t.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{' {
            if let Some(rel) = t[i..].find('}') {
                let end = i + rel;
                let val = match &t[i + 1..end] {
                    "n" | "seq" => Some(v.seq),
                    "name" => Some(v.name),
                    "stem" => Some(v.stem),
                    "ext" => Some(v.ext),
                    _ => None,
                };
                if let Some(s) = val {
                    for ch in s.chars() {
                        if ch == '$' {
                            out.push_str("$$");
                        } else {
                            out.push(ch);
                        }
                    }
                    i = end + 1;
                    continue;
                }
            }
        }
        let ch = t[i..].chars().next().unwrap_or('\u{fffd}');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}
