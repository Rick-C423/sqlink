use crate::database::pool::ConnectionPoolManager;
use crate::database::test_connection as db_test_connection;
use crate::models::ConnectionConfig;
use tauri::State;

#[tauri::command]
pub async fn connection_test(config: ConnectionConfig) -> Result<(), String> {
    db_test_connection(&config).await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn connection_create(
    config: ConnectionConfig,
    manager: State<'_, ConnectionPoolManager>,
) -> Result<String, String> {
    manager
        .create_connection(config)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn connection_disconnect(
    connection_id: String,
    manager: State<'_, ConnectionPoolManager>,
) -> Result<(), String> {
    manager
        .disconnect(&connection_id)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn connection_list(
    manager: State<'_, ConnectionPoolManager>,
) -> Result<Vec<ConnectionConfig>, String> {
    let connections = manager.list_connections().await;
    // 返回给前端时掩码密码，避免明文密码离开后端
    Ok(connections
        .into_iter()
        .map(|(_, mut config)| {
            config.password = String::new();
            config
        })
        .collect())
}

/// 保存连接配置（密码加密落盘）并建立活动连接
#[tauri::command]
pub async fn connection_save(
    config: ConnectionConfig,
    manager: State<'_, ConnectionPoolManager>,
) -> Result<String, String> {
    manager
        .save_and_connect(config)
        .await
        .map_err(|e| e.to_string())
}

/// 已保存连接列表（密码掩码为空）
#[tauri::command]
pub async fn saved_connections_list(
    manager: State<'_, ConnectionPoolManager>,
) -> Result<Vec<ConnectionConfig>, String> {
    manager.list_saved().map_err(|e| e.to_string())
}

/// 用已保存配置建立活动连接（密码仅在后端解密）
#[tauri::command]
pub async fn connection_connect_saved(
    id: String,
    manager: State<'_, ConnectionPoolManager>,
) -> Result<String, String> {
    manager.connect_saved(&id).await.map_err(|e| e.to_string())
}

/// 删除已保存连接（同时断开活动连接）
#[tauri::command]
pub async fn connection_delete_saved(
    id: String,
    manager: State<'_, ConnectionPoolManager>,
) -> Result<(), String> {
    manager.delete_saved(&id).await.map_err(|e| e.to_string())
}

/// 更新已保存连接（密码留空则保留原密码，更新后断开活动连接）
#[tauri::command]
pub async fn connection_update(
    config: ConnectionConfig,
    manager: State<'_, ConnectionPoolManager>,
) -> Result<(), String> {
    manager
        .update_saved(config)
        .await
        .map_err(|e| e.to_string())
}
