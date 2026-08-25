use tauri::State;
use crate::database::pool::ConnectionPoolManager;
use crate::database::returns_rows;
use crate::models::*;

#[tauri::command]
pub async fn query_execute(
    connection_id: String,
    sql: String,
    database: Option<String>,
    query_id: Option<u64>,
    // 为 true 时记录到查询历史（仅编辑器手动执行传 true，内部加载不传）
    record: Option<bool>,
    manager: State<'_, ConnectionPoolManager>,
) -> Result<QueryResult, String> {
    let driver = manager.get_connection(&connection_id).await
        .map_err(|e| e.to_string())?;

    // 语句类型用于日志（SELECT/INSERT/…），不记完整 SQL，避免敏感数据进日志
    let sql_kind = sql.split_whitespace().next().unwrap_or("").to_uppercase();

    let result = if returns_rows(&sql) {
        driver.query(&sql, database.as_deref(), query_id).await
    } else {
        driver.execute(&sql, database.as_deref(), query_id).await
    };

    match result {
        Ok(r) => {
            if r.execution_time_ms > 2000 {
                tracing::warn!(
                    conn = %connection_id, db = ?database, kind = %sql_kind,
                    ms = r.execution_time_ms, "慢查询"
                );
            } else {
                tracing::debug!(
                    conn = %connection_id, db = ?database, kind = %sql_kind,
                    ms = r.execution_time_ms, rows_affected = ?r.rows_affected,
                    truncated = r.truncated, "查询完成"
                );
            }
            if record == Some(true) {
                manager.record_history(&connection_id, database.as_deref(), &sql, &r);
            }
            Ok(r)
        }
        Err(e) => {
            tracing::error!(
                conn = %connection_id, db = ?database, kind = %sql_kind,
                error = %e, "查询失败"
            );
            Err(e.to_string())
        }
    }
}

