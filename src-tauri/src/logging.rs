//! 运行日志落盘（界面不再展示流水日志）。
//! 位置：优先放在程序所在目录（免安装版 = exe 同级；安装版 = 安装目录），不写入系统盘用户数据目录。
//! 该目录不可写（如安装到只读目录）时回退到 app_data_dir。
//! 单文件追加 + 超过 1 MiB 时轮转一份 grename.1.log，避免长期无限增长。

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

use tauri::{AppHandle, Manager};

use crate::model::{AppError, AppResult};

const MAX_LOG_BYTES: u64 = 1024 * 1024;

pub fn log_path(app: &AppHandle) -> AppResult<PathBuf> {
    // 优先放在程序所在目录（exe 同级）：免安装版即软件文件夹，安装版即安装目录。
    // 不写入系统盘用户数据目录（如 AppData），避免与程序分离、重装即丢失。
    if let Some(exe) = std::env::current_exe().ok() {
        if let Some(dir) = exe.parent() {
            if fs::create_dir_all(dir).is_ok() {
                return Ok(dir.join("grename.log"));
            }
        }
    }
    // 回退：程序目录不可写（如安装到只读目录）时，仍保证日志可落盘。
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| AppError::new("PATH_ERROR", e.to_string()))?;
    fs::create_dir_all(&dir)?;
    Ok(dir.join("grename.log"))
}

pub fn append(app: &AppHandle, line: &str) -> AppResult<()> {
    let path = log_path(app)?;
    if let Ok(meta) = fs::metadata(&path) {
        if meta.len() > MAX_LOG_BYTES {
            let old = path.with_extension("1.log");
            let _ = fs::remove_file(&old);
            let _ = fs::rename(&path, &old);
        }
    }
    let mut f = OpenOptions::new().create(true).append(true).open(&path)?;
    writeln!(f, "{line}")?;
    Ok(())
}

/// 打开前确保文件存在，否则系统默认程序会提示找不到文件。
pub fn ensure_file(app: &AppHandle) -> AppResult<PathBuf> {
    let path = log_path(app)?;
    if !path.exists() {
        OpenOptions::new().create(true).append(true).open(&path)?;
    }
    Ok(path)
}
