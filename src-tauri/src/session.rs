//! 一项任务独占一个物理会话。租约未正常归还时必须回滚或隔离，不能把不确定状态交给下一任务。

use crate::models::{DatabaseError, QueryResult, ResolvedTable};
use crate::typed_value::{TypedQueryResult, TypedValue};
use async_trait::async_trait;
use serde::Serialize;
use std::sync::Arc;

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionCapabilities {
    pub cancellation: bool,
    pub cancellation_reason: Option<String>,
    pub exclusive_session: bool,
    /// full 清理/关闭整个物理会话；transactionOnly 保留逻辑连接的 TEMP/ATTACH/PRAGMA。
    pub reset_policy: String,
}

impl SessionCapabilities {
    pub fn supported() -> Self {
        Self {
            cancellation: true,
            cancellation_reason: None,
            exclusive_session: true,
            reset_policy: "full".into(),
        }
    }
    pub fn without_cancellation(reason: &str) -> Self {
        Self {
            cancellation: false,
            cancellation_reason: Some(reason.into()),
            exclusive_session: true,
            reset_policy: "full".into(),
        }
    }
    pub fn transaction_only() -> Self {
        Self {
            reset_policy: "transactionOnly".into(),
            ..Self::supported()
        }
    }
}

#[async_trait]
pub trait SessionCancellation: Send + Sync {
    /// 只对创建此句柄的租约有效；归还后必须拒绝，不得中断复用该连接的新任务。
    async fn cancel(&self) -> Result<(), DatabaseError>;
}

/// 仅由当前物理会话读取；身份令牌不作为公开结果字段下发。
#[derive(Clone, Debug)]
pub struct TargetSnapshot {
    pub table: ResolvedTable,
    pub identity: String,
    pub write_reasons: Vec<String>,
}

#[async_trait]
pub trait SessionLease: Send {
    /// 随机租约标识，每次租用重新生成；不能由池下标或可重用的服务器 PID 代替。
    fn session_id(&self) -> &str;
    fn capabilities(&self) -> SessionCapabilities;
    fn cancel_handle(&self) -> Option<Arc<dyn SessionCancellation>>;
    /// 仅供后端受控导出：逐行交付原值，不使用交互查询截断，也不收集整表。
    async fn stream_export(
        &mut self,
        sql: &str,
        sink: Arc<dyn crate::export::ExportSink>,
    ) -> Result<u64, DatabaseError> {
        let _ = (sql, sink);
        Err(DatabaseError::TypeError(
            "EXPORT_UNSUPPORTED: 当前会话不支持流式导出".into(),
        ))
    }
    async fn query(&mut self, sql: &str) -> Result<QueryResult, DatabaseError>;
    async fn execute(&mut self, sql: &str) -> Result<QueryResult, DatabaseError>;
    async fn query_typed(
        &mut self,
        sql: &str,
        params: &[TypedValue],
    ) -> Result<TypedQueryResult, DatabaseError>;
    async fn execute_bound(
        &mut self,
        sql: &str,
        params: &[TypedValue],
    ) -> Result<u64, DatabaseError>;
    async fn begin(&mut self) -> Result<(), DatabaseError>;
    /// 写事务须在旧行核对前取得适当的写入/对象保护；SQLite 使用 BEGIN IMMEDIATE。
    async fn begin_write(&mut self) -> Result<(), DatabaseError> {
        self.begin().await
    }
    /// 必须在本租约事务内读取目录；lock=true 时保护对象直到提交/回滚。
    async fn inspect_target(
        &mut self,
        target: &ResolvedTable,
        lock: bool,
    ) -> Result<TargetSnapshot, DatabaseError> {
        let _ = (target, lock);
        Err(DatabaseError::Unknown(
            "OBJECT_IDENTITY_UNSUPPORTED: 当前会话尚不能证明目标对象身份".into(),
        ))
    }
    async fn commit(&mut self) -> Result<(), DatabaseError>;
    async fn rollback(&mut self) -> Result<(), DatabaseError>;
    /// 正常归还前去活取消句柄并清理事务；按 reset_policy 清理或保留会话设置，失败时隔离。
    async fn release(self: Box<Self>) -> Result<(), DatabaseError>;
}
