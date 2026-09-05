//! MCP 设置的持久化与进程内共享。
//! 文件: {app_config_dir}/mcp-settings.json
//! 连接级访问控制不在本文件：存于 connections.json 每个连接的 mcpAllowed 字段。

use std::{path::PathBuf, sync::{Arc, RwLock}};

use serde::{Deserialize, Serialize};

/// 默认监听端口；仅绑定 127.0.0.1。
pub const DEFAULT_MCP_PORT: u16 = 48123;

/// MCP 服务设置。所有字段带缺省值，旧配置文件或缺失字段均可正常加载。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct McpSettings {
    /// 是否启用 MCP 服务（停用后不监听任何端口）
    pub enabled: bool,
    /// 监听端口；SQLINK_MCP_PORT 环境变量优先于该值
    pub port: u16,
    /// 被禁用的工具名；禁用后 tools/list 不返回且调用报错
    pub disabled_tools: Vec<String>,
}

impl Default for McpSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            port: DEFAULT_MCP_PORT,
            disabled_tools: Vec::new(),
        }
    }
}

impl McpSettings {
    fn path(config_dir: &std::path::Path) -> PathBuf {
        config_dir.join("mcp-settings.json")
    }

    /// 读取设置；文件缺失或损坏时回退默认值（不视为致命错误）。
    pub fn load(config_dir: &std::path::Path) -> Self {
        let path = Self::path(config_dir);
        match std::fs::read_to_string(&path) {
            Ok(content) => match serde_json::from_str::<McpSettings>(&content) {
                Ok(settings) => settings,
                Err(error) => {
                    tracing::warn!(error = %error, path = %path.display(), "MCP 设置解析失败，使用默认设置");
                    Self::default()
                }
            },
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self, config_dir: &std::path::Path) -> Result<(), String> {
        if self.enabled && self.port == 0 {
            return Err("MCP 端口不能为 0".into());
        }
        let path = Self::path(config_dir);
        std::fs::create_dir_all(config_dir).map_err(|e| format!("创建配置目录失败: {e}"))?;
        std::fs::write(&path, serde_json::to_vec_pretty(self).map_err(|e| e.to_string())?)
            .map_err(|e| format!("写入 MCP 设置失败: {e}"))
    }
}

/// 进程内共享的 MCP 设置。写少读多：MCP 工厂每个请求读取一次快照，
/// 因此工具开关与连接白名单的变更无需重启服务即可生效。
#[derive(Clone)]
pub struct McpSettingsState {
    inner: Arc<RwLock<Arc<McpSettings>>>,
    config_dir: PathBuf,
}

impl McpSettingsState {
    pub fn load(config_dir: PathBuf) -> Self {
        let settings = McpSettings::load(&config_dir);
        Self {
            inner: Arc::new(RwLock::new(Arc::new(settings))),
            config_dir,
        }
    }

    /// 当前设置快照（Arc，读取廉价）
    pub fn get(&self) -> Arc<McpSettings> {
        self.inner
            .read()
            .expect("MCP 设置锁不可重入且不会中毒")
            .clone()
    }

    /// 校验并持久化新设置，成功后替换进程内快照。
    pub fn replace(&self, settings: McpSettings) -> Result<(), String> {
        settings.save(&self.config_dir)?;
        *self.inner.write().expect("MCP 设置锁不可重入且不会中毒") = Arc::new(settings);
        Ok(())
    }

    pub fn config_dir(&self) -> &std::path::Path {
        &self.config_dir
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_roundtrip_and_defaults() {
        let dir = std::env::temp_dir().join(format!("sqlink-mcp-settings-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();

        // 文件缺失 → 默认值
        let loaded = McpSettings::load(&dir);
        assert!(loaded.enabled);
        assert_eq!(loaded.port, DEFAULT_MCP_PORT);
        assert!(loaded.disabled_tools.is_empty());

        // 保存后可读回
        let settings = McpSettings {
            enabled: false,
            port: 48999,
            disabled_tools: vec!["execute_readonly_sql".into()],
        };
        settings.save(&dir).unwrap();
        let loaded = McpSettings::load(&dir);
        assert!(!loaded.enabled);
        assert_eq!(loaded.port, 48999);
        assert_eq!(loaded.disabled_tools, vec!["execute_readonly_sql".to_string()]);

        // 损坏文件 → 回退默认值
        std::fs::write(dir.join("mcp-settings.json"), "not json").unwrap();
        assert!(McpSettings::load(&dir).enabled);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn save_rejects_zero_port() {
        let dir = std::env::temp_dir().join(format!("sqlink-mcp-settings-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let err = McpSettings { enabled: true, port: 0, disabled_tools: vec![] }.save(&dir);
        assert!(err.is_err());
        std::fs::remove_dir_all(&dir).ok();
    }
}
