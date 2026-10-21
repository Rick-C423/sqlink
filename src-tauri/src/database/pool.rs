use crate::database::{create_and_connect_driver, DatabaseDriver};
use crate::history::HistoryStore;
use crate::models::*;
use crate::result_context::{
    ContextError, PublishedResultContext, ResultContext, ResultContextRegistry, ResultContextSeed,
    StoredRowIdentity,
};
use crate::store::ConnectionStore;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};
use uuid::Uuid;

#[derive(Clone)]
pub struct ConnectionSnapshot {
    pub driver: Arc<dyn DatabaseDriver>,
    pub generation: String,
    /// SQLite 文件来源来自后端连接配置，不能采用请求传入的标签冒充文件身份。
    pub database: Option<String>,
}

#[derive(Clone)]
pub struct ConnectionPoolManager {
    connections: Arc<RwLock<HashMap<String, ConnectionSnapshot>>>,
    lifecycle: Arc<Mutex<()>>,
    pub result_contexts: Arc<ResultContextRegistry>,
    pub tasks: Arc<crate::task_registry::TaskRegistry>,
    pub(crate) commit_receipts: Arc<crate::commit_receipt::ReceiptRegistry>,
    pub(crate) configs: Arc<RwLock<HashMap<String, ConnectionConfig>>>,
    store: Option<Arc<ConnectionStore>>,
    history: Option<Arc<HistoryStore>>,
    import_journal_dir: Option<PathBuf>,
}

impl ConnectionPoolManager {
    pub fn new(config_dir: Option<PathBuf>) -> Self {
        let import_journal_dir = config_dir
            .as_ref()
            .map(|directory| directory.join("import-journal"));
        let history = config_dir
            .as_ref()
            .map(|dir| Arc::new(HistoryStore::new(dir.clone())));
        let store = config_dir.and_then(|dir| match ConnectionStore::new(dir) {
            Ok(s) => Some(Arc::new(s)),
            Err(e) => {
                tracing::error!(error = %e, "连接存储初始化失败（持久化不可用）");
                None
            }
        });
        Self {
            connections: Arc::new(RwLock::new(HashMap::new())),
            lifecycle: Arc::new(Mutex::new(())),
            result_contexts: Arc::new(ResultContextRegistry::default()),
            tasks: Arc::new(crate::task_registry::TaskRegistry::default()),
            commit_receipts: Arc::new(crate::commit_receipt::ReceiptRegistry::default()),
            configs: Arc::new(RwLock::new(HashMap::new())),
            store,
            history,
            import_journal_dir,
        }
    }

    pub fn import_journal_dir(&self) -> Option<PathBuf> {
        self.import_journal_dir.clone()
    }

    pub async fn create_connection(
        &self,
        config: ConnectionConfig,
    ) -> Result<String, DatabaseError> {
        let _lifecycle = self.lifecycle.lock().await;
        let id = config
            .id
            .clone()
            .unwrap_or_else(|| Uuid::new_v4().to_string());

        let exists = self.connections.read().await.contains_key(&id);
        if exists {
            self.disconnect_inner(&id).await?;
        }

        // 只记元信息，绝不记密码/完整连接串
        let driver = create_and_connect_driver(&config).await.inspect_err(|e| {
            tracing::warn!(
                driver = %config.driver_type, host = %config.host, port = config.port,
                db = ?config.database, error = %e, "连接失败"
            );
        })?;
        tracing::info!(
            id = %id, driver = %config.driver_type, host = %config.host,
            port = config.port, db = ?config.database, "连接成功"
        );

        let database = config.database.clone();
        self.configs.write().await.insert(id.clone(), config);
        self.connections.write().await.insert(
            id.clone(),
            ConnectionSnapshot {
                driver,
                generation: Uuid::new_v4().to_string(),
                database,
            },
        );

        Ok(id)
    }

    pub async fn get_connection(&self, id: &str) -> Result<Arc<dyn DatabaseDriver>, DatabaseError> {
        Ok(self.get_connection_snapshot(id).await?.driver)
    }

    pub async fn get_connection_snapshot(
        &self,
        id: &str,
    ) -> Result<ConnectionSnapshot, DatabaseError> {
        // 快路径：已有活动连接
        if let Some(snapshot) = self.connections.read().await.get(id).cloned() {
            return Ok(snapshot);
        }
        // 重连双重检查，避免并发请求各自建连并覆盖更新的代次。
        let _lifecycle = self.lifecycle.lock().await;
        if let Some(snapshot) = self.connections.read().await.get(id).cloned() {
            return Ok(snapshot);
        }
        // 活动连接不存在（应用重启 / 被服务端断开）：用已保存配置自动重连，
        // 避免向前端抛"连接已关闭"——用户感知不到中断
        let config = self
            .require_store()?
            .get_decrypted(id)?
            .ok_or(DatabaseError::ConnectionClosed)?;
        tracing::info!(id = %id, "活动连接不存在，用已保存配置自动重连");
        let driver = create_and_connect_driver(&config).await?;
        let database = config.database.clone();
        self.configs.write().await.insert(id.to_string(), config);
        let snapshot = ConnectionSnapshot {
            driver,
            generation: Uuid::new_v4().to_string(),
            database,
        };
        self.connections
            .write()
            .await
            .insert(id.to_string(), snapshot.clone());
        Ok(snapshot)
    }

    pub async fn disconnect(&self, id: &str) -> Result<(), DatabaseError> {
        let _lifecycle = self.lifecycle.lock().await;
        self.disconnect_inner(id).await
    }

