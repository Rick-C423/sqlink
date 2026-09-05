pub mod commands;
pub mod database;
pub mod history;
pub mod logging;
pub mod mcp;
pub mod mcp_settings;
pub mod models;
pub mod store;

use tauri::Manager;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            // 连接管理
            commands::connection::connection_test,
            commands::connection::connection_create,
            commands::connection::connection_disconnect,
            commands::connection::connection_list,
            // 连接持久化
            commands::connection::connection_save,
            commands::connection::saved_connections_list,
            commands::connection::connection_connect_saved,
            commands::connection::connection_delete_saved,
            commands::connection::connection_update,
            // 查询
            commands::query::query_execute,
            commands::query::query_cancel,
            commands::query::query_execute_batch,
            commands::query::table_export_csv,
            // Schema
            commands::query::schema_get_databases,
            commands::query::schema_get_database_infos,
            commands::query::schema_get_tables,
            commands::query::schema_get_columns,
            commands::query::schema_get_indexes,
            commands::query::schema_get_foreign_keys,
            commands::query::schema_get_views,
            commands::query::schema_get_view_definition,
            commands::query::schema_get_triggers,
            commands::query::schema_get_trigger_definition,
            commands::query::schema_get_procedures,
            commands::query::schema_get_procedure_definition,
            commands::query::schema_get_functions,
            commands::query::schema_get_function_definition,
            commands::query::schema_get_users,
            commands::query::schema_get_current_user,
            // SQL 转储导入
            commands::import::sql_import_file,
            commands::import::sql_dump_file,
            // 查询历史
            commands::history::history_list,
            commands::history::history_delete,
            commands::history::history_clear,
            // 窗口主题
            commands::window::set_window_theme,
            // 日志
            logging::open_log_folder,
            // MCP 设置
            commands::mcp::mcp_settings_get,
            commands::mcp::mcp_settings_save,
            commands::mcp::mcp_set_connection_allowed,
        ])
        .setup(|app| {
            // 日志初始化要最先做：后续初始化过程的失败才能进日志
            if let Ok(log_dir) = app.path().app_log_dir() {
                if let Some(guard) = logging::init(&log_dir) {
                    app.manage(guard);
                    tracing::info!(
                        version = env!("CARGO_PKG_VERSION"),
                        log_dir = %log_dir.display(),
                        "SQLink 启动"
                    );
                }
            }
            // 初始化数据库连接池管理器（含本地持久化存储）
            let config_dir = app.path().app_config_dir().ok();
            let manager = database::pool::ConnectionPoolManager::new(config_dir.clone());
            app.manage(manager.clone());

            // MCP 仅监听本机回环地址，并以 Bearer Token 保护。
            // 按设置决定是否启动；启动失败不影响桌面客户端的主流程。
            if let Some(config_dir) = config_dir {
                let settings_state = mcp_settings::McpSettingsState::load(config_dir.clone());
                let supervisor = mcp::McpSupervisor::new();
                app.manage(supervisor.clone());
                app.manage(settings_state.clone());
                tauri::async_runtime::spawn(async move {
                    let settings = settings_state.get();
                    if let Err(error) =
                        supervisor.apply(&settings, &settings_state, &manager).await
                    {
                        tracing::error!(error = %error, "MCP 服务启动失败");
                    }
                });
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
