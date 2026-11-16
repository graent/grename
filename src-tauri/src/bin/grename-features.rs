//! v1.15 P1 功能回归：自定义规则模板（§6.7）+ 暂停标志语义。
//! 无 GUI / 无 WebView，直接驱动 *_impl 纯函数；输出 FEATURES_JSON={...}。
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;

use serde_json::json;
use tauri_app_lib::db::Db;
use tauri_app_lib::state::{AppState, JobRuntime};

fn arg(name: &str) -> String {
    let key = format!("--{name}");
    let a: Vec<String> = env::args().collect();
    a.iter().position(|x| x == &key)
        .and_then(|i| a.get(i + 1)).cloned()
        .unwrap_or_default()
}

fn code_of<T>(r: Result<T, tauri_app_lib::model::AppError>) -> String {
    r.err().map(|e| e.code).unwrap_or_default()
}

struct Rep { checks: Vec<(String, bool, String)> }
impl Rep {
    fn ck(&mut self, n: &str, ok: bool, d: impl Into<String>) {
        self.checks.push((n.to_string(), ok, d.into()));
    }
}

fn main() {
    let mut rep = Rep { checks: vec![] };
    let dir = PathBuf::from(arg("dir"));
    fs::create_dir_all(&dir).expect("建临时目录失败");
    let db_path = dir.join("features.db");
    let state = AppState::new(Db::open(&db_path).expect("建库失败"));
    let bundle = dir.join("templates.json");

    let rule = r#"{"pattern":"","replacement":"ren_$0","scope":"name","outputMode":"in_place"}"#;
    let rule2 = r#"{"pattern":"\\s+","replacement":"_","scope":"full","outputMode":"to_dir"}"#;

    // --- 模板 CRUD ---
    let t1 = templates_save(&state, "常用前缀", rule);
    rep.ck("tpl.save", t1.is_ok(), format!("{:?}", t1.as_ref().map(|x| &x.id)));
    let id1 = t1.as_ref().map(|x| x.id.clone()).unwrap_or_default();
    let list1 = tauri_app_lib::templates::list_templates_impl(&state).unwrap_or_default();
    rep.ck("tpl.list", list1.len() == 1 && list1[0].name == "常用前缀",
           format!("len={}", list1.len()));

    let t2 = templates_save(&state, "常用前缀", rule2);
    let list2 = tauri_app_lib::templates::list_templates_impl(&state).unwrap_or_default();
    rep.ck("tpl.upsert_same_name",
           t2.is_ok() && t2.as_ref().map(|x| x.id.as_str()) == Some(id1.as_str())
               && list2.len() == 1 && list2[0].rule_json.contains("ren_") == false,
           format!("len={} sameId={}", list2.len(),
                   t2.as_ref().map(|x| x.id.as_str()) == Some(id1.as_str())));

    templates_save(&state, "空白转下划线", rule2).expect("保存第二个模板失败");
    let list3 = tauri_app_lib::templates::list_templates_impl(&state).unwrap_or_default();
    rep.ck("tpl.multi", list3.len() == 2, format!("len={}", list3.len()));

    // --- 安全闸门 ---
    let bad_path = r#"{"pattern":"","replacement":"x","outputDir":"C://tmp//out"}"#;
    rep.ck("tpl.reject_absolute_path",
           code_of(templates_save(&state, "带路径", bad_path)) == "TEMPLATE_ABSOLUTE_PATH",
           code_of(templates_save(&state, "带路径", bad_path)));
    rep.ck("tpl.reject_bad_json",
           code_of(templates_save(&state, "坏 JSON", "{not-json")) == "TEMPLATE_RULE_INVALID",
           code_of(templates_save(&state, "坏 JSON", "{not-json")));
    rep.ck("tpl.reject_empty_name",
           code_of(templates_save(&state, "  ", rule)) == "TEMPLATE_NAME_INVALID",
           code_of(templates_save(&state, "  ", rule)));

    // --- 导出 / 导入 ---
    let n = tauri_app_lib::templates::export_templates_impl(&state, bundle.to_str().unwrap(), None)
        .unwrap_or(0);
    rep.ck("tpl.export_count", n == 2, format!("n={n}"));
    let text = fs::read_to_string(&bundle).unwrap_or_default();
    rep.ck("tpl.export_no_absolute_path",
           !text.contains("C:////") && !text.contains("outputDir") && !text.contains("rootPath"),
           format!("bytes={}", text.len()));
    rep.ck("tpl.export_schema", text.contains("\"schema\": 1"), "schema=1");

    let d1 = tauri_app_lib::templates::delete_template_impl(&state, &id1).unwrap_or(false);
    rep.ck("tpl.delete", d1, format!("deleted={d1}"));
    let d2 = tauri_app_lib::templates::delete_template_impl(&state, &id1).unwrap_or(true);
    rep.ck("tpl.delete_idempotent", !d2, format!("second={d2}"));

    let imp = tauri_app_lib::templates::import_templates_impl(&state, bundle.to_str().unwrap());
    rep.ck("tpl.import",
           imp.as_ref().map(|x| x.imported == 2 && x.skipped == 0).unwrap_or(false),
           format!("{:?}", imp.as_ref().map(|x| (x.imported, x.skipped))));

    let bad_bundle = dir.join("bad.json");
    fs::write(&bad_bundle, r#"{"schema":99,"exportedAt":0,"templates":[]}"#).unwrap();
    rep.ck("tpl.import_bad_schema",
           code_of(tauri_app_lib::templates::import_templates_impl(&state, bad_bundle.to_str().unwrap()))
               == "TEMPLATE_IMPORT_SCHEMA", "schema 99 应被拒绝");

    let mixed = dir.join("mixed.json");
    fs::write(&mixed, json!({
        "schema": 1, "exportedAt": 0,
        "templates": [
            {"name":"合法", "ruleJson": rule},
            {"name":"非法路径", "ruleJson": bad_path}
        ]
    }).to_string()).unwrap();
    let mi = tauri_app_lib::templates::import_templates_impl(&state, mixed.to_str().unwrap());
    rep.ck("tpl.import_skips_invalid",
           mi.as_ref().map(|x| x.imported == 1 && x.skipped == 1 && x.errors.len() == 1)
               .unwrap_or(false),
           format!("{:?}", mi.as_ref().map(|x| (x.imported, x.skipped, x.errors.len()))));

    // --- 暂停标志语义 ---
    let rt = JobRuntime::new();
    rep.ck("pause.default_false", !rt.is_paused() && !rt.is_cancelled(), "初始未暂停未取消");
    rep.ck("pause.wait_unpaused_returns_true", rt.wait_while_paused(), "未暂停应立即返回 true");
    rt.paused.store(true, Ordering::Relaxed);
    rep.ck("pause.flag_set", rt.is_paused(), "置位后可读");
    let c = rt.clone();
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(120));
        c.paused.store(false, Ordering::Relaxed);
    });
    let ok = rt.wait_while_paused();
    rep.ck("pause.resume_unblocks", ok && !rt.is_paused(), format!("wait={ok}"));

    rt.paused.store(true, Ordering::Relaxed);
    let c2 = rt.clone();
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(120));
        c2.cancel.store(true, Ordering::Relaxed);
    });
    let cancelled = rt.wait_while_paused();
    rep.ck("pause.cancel_during_pause_returns_false", !cancelled,
           format!("wait={cancelled}（应让执行循环跳出）"));

    let passed = rep.checks.iter().filter(|x| x.1).count();
    let out = json!({
        "module": "grename-features",
        "passed": passed, "total": rep.checks.len(),
        "checks": rep.checks.iter().map(|(n, ok, d)| json!({"name": n, "ok": ok, "detail": d}))
                      .collect::<Vec<_>>(),
    });
    println!("FEATURES_JSON={}", out);
    if passed != rep.checks.len() { std::process::exit(1); }
}

fn templates_save(state: &AppState, name: &str, rule_json: &str)
    -> Result<tauri_app_lib::templates::TemplateInfo, tauri_app_lib::model::AppError> {
    tauri_app_lib::templates::save_template_impl(state, name, rule_json)
}
