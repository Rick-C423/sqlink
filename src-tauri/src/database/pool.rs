use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;
use uuid::Uuid;
use crate::database::{DatabaseDriver, create_and_connect_driver};
use crate::history::HistoryStore;
use crate::models::*;
use crate::store::ConnectionStore;

pub struct ConnectionPoolManager {
    connections: Arc<RwLock<HashMap<String, Arc<dyn DatabaseDriver>>>>,
    pub(crate) configs: Arc<RwLock<HashMap<String, ConnectionConfig>>>,
    store: Option<ConnectionStore>,
    history: Option<HistoryStore>,
}

impl ConnectionPoolManager {
    pub fn new(config_dir: Option<PathBuf>) -> Self {
        let history = config_dir.as_ref().map(|dir| HistoryStore::new(dir.clone()));
        let store = config_dir.and_then(|dir| {
            match ConnectionStore::new(dir) {
                Ok(s) => Some(s),
                Err(e) => {
                    eprintln!("连接存储初始化失败（持久化不可用）: {}", e);
                    None
                }
            }
        });
        Self {
            connections: Arc::new(RwLock::new(HashMap::new())),
            configs: Arc::new(RwLock::new(HashMap::new())),
            store,
            history,
        }
    }

    pub async fn create_connection(&self, config: ConnectionConfig) -> Result<String, DatabaseError> {
        let id = config.id.clone().unwrap_or_else(|| Uuid::new_v4().to_string());

        if self.connections.read().await.contains_key(&id) {
            self.disconnect(&id).await.ok();
        }

        let driver = create_and_connect_driver(&config).await?;

        self.configs.write().await.insert(id.clone(), config);
        self.connections.write().await.insert(id.clone(), driver);

        Ok(id)
    }

    pub async fn get_connection(&self, id: &str) -> Result<Arc<dyn DatabaseDriver>, DatabaseError> {
        // 快路径：已有活动连接
        if let Some(d) = self.connections.read().await.get(id).cloned() {
            return Ok(d);
        }
        // 活动连接不存在（应用重启 / 被服务端断开）：用已保存配置自动重连，
        // 避免向前端抛"连接已关闭"——用户感知不到中断
        let config = self.store.as_ref()
            .and_then(|s| s.get_decrypted(id).ok().flatten())
            .ok_or(DatabaseError::ConnectionClosed)?;
        let driver = create_and_connect_driver(&config).await?;
        self.configs.write().await.insert(id.to_string(), config);
        self.connections.write().await.insert(id.to_string(), driver.clone());
        Ok(driver)
    }

    pub async fn disconnect(&self, id: &str) -> Result<(), DatabaseError> {
        let driver = self.connections.write().await.remove(id);
        self.configs.write().await.remove(id);
        // 显式关闭底层连接池，避免 TCP 连接泄漏
        if let Some(driver) = driver {
            driver.disconnect().await?;
        }
        Ok(())
    }

    pub async fn list_connections(&self) -> Vec<(String, ConnectionConfig)> {
        let configs = self.configs.read().await;
        configs.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
    }

    // ---------- 持久化 ----------

    fn require_store(&self) -> Result<&ConnectionStore, DatabaseError> {
        self.store.as_ref()
            .ok_or_else(|| DatabaseError::Unknown("连接持久化存储不可用".into()))
    }

    /// 保存连接配置到磁盘（密码加密），并按保存的 id 建立活动连接
    pub async fn save_and_connect(&self, mut config: ConnectionConfig) -> Result<String, DatabaseError> {
        if config.id.is_none() {
            config.id = Some(Uuid::new_v4().to_string());
        }
        self.require_store()?.save(&config)?;
        self.create_connection(config).await
    }

    /// 已保存连接列表（密码掩码）
    pub fn list_saved(&self) -> Result<Vec<ConnectionConfig>, DatabaseError> {
        self.require_store()?.list()
    }

    /// 用已保存的配置建立活动连接（密码在后端解密，不下发前端）
    pub async fn connect_saved(&self, id: &str) -> Result<String, DatabaseError> {
        let config = self.require_store()?.get_decrypted(id)?
            .ok_or_else(|| DatabaseError::Unknown(format!("连接不存在: {}", id)))?;
        self.create_connection(config).await
    }

    /// 删除已保存连接（同时断开活动连接）
    pub async fn delete_saved(&self, id: &str) -> Result<(), DatabaseError> {
        self.disconnect(id).await.ok();
        self.require_store()?.delete(id)
    }

    /// 更新已保存连接：密码留空则保留原密码；更新后断开活动连接（前端需重新点击连接）
    pub async fn update_saved(&self, config: ConnectionConfig) -> Result<(), DatabaseError> {
        let id = config.id.clone()
            .ok_or_else(|| DatabaseError::Unknown("更新连接缺少 id".into()))?;
        let mut cfg = config;
        if cfg.password.is_empty() {
            if let Some(old) = self.require_store()?.get_decrypted(&id)? {
                cfg.password = old.password;
            }
        }
        self.require_store()?.save(&cfg)?;
        self.disconnect(&id).await.ok();
        Ok(())
    }

    // ---------- 查询历史 ----------

    /// 记录一条执行成功的查询（编辑器手动执行的才进来）
    pub fn record_history(&self, connection_id: &str, database: Option<&str>, sql: &str, result: &QueryResult) {
        let Some(history) = &self.history else { return };
        let entry = HistoryEntry {
            id: Uuid::new_v4().to_string(),
            connection_id: connection_id.to_string(),
            database: database.map(|d| d.to_string()),
            sql: sql.to_string(),
            executed_at: chrono::Utc::now().timestamp_millis(),
            duration_ms: result.execution_time_ms,
            row_count: result.rows_affected.or(result.total_count.map(|n| n as u64)),
            truncated: result.truncated,
        };
        if let Err(e) = history.append(entry) {
            eprintln!("记录查询历史失败: {}", e);
        }
    }

    pub fn list_history(&self, connection_id: &str, limit: usize) -> Vec<HistoryEntry> {
        self.history
            .as_ref()
            .map(|h| h.list(connection_id, limit))
            .unwrap_or_default()
    }

    pub fn delete_history(&self, id: &str) -> Result<(), DatabaseError> {
        self.history
            .as_ref()
            .ok_or_else(|| DatabaseError::Unknown("历史存储不可用".into()))?
            .delete(id)
    }

    pub fn clear_history(&self, connection_id: &str) -> Result<(), DatabaseError> {
        self.history
            .as_ref()
            .ok_or_else(|| DatabaseError::Unknown("历史存储不可用".into()))?
            .clear(connection_id)
    }
}
