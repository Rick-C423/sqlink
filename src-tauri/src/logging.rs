//! 日志体系：tracing 按天滚动落盘 + 控制台双输出
//!
//! - 文件位置：Tauri `app_log_dir()`（Windows 为 `%APPDATA%\com.sqlconnect.app\logs`）
//! - 文件命名：`sqlink.log.YYYY-MM-DD`，按天滚动
//! - 自动清理：启动时删除 `RETENTION_DAYS` 天前的旧日志
//! - 级别控制：环境变量 `SQLINK_LOG`（如 `SQLINK_LOG=debug`），默认 `info`、本 crate `debug`
//!
//! 红线：密码、完整连接串绝不进日志，只记 host/port/库名等元信息。

use std::path::Path;
use std::time::{Duration, SystemTime};

use tracing_subscriber::fmt;
use tracing_subscriber::prelude::*;
use tracing_subscriber::EnvFilter;

/// 旧日志保留天数
const RETENTION_DAYS: u64 = 7;

/// 持有期间保证日志线程存活并在退出前冲刷；必须 manage 进 Tauri 状态
/// （字段不被读取是正常的——它的价值在于 Drop 时的冲刷行为）
#[allow(dead_code)]
pub struct LogGuard(tracing_appender::non_blocking::WorkerGuard);

/// 初始化全局日志。返回的 guard 需持有到应用退出，否则末尾日志可能丢失。
pub fn init(log_dir: &Path) -> Option<LogGuard> {
    if let Err(e) = std::fs::create_dir_all(log_dir) {
        eprintln!("创建日志目录失败 {}: {}", log_dir.display(), e);
        return None;
    }
    cleanup_old_logs(log_dir);

    let file_appender = tracing_appender::rolling::daily(log_dir, "sqlink.log");
    let (file_writer, guard) = tracing_appender::non_blocking(file_appender);

    let filter = EnvFilter::try_from_env("SQLINK_LOG")
        .unwrap_or_else(|_| EnvFilter::new("info,sqlconnect=debug"));

    let file_layer = fmt::layer()
        .with_writer(file_writer)
        .with_ansi(false)
        .with_target(true);
    let stdout_layer = fmt::layer().with_writer(std::io::stdout).with_target(false);

    tracing_subscriber::registry()
        .with(filter)
        .with(file_layer)
        .with(stdout_layer)
        .init();

    Some(LogGuard(guard))
}

/// 清理 N 天前的滚动日志文件（按文件修改时间判断）
fn cleanup_old_logs(log_dir: &Path) {
    let cutoff = SystemTime::now() - Duration::from_secs(RETENTION_DAYS * 24 * 3600);
    let Ok(entries) = std::fs::read_dir(log_dir) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // 只清理本应用生成的滚动文件：sqlink.log.YYYY-MM-DD[.gz]
        if !(name.starts_with("sqlink.log.")) {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let Ok(mtime) = meta.modified() else { continue };
        if mtime < cutoff {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// 在系统文件管理器中打开日志目录（帮助菜单「打开日志目录」）
#[tauri::command]
pub fn open_log_folder(app: tauri::AppHandle) -> Result<(), String> {
    use tauri::Manager;
    use tauri_plugin_shell::ShellExt;

    let dir = app.path().app_log_dir().map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    app.shell()
        .open(dir.to_string_lossy(), None)
        .map_err(|e| e.to_string())
}