    async fn disconnect_inner(&self, id: &str) -> Result<(), DatabaseError> {
        let (snapshot, invalidation) = {
            let mut connections = self.connections.write().await;
            let snapshot = connections.remove(id);
            let invalidation = self
                .result_contexts
                .invalidate_connection(id, chrono::Utc::now().timestamp_millis());
            (snapshot, invalidation)
        };
        self.configs.write().await.remove(id);
        self.tasks.request_cancel_connection(id).await;
        // 显式关闭底层连接池，避免 TCP 连接泄漏
        if let Some(snapshot) = snapshot {
            snapshot.driver.disconnect().await?;
            tracing::info!(id = %id, "连接已断开");
        }
        invalidation.map_err(|error| DatabaseError::Unknown(error.to_string()))
    }

    /// 身份校验不能通过自动重连令旧结果重新生效。
    pub async fn active_connection_snapshot(
        &self,
        id: &str,
    ) -> Result<ConnectionSnapshot, ContextError> {
        self.connections
            .read()
            .await
            .get(id)
            .cloned()
            .ok_or_else(|| {
                ContextError::new("CONNECTION_CHANGED", "连接已关闭或发生变化，请重新加载结果")
            })
    }

    pub async fn register_result_context(
        &self,
        snapshot: &ConnectionSnapshot,
        seed: ResultContextSeed,
        rows: Vec<Option<StoredRowIdentity>>,
    ) -> Result<PublishedResultContext, ContextError> {
        self.register_verified_result_context(snapshot, seed, rows, None, None)
            .await
    }

    pub(crate) async fn register_verified_result_context(
        &self,
        snapshot: &ConnectionSnapshot,
        seed: ResultContextSeed,
        rows: Vec<Option<StoredRowIdentity>>,
        object_identity: Option<String>,
        policy: Option<crate::result_context::EditPolicy>,
    ) -> Result<PublishedResultContext, ContextError> {
        // 与断开/替换共享同一把连接表锁，不能发布已被换代的查询结果。
        let connections = self.connections.read().await;
        let current = connections.get(&seed.connection_id).ok_or_else(|| {
            ContextError::new("CONNECTION_CHANGED", "读取期间连接已关闭，请重新加载")
        })?;
        if current.generation != snapshot.generation
            || seed.connection_generation != current.generation
            || !Arc::ptr_eq(&current.driver, &snapshot.driver)
        {
            return Err(ContextError::new(
                "CONNECTION_CHANGED",
                "读取期间连接已换代，请重新加载",
            ));
        }
        self.result_contexts.register_with_identity(
            seed,
            rows,
            chrono::Utc::now().timestamp_millis(),
            object_identity,
            policy,
        )
    }

    pub async fn validate_result_handles(
        &self,
        context: &ResultContext,
        handles: &[String],
    ) -> Result<Vec<StoredRowIdentity>, ContextError> {
        let connections = self.connections.read().await;
        let current = connections
            .get(&context.connection_id)
            .ok_or_else(|| ContextError::new("CONNECTION_CHANGED", "连接已关闭，请重新加载结果"))?;
        self.result_contexts.validate(
            &context.result_id,
            context.revision,
            &context.connection_id,
            &current.generation,
            handles,
            chrono::Utc::now().timestamp_millis(),
        )
    }

    pub async fn list_connections(&self) -> Vec<(String, ConnectionConfig)> {
        let configs = self.configs.read().await;
        configs
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    // ---------- 持久化 ----------

    fn require_store(&self) -> Result<&ConnectionStore, DatabaseError> {
        self.store
            .as_ref()
            .map(Arc::as_ref)
            .ok_or_else(|| DatabaseError::Unknown("连接持久化存储不可用".into()))
    }

    pub fn credential_health(&self) -> crate::store::CredentialHealth {
        self.store
            .as_ref()
            .map(|store| store.credential_health())
            .unwrap_or(crate::store::CredentialHealth {
                state: "unavailable".into(),
                backend: None,
                message: Some("连接凭据存储未初始化，写入保持关闭".into()),
            })
    }

    /// 保存连接配置到磁盘（密码加密），并按保存的 id 建立活动连接
    pub async fn save_and_connect(
        &self,
        mut config: ConnectionConfig,
    ) -> Result<String, DatabaseError> {
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

    /// 更新已保存连接的 MCP 访问开关（仅改 connections.json，不断开活动连接）
    pub fn set_connection_mcp_allowed(&self, id: &str, allowed: bool) -> Result<(), DatabaseError> {
        self.require_store()?.set_mcp_allowed(id, allowed)
    }

    /// 用已保存的配置建立活动连接（密码在后端解密，不下发前端）
    pub async fn connect_saved(&self, id: &str) -> Result<String, DatabaseError> {
        let config = self
            .require_store()?
            .get_decrypted(id)?
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
        let id = config
            .id
            .clone()
            .ok_or_else(|| DatabaseError::Unknown("更新连接缺少 id".into()))?;
        // 空密码由存储层保留原密文；元数据更新不需要先解密旧密码。
        self.require_store()?.save(&config)?;
        self.disconnect(&id).await.ok();
        Ok(())
    }

    // ---------- 查询历史 ----------

    /// 记录一条执行成功的查询（编辑器手动执行的才进来）
    pub fn record_history(
        &self,
        connection_id: &str,
        database: Option<&str>,
        sql: &str,
        result: &QueryResult,
    ) {
        let Some(history) = &self.history else { return };
        let entry = HistoryEntry {
            id: Uuid::new_v4().to_string(),
            connection_id: connection_id.to_string(),
            database: database.map(|d| d.to_string()),
            sql: sql.to_string(),
            executed_at: chrono::Utc::now().timestamp_millis(),
            duration_ms: result.execution_time_ms,
            row_count: result
                .rows_affected
                .or(result.total_count.map(|n| n as u64)),
            truncated: result.truncated,
        };
        if let Err(e) = history.append(entry) {
            tracing::warn!(error = %e, "记录查询历史失败");
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
