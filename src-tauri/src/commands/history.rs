use crate::database::pool::ConnectionPoolManager;
use crate::models::*;
use tauri::State;

/// 查询历史：按连接列出（最新在前）
#[tauri::command]
pub async fn history_list(
    connection_id: String,
    limit: Option<usize>,
    manager: State<'_, ConnectionPoolManager>,
) -> Result<Vec<HistoryEntry>, String> {
    Ok(manager.list_history(&connection_id, limit.unwrap_or(100)))
}

/// 删除单条历史
#[tauri::command]
pub async fn history_delete(
    id: String,
    manager: State<'_, ConnectionPoolManager>,
) -> Result<(), String> {
    manager.delete_history(&id).map_err(|e| e.to_string())
}

/// 清空某连接的全部历史
#[tauri::command]
pub async fn history_clear(
    connection_id: String,
    manager: State<'_, ConnectionPoolManager>,
) -> Result<(), String> {
    manager
        .clear_history(&connection_id)
        .map_err(|e| e.to_string())
}
