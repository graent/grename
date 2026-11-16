//! 自定义规则模板（设计文档 §6.7 / §8）：SQLite 持久化 + JSON 导入导出。
//!
//! 约束（§13 第 6 条）：模板含规则快照与输出模式偏好，**不含绝对路径**；
//! 因此导出文件可跨机器复用，也不会把本机目录结构带出去。
//!
//! 新增表用 `CREATE TABLE IF NOT EXISTS`，老库无需 ALTER / 迁移。

use std::fs;

use rusqlite::params;
use serde::{Deserialize, Serialize};
use tauri::State;
use uuid::Uuid;

use crate::model::{AppError, AppResult};
use crate::state::{now_ms, AppState};

const SCHEMA_VERSION: i32 = 1;
const MAX_TEMPLATES: usize = 200;
const MAX_NAME_LEN: usize = 64;
const MAX_RULE_BYTES: usize = 16 * 1024;
const MAX_IMPORT_BYTES: usize = 2 * 1024 * 1024;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TemplateInfo {
    pub id: String,
    pub name: String,
    pub rule_json: String,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TemplateItem {
    pub name: String,
    pub rule_json: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TemplateBundle {
    pub schema: i32,
    pub exported_at: i64,
    pub templates: Vec<TemplateItem>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportResult {
    pub imported: usize,
    pub skipped: usize,
    pub errors: Vec<String>,
}

fn validate(name: &str, rule_json: &str) -> AppResult<()> {
    let n = name.trim();
    if n.is_empty() || n.chars().count() > MAX_NAME_LEN {
        return Err(AppError::new(
            "TEMPLATE_NAME_INVALID",
            format!("模板名须为 1~{MAX_NAME_LEN} 个字符"),
        ));
    }
    if rule_json.is_empty() || rule_json.len() > MAX_RULE_BYTES {
        return Err(AppError::new("TEMPLATE_RULE_INVALID", "规则内容为空或过大"));
    }
    let v: serde_json::Value = serde_json::from_str(rule_json)
        .map_err(|e| AppError::new("TEMPLATE_RULE_INVALID", format!("规则必须为合法 JSON：{e}")))?;
    // §6.7：不得携带绝对输出路径（含 outputDir / output_dir 任一命名）。
    if let Some(obj) = v.as_object() {
        for key in ["outputDir", "output_dir", "rootPath", "root_path"] {
            if obj.get(key).and_then(|x| x.as_str()).unwrap_or("").trim().is_empty() {
                continue;
            }
            return Err(AppError::new(
                "TEMPLATE_ABSOLUTE_PATH",
                "模板不得包含输出目录 / 根目录等绝对路径",
            ));
        }
    }
    Ok(())
}

fn list_impl(state: &AppState) -> AppResult<Vec<TemplateInfo>> {
    let conn = state.db.lock();
    let mut stmt = conn.prepare(
        "SELECT id, name, rule_json, created_at, updated_at FROM templates
         ORDER BY updated_at DESC, name ASC",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(TemplateInfo {
            id: r.get(0)?,
            name: r.get(1)?,
            rule_json: r.get(2)?,
            created_at: r.get(3)?,
            updated_at: r.get(4)?,
        })
    })?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

/// 同名视为更新（幂等），避免反复保存产生噪声条目。
fn save_impl(state: &AppState, name: &str, rule_json: &str) -> AppResult<TemplateInfo> {
    validate(name, rule_json)?;
    let conn = state.db.lock();
    let now = now_ms();
    let existing: Option<(String, i64)> = conn
        .query_row(
            "SELECT id, created_at FROM templates WHERE name=?1",
            params![name.trim()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .ok();
    let (id, created) = match existing {
        Some((id, created)) => {
            conn.execute(
                "UPDATE templates SET rule_json=?1, updated_at=?2 WHERE id=?3",
                params![rule_json, now, id],
            )?;
            (id, created)
        }
        None => {
            let count: i64 = conn.query_row("SELECT COUNT(*) FROM templates", [], |r| r.get(0))?;
            if count as usize >= MAX_TEMPLATES {
                return Err(AppError::new(
                    "TEMPLATE_LIMIT",
                    format!("模板数量已达上限 {MAX_TEMPLATES}"),
                ));
            }
            let id = format!("t_{}", Uuid::new_v4().simple());
            conn.execute(
                "INSERT INTO templates(id, name, rule_json, created_at, updated_at)
                 VALUES(?1, ?2, ?3, ?4, ?5)",
                params![id, name.trim(), rule_json, now, now],
            )?;
            (id, now)
        }
    };
    Ok(TemplateInfo {
        id,
        name: name.trim().to_string(),
        rule_json: rule_json.to_string(),
        created_at: created,
        updated_at: now,
    })
}

#[tauri::command]
pub fn list_templates(state: State<'_, AppState>) -> AppResult<Vec<TemplateInfo>> {
    list_impl(state.inner())
}

#[tauri::command]
pub fn save_template(
    state: State<'_, AppState>,
    name: String,
    rule_json: String,
) -> AppResult<TemplateInfo> {
    save_impl(state.inner(), &name, &rule_json)
}

#[tauri::command]
pub fn delete_template(state: State<'_, AppState>, id: String) -> AppResult<bool> {
    delete_template_impl(state.inner(), &id)
}

pub fn delete_template_impl(state: &AppState, id: &str) -> AppResult<bool> {
    let conn = state.db.lock();
    Ok(conn.execute("DELETE FROM templates WHERE id=?1", params![id])? > 0)
}

/// 导出到用户选定路径；`ids` 为空表示导出全部。
#[tauri::command]
pub fn export_templates(
    state: State<'_, AppState>,
    path: String,
    ids: Option<Vec<String>>,
) -> AppResult<usize> {
    export_templates_impl(state.inner(), &path, ids)
}

pub fn export_templates_impl(
    state: &AppState,
    path: &str,
    ids: Option<Vec<String>>,
) -> AppResult<usize> {
    let mut items: Vec<TemplateItem> = list_impl(state)?
        .into_iter()
        .filter(|t| ids.as_ref().map(|v| v.contains(&t.id)).unwrap_or(true))
        .map(|t| TemplateItem { name: t.name, rule_json: t.rule_json })
        .collect();
    items.sort_by(|a, b| a.name.cmp(&b.name));
    let bundle = TemplateBundle {
        schema: SCHEMA_VERSION,
        exported_at: now_ms(),
        templates: items.clone(),
    };
    let text = serde_json::to_string_pretty(&bundle)
        .map_err(|e| AppError::new("TEMPLATE_EXPORT_FAILED", e.to_string()))?;
    fs::write(path, text)?;
    Ok(items.len())
}

#[tauri::command]
pub fn import_templates(state: State<'_, AppState>, path: String) -> AppResult<ImportResult> {
    import_templates_impl(state.inner(), &path)
}

pub fn import_templates_impl(state: &AppState, path: &str) -> AppResult<ImportResult> {
    let text = fs::read_to_string(path)?;
    if text.len() > MAX_IMPORT_BYTES {
        return Err(AppError::new("TEMPLATE_IMPORT_TOO_LARGE", "导入文件过大"));
    }
    let bundle: TemplateBundle = serde_json::from_str(&text)
        .map_err(|e| AppError::new("TEMPLATE_IMPORT_INVALID", format!("文件不是合法模板包：{e}")))?;
    if bundle.schema != SCHEMA_VERSION {
        return Err(AppError::new(
            "TEMPLATE_IMPORT_SCHEMA",
            format!("模板包版本 {} 不受支持（期望 {SCHEMA_VERSION}）", bundle.schema),
        ));
    }
    let mut res = ImportResult { imported: 0, skipped: 0, errors: vec![] };
    for item in bundle.templates {
        match save_impl(state, &item.name, &item.rule_json) {
            Ok(_) => res.imported += 1,
            Err(e) => {
                res.errors.push(format!("{}：{}（{}）", item.name, e.message, e.code));
                res.skipped += 1;
            }
        }
    }
    Ok(res)
}

/// 供后端命令级回归直接驱动（无 GUI / 无 WebView）。
pub fn list_templates_impl(state: &AppState) -> AppResult<Vec<TemplateInfo>> { list_impl(state) }
pub fn save_template_impl(state: &AppState, name: &str, rule_json: &str) -> AppResult<TemplateInfo> {
    save_impl(state, name, rule_json)
}
