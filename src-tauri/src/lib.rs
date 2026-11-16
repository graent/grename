//! Grename M0 —— 海量文件批量重命名的性能验证骨架。
//! 设计文档：docs/Grename-详细需求分析与设计文档.md v1.3
//!
//! 模块划分：
//!   model     前后端共享的数据契约与错误码
//!   db        SQLite 单写连接 + DDL
//!   state     任务运行时、原子计数、取消令牌、80ms 聚合进度 Emitter
//!   scan      jwalk 流式并行扫描 → 批量落库
//!   plan      输出路径计算、输出目录隔离、冲突检测下推 SQLite、挡路者标记
//!   pathpolicy 跨平台路径策略（大小写语义 / 非法字符 / 保留名 / 长度上限）
//!   stage     两阶段临时名：临时名生成与崩溃后处置判定（纯函数）
//!   execute   ① rename（含量阶段让位）/ ② .part→校验→原子改名，双轨进度
//!   undo      v1.3 跨会话撤销：全量复核 + 令牌 + 二次 stat
//!   logging   运行日志追加写 grename.log（界面不展示流水）
//!   commands  Tauri Command 层（游标分页，不返回全量）

mod commands;
mod logging;

// M0 基准采集（--features bench）需要复用真实后端模块，故将其可见性放开。
// 仅改变可见性，不改变任何运行时行为；默认构建产物与之前完全一致。
pub mod db;
pub mod execute;
pub mod maintenance;
pub mod model;
pub mod plan;
pub mod pathpolicy;
pub mod rule;
pub mod scan;
pub mod stage;
pub mod state;
pub mod templates;
pub mod undo;

use tauri::Manager;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            let dir = app
                .path()
                .app_data_dir()
                .unwrap_or_else(|_| std::env::current_dir().unwrap_or_default());
            let db = db::Db::open(&dir.join("grename.db")).expect("grename db init failed");
            let app_state = state::AppState::new(db);
            // v1.3：只收敛非终态任务，不清零历史任务的 undo_available。
            app_state.reconcile_on_startup();
            // M3：启动时按 history_retention_days 清理过期终态历史。
            // 只删数据库记录，绝不触碰文件系统上的产物。
            let _ = maintenance::prune_history(&app_state);
            app.manage(app_state);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::scan_files,
            commands::build_plan,
            commands::preview_rule,
            commands::start_execute,
            commands::cancel_job,
            commands::get_job,
            commands::list_jobs,
            commands::get_entries,
            commands::get_undo_conflicts,
            commands::precheck_undo,
            commands::undo_job,
            commands::append_log,
            commands::get_log_path,
            commands::open_log_file,
            maintenance::get_settings,
            maintenance::set_settings,
            maintenance::prune_history_cmd,
            maintenance::resume_interrupted,
            maintenance::export_report,
            templates::list_templates,
            templates::save_template,
            templates::delete_template,
            templates::export_templates,
            templates::import_templates,
            commands::pause_job,
            commands::resume_job,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