#[tauri::command]
pub async fn query_cancel(
    connection_id: String,
    query_id: u64,
    manager: State<'_, ConnectionPoolManager>,
) -> Result<(), String> {
    let driver = manager.get_connection(&connection_id).await
        .map_err(|e| e.to_string())?;
    driver.cancel_query(query_id).await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn query_execute_batch(
    connection_id: String,
    database: Option<String>,
    statements: Vec<String>,
    manager: State<'_, ConnectionPoolManager>,
) -> Result<u64, String> {
    let driver = manager.get_connection(&connection_id).await
        .map_err(|e| e.to_string())?;
    driver.execute_batch(statements, database.as_deref()).await.map_err(|e| e.to_string())
}

/// 整表导出 CSV，返回导出行数
#[tauri::command]
pub async fn table_export_csv(
    connection_id: String,
    database: Option<String>,
    table: String,
    path: String,
    manager: State<'_, ConnectionPoolManager>,
) -> Result<u64, String> {
    let driver = manager.get_connection(&connection_id).await
        .map_err(|e| e.to_string())?;
    driver.export_table_csv(database.as_deref(), &table, &path).await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn schema_get_databases(
    connection_id: String,
    manager: State<'_, ConnectionPoolManager>,
) -> Result<Vec<String>, String> {
    let driver = manager.get_connection(&connection_id).await
        .map_err(|e| e.to_string())?;
    driver.get_databases().await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn schema_get_tables(
    connection_id: String,
    database: Option<String>,
    manager: State<'_, ConnectionPoolManager>,
) -> Result<Vec<TableInfo>, String> {
    let driver = manager.get_connection(&connection_id).await
        .map_err(|e| e.to_string())?;
    driver.get_tables(database.as_deref()).await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn schema_get_columns(
    connection_id: String,
    database: Option<String>,
    table: String,
    manager: State<'_, ConnectionPoolManager>,
) -> Result<Vec<ColumnInfo>, String> {
    let driver = manager.get_connection(&connection_id).await
        .map_err(|e| e.to_string())?;
    driver.get_columns(database.as_deref(), &table).await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn schema_get_indexes(
    connection_id: String,
    database: Option<String>,
    table: String,
    manager: State<'_, ConnectionPoolManager>,
) -> Result<Vec<IndexInfo>, String> {
    let driver = manager.get_connection(&connection_id).await
        .map_err(|e| e.to_string())?;
    driver.get_indexes(database.as_deref(), &table).await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn schema_get_foreign_keys(
    connection_id: String,
    database: Option<String>,
    table: String,
    manager: State<'_, ConnectionPoolManager>,
) -> Result<Vec<ForeignKeyInfo>, String> {
    let driver = manager.get_connection(&connection_id).await
        .map_err(|e| e.to_string())?;
    driver.get_foreign_keys(database.as_deref(), &table).await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn schema_get_views(
    connection_id: String,
    database: Option<String>,
    manager: State<'_, ConnectionPoolManager>,
) -> Result<Vec<TableInfo>, String> {
    let driver = manager.get_connection(&connection_id).await
        .map_err(|e| e.to_string())?;
    driver.get_views(database.as_deref()).await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn schema_get_view_definition(
    connection_id: String,
    database: Option<String>,
    view: String,
    manager: State<'_, ConnectionPoolManager>,
) -> Result<String, String> {
    let driver = manager.get_connection(&connection_id).await
        .map_err(|e| e.to_string())?;
    driver.get_view_definition(database.as_deref(), &view).await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn schema_get_triggers(
    connection_id: String,
    database: Option<String>,
    manager: State<'_, ConnectionPoolManager>,
) -> Result<Vec<TableInfo>, String> {
    let driver = manager.get_connection(&connection_id).await
        .map_err(|e| e.to_string())?;
    driver.get_triggers(database.as_deref()).await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn schema_get_trigger_definition(
    connection_id: String,
    database: Option<String>,
    trigger: String,
    manager: State<'_, ConnectionPoolManager>,
) -> Result<String, String> {
    let driver = manager.get_connection(&connection_id).await
        .map_err(|e| e.to_string())?;
    driver.get_trigger_definition(database.as_deref(), &trigger).await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn schema_get_procedures(
    connection_id: String,
    database: Option<String>,
    manager: State<'_, ConnectionPoolManager>,
) -> Result<Vec<TableInfo>, String> {
    let driver = manager.get_connection(&connection_id).await
        .map_err(|e| e.to_string())?;
    driver.get_procedures(database.as_deref()).await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn schema_get_procedure_definition(
    connection_id: String,
    database: Option<String>,
    procedure: String,
    manager: State<'_, ConnectionPoolManager>,
) -> Result<String, String> {
    let driver = manager.get_connection(&connection_id).await
        .map_err(|e| e.to_string())?;
    driver.get_procedure_definition(database.as_deref(), &procedure).await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn schema_get_functions(
    connection_id: String,
    database: Option<String>,
    manager: State<'_, ConnectionPoolManager>,
) -> Result<Vec<TableInfo>, String> {
    let driver = manager.get_connection(&connection_id).await
        .map_err(|e| e.to_string())?;
    driver.get_functions(database.as_deref()).await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn schema_get_function_definition(
    connection_id: String,
    database: Option<String>,
    function: String,
    manager: State<'_, ConnectionPoolManager>,
) -> Result<String, String> {
    let driver = manager.get_connection(&connection_id).await
        .map_err(|e| e.to_string())?;
    driver.get_function_definition(database.as_deref(), &function).await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn schema_get_users(
    connection_id: String,
    database: Option<String>,
    manager: State<'_, ConnectionPoolManager>,
) -> Result<Vec<DbUserInfo>, String> {
    let driver = manager.get_connection(&connection_id).await
        .map_err(|e| e.to_string())?;
    driver.get_users(database.as_deref()).await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn schema_get_current_user(
    connection_id: String,
    database: Option<String>,
    manager: State<'_, ConnectionPoolManager>,
) -> Result<CurrentUserInfo, String> {
    let driver = manager.get_connection(&connection_id).await
        .map_err(|e| e.to_string())?;
    driver.get_current_user(database.as_deref()).await.map_err(|e| e.to_string())
}
