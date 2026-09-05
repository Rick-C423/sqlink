//! MCP 设置的读取、保存（热应用）与连接访问开关。

use serde::Serialize;
use tauri::State;

use crate::database::pool::ConnectionPoolManager;
use crate::mcp::{McpRunStatus, McpSupervisor, MCP_TOOLS};
use crate::mcp_settings::{McpSettings, McpSettingsState};

/// 设置 + 运行状态快照：前端设置对话框一次获取、保存后回显。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpSettingsSnapshot {
    pub enabled: bool,
    pub port: u16,
    pub disabled_tools: Vec<String>,
    pub running: bool,
    pub url: Option<String>,
    /// 全部可用工具名（渲染工具开关列表用）
    pub tools: Vec<&'static str>,
}

fn snapshot(settings: &McpSettings, status: &McpRunStatus) -> McpSettingsSnapshot {
    McpSettingsSnapshot {
        enabled: settings.enabled,
        port: settings.port,
        disabled_tools: settings.disabled_tools.clone(),
        running: status.running,
        url: status.url.clone(),
        tools: MCP_TOOLS.to_vec(),
    }
}

#[tauri::command]
pub async fn mcp_settings_get(
    supervisor: State<'_, McpSupervisor>,
    settings: State<'_, McpSettingsState>,
) -> Result<McpSettingsSnapshot, String> {
    Ok(snapshot(&settings.get(), &supervisor.status()))
}

/// 保存设置并热应用：停用 → 停止服务；启用 → 停旧实例后按新端口重启。
/// 工具开关无需重启（每个请求按最新快照生效）。
#[tauri::command]
pub async fn mcp_settings_save(
    mut new_settings: McpSettings,
    supervisor: State<'_, McpSupervisor>,
    settings: State<'_, McpSettingsState>,
    manager: State<'_, ConnectionPoolManager>,
) -> Result<McpSettingsSnapshot, String> {
    new_settings
        .disabled_tools
        .retain(|name| MCP_TOOLS.contains(&name.as_str()));
    settings.replace(new_settings)?;
    supervisor
        .apply(&settings.get(), &settings, &manager)
        .await?;
    Ok(snapshot(&settings.get(), &supervisor.status()))
}

/// 更新单个已保存连接的 MCP 访问开关（写 connections.json，不断开活动连接）。
#[tauri::command]
pub async fn mcp_set_connection_allowed(
    connection_id: String,
    allowed: bool,
    manager: State<'_, ConnectionPoolManager>,
) -> Result<(), String> {
    manager
        .set_connection_mcp_allowed(&connection_id, allowed)
        .map_err(|e| e.to_string())
}
