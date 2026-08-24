pub mod commands;
pub mod database;
pub mod history;
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
            commands::query::schema_get_tables,
            commands::query::schema_get_columns,
            commands::query::schema_get_indexes,
            commands::query::schema_get_foreign_keys,
            commands::query::schema_get_views,
            commands::query::schema_get_view_definition,
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
        ])
        .setup(|app| {
            // 初始化数据库连接池管理器（含本地持久化存储）
            let config_dir = app.path().app_config_dir().ok();
            app.manage(database::pool::ConnectionPoolManager::new(config_dir));
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
