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

    let result = if returns_rows(&sql) {
        driver.query(&sql, database.as_deref(), query_id).await
    } else {
        driver.execute(&sql, database.as_deref(), query_id).await
    }
    .map_err(|e| e.to_string())?;

    if record == Some(true) {
        manager.record_history(&connection_id, database.as_deref(), &sql, &result);
    }
    Ok(result)
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
