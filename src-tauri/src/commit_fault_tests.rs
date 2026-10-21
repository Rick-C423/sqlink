//! 以真实 SQLite 事务注入丢回执、清理中断和计数异常，核对数据库与公开回执。
use super::*;
use crate::models::ConnectionConfig;
use crate::session::{SessionCancellation, SessionCapabilities, TargetSnapshot};
use crate::table_result::{read_table_page, TablePageRequest};
use crate::typed_value::TypedQueryResult;
use async_trait::async_trait;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;
use uuid::Uuid;

#[derive(Clone, Copy)]
enum Fault {
    CommitAckLost,
    ReleaseFailed,
    ReleasePending,
    MultiMatch,
    MultiAffected,
    ZeroAffected,
    RollbackAckLost,
    CommitPending,
    WritePending,
    CancelAfterCheck,
}
struct FaultLease {
    inner: Box<dyn SessionLease>,
    fault: Fault,
    release_started: Arc<Notify>,
    tasks: Arc<crate::task_registry::TaskRegistry>,
    task_id: String,
}
#[async_trait]
impl SessionLease for FaultLease {
    fn session_id(&self) -> &str {
        self.inner.session_id()
    }
    fn capabilities(&self) -> SessionCapabilities {
        self.inner.capabilities()
    }
    fn cancel_handle(&self) -> Option<Arc<dyn SessionCancellation>> {
        self.inner.cancel_handle()
    }
    async fn query(&mut self, sql: &str) -> Result<crate::models::QueryResult, DatabaseError> {
        self.inner.query(sql).await
    }
    async fn execute(&mut self, sql: &str) -> Result<crate::models::QueryResult, DatabaseError> {
        self.inner.execute(sql).await
    }
    async fn query_typed(
        &mut self,
        sql: &str,
        params: &[TypedValue],
    ) -> Result<TypedQueryResult, DatabaseError> {
        let mut result = self.inner.query_typed(sql, params).await?;
        if matches!(self.fault, Fault::CancelAfterCheck) {
            self.tasks.request_cancel(&self.task_id).await.unwrap();
        }
        if matches!(self.fault, Fault::MultiMatch) && !result.typed_rows.is_empty() {
            result.typed_rows.push(result.typed_rows[0].clone());
            result.result.rows.push(result.result.rows[0].clone());
        }
        Ok(result)
    }
    async fn execute_bound(
        &mut self,
        sql: &str,
        params: &[TypedValue],
    ) -> Result<u64, DatabaseError> {
        let affected = self.inner.execute_bound(sql, params).await?;
        if matches!(self.fault, Fault::WritePending) {
            self.release_started.notify_one();
            return std::future::pending().await;
        }
        Ok(match self.fault {
            Fault::MultiAffected | Fault::RollbackAckLost => 2,
            Fault::ZeroAffected => 0,
            _ => affected,
        })
    }
    async fn begin(&mut self) -> Result<(), DatabaseError> {
        self.inner.begin().await
    }
    async fn begin_write(&mut self) -> Result<(), DatabaseError> {
        self.inner.begin_write().await
    }
    async fn inspect_target(
        &mut self,
        target: &ResolvedTable,
        lock: bool,
    ) -> Result<TargetSnapshot, DatabaseError> {
        self.inner.inspect_target(target, lock).await
    }
    async fn commit(&mut self) -> Result<(), DatabaseError> {
        self.inner.commit().await?;
        if matches!(self.fault, Fault::CommitPending) {
            self.release_started.notify_one();
            return std::future::pending().await;
        }
        if matches!(self.fault, Fault::CommitAckLost) {
            Err(DatabaseError::ConnectionError(
                "故障注入：服务端已提交但回执丢失".into(),
            ))
        } else {
            Ok(())
        }
    }
    async fn rollback(&mut self) -> Result<(), DatabaseError> {
        self.inner.rollback().await?;
        if matches!(self.fault, Fault::RollbackAckLost) {
            Err(DatabaseError::ConnectionError(
                "故障注入：回滚回执丢失".into(),
            ))
        } else {
            Ok(())
        }
    }
    async fn release(self: Box<Self>) -> Result<(), DatabaseError> {
        let FaultLease {
            inner,
            fault,
            release_started,
            ..
        } = *self;
        inner.release().await?;
        release_started.notify_one();
        match fault {
            Fault::ReleaseFailed => {
                Err(DatabaseError::ConnectionError("故障注入：清理失败".into()))
            }
            Fault::ReleasePending => std::future::pending().await,
            _ => Ok(()),
        }
    }
}

async fn start(
    fault: Fault,
) -> (
    ConnectionPoolManager,
    ChangeSetRequest,
    tokio::task::JoinHandle<CommitReceipt>,
    Arc<Notify>,
) {
    let manager = ConnectionPoolManager::new(None);
    let config:ConnectionConfig=serde_json::from_value(serde_json::json!({"id":"fault","name":"提交故障回归","driverType":"sqlite","host":"","port":0,"user":"","password":"","database":":memory:"})).unwrap();
    manager.create_connection(config).await.unwrap();
    let driver = manager.get_connection("fault").await.unwrap();
    for sql in [
        "PRAGMA foreign_keys=ON",
        "CREATE TABLE entries(id INTEGER PRIMARY KEY NOT NULL,value TEXT)",
        "INSERT INTO entries VALUES(1,'原值')",
    ] {
        driver.execute_bound(sql, &[], None).await.unwrap();
    }
    let page = read_table_page(
        &manager,
        TablePageRequest {
            protocol_version: 1,
            connection_id: "fault".into(),
            database: "main".into(),
            table_parts: vec!["entries".into()],
            page: 1,
            page_size: 20,
            sort: None,
        },
    )
    .await
    .unwrap();
    let request = ChangeSetRequest {
        protocol_version: 1,
        change_set_id: Uuid::new_v4().to_string(),
        result_id: page.context.result_id.clone(),
        revision: 1,
        operations: vec![ChangeOperation::Update {
            op_id: Uuid::new_v4().to_string(),
            row_handle: page.row_handles[0].clone().unwrap(),
            changes: BTreeMap::from([(
                page.context.columns[1].column_id.clone(),
                CellInput::Set {
                    value: TypedValue::Text("新值".into()),
                },
            )]),
        }],
    };
    let mut task = manager
        .tasks
        .start(None, "fault", None, "changeset")
        .unwrap();
    task.prepare(
        &page.context.connection_generation,
        driver.session_capabilities(),
    )
    .unwrap();
    let inner = driver.lease_session(None).await.unwrap();
    task.attach(&*inner).unwrap();
    task.running().unwrap();
    let mut base = base_receipt(&request, &page.context);
    base.task_id = Some(task.task_id().into());
    let digest = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&request).unwrap())
    );
    let ReceiptReservation::New(mut guard) = manager.commit_receipts.reserve(digest, base).unwrap()
    else {
        panic!("夹具必须生成新变更ID")
    };
    guard.bind_contexts(manager.result_contexts.clone());
    let notify = Arc::new(Notify::new());
    let lease = Box::new(FaultLease {
        inner,
        fault,
        release_started: notify.clone(),
        tasks: manager.tasks.clone(),
        task_id: task.task_id().into(),
    });
    let worker_manager = manager.clone();
    let worker_request = request.clone();
    let job = tokio::spawn(async move {
        run_on_lease(
            &worker_manager,
            &worker_request,
            &page.context,
            &*driver,
            lease,
            guard,
            task,
        )
        .await
    });
    (manager, request, job, notify)
}
async fn value(manager: &ConnectionPoolManager) -> String {
    let result = tokio::time::timeout(
        Duration::from_secs(3),
        manager.get_connection("fault").await.unwrap().query_typed(
            "SELECT value FROM entries",
            &[],
            None,
        ),
    )
    .await
    .unwrap()
    .unwrap();
    let Some(TypedValue::Text(value)) = &result.typed_rows[0][0] else {
        panic!("夹具必须为文本")
    };
    value.clone()
}
#[tokio::test]
async fn lost_commit_ack_preserves_unknown_and_never_replays_committed_data() {
    let (manager, request, job, _) = start(Fault::CommitAckLost).await;
    let receipt = job.await.unwrap();
    assert_eq!(receipt.status, CommitStatus::Unknown);
    assert_eq!(receipt.commit_state, CommitState::Unknown);
    assert_eq!(receipt.error_code.as_deref(), Some("COMMIT_UNKNOWN"));
    assert_eq!(value(&manager).await, "新值");
    let repeated = commit_internal(&manager, request.clone(), None, true)
        .await
        .unwrap();
    assert_eq!(repeated.status, CommitStatus::Unknown);
    assert!(manager
        .result_contexts
        .get_context(&request.result_id, 1, now())
        .is_err());
    assert_eq!(manager.tasks.active_count(), 0);
}
#[tokio::test]
async fn confirmed_commit_is_not_undone_by_cleanup_failure() {
    let (manager, request, job, _) = start(Fault::ReleaseFailed).await;
    let receipt = job.await.unwrap();
    assert_eq!(receipt.status, CommitStatus::Committed);
    assert_eq!(receipt.commit_state, CommitState::Committed);
    assert_eq!(receipt.cleanup_state, CleanupState::Failed);
    assert_eq!(receipt.refresh_state, RefreshState::Required);
    assert_eq!(value(&manager).await, "新值");
    assert_eq!(
        manager
            .commit_receipts
            .get(&request.change_set_id, &request.result_id, 1)
            .unwrap()
            .status,
        CommitStatus::Committed
    );
}
#[tokio::test]
async fn confirmed_commit_survives_owner_abort_during_cleanup() {
    let (manager, request, job, notify) = start(Fault::ReleasePending).await;
    tokio::time::timeout(Duration::from_secs(3), notify.notified())
        .await
        .unwrap();
    assert_eq!(
        manager
            .commit_receipts
            .get(&request.change_set_id, &request.result_id, 1)
            .unwrap()
            .status,
        CommitStatus::Committed
    );
    job.abort();
    assert!(job.await.unwrap_err().is_cancelled());
    let receipt = manager
        .commit_receipts
        .get(&request.change_set_id, &request.result_id, 1)
        .unwrap();
    assert_eq!(receipt.status, CommitStatus::Committed);
    assert_eq!(receipt.cleanup_state, CleanupState::Unknown);
    assert_eq!(
        receipt.error_code.as_deref(),
        Some("COMMITTED_CLEANUP_UNKNOWN")
    );
    assert_eq!(value(&manager).await, "新值");
    assert_eq!(manager.tasks.active_count(), 0);
}
#[tokio::test]
async fn multiple_matches_never_execute_dml() {
    let (manager, _, job, _) = start(Fault::MultiMatch).await;
    let receipt = job.await.unwrap();
    assert_eq!(receipt.status, CommitStatus::Conflict);
    assert_eq!(receipt.error_code.as_deref(), Some("MULTI_ROW_MATCH"));
    assert_eq!(receipt.operation_results[0].matched_rows, Some(2));
    assert_eq!(receipt.operation_results[0].affected_rows, None);
    assert_eq!(value(&manager).await, "原值");
}
#[tokio::test]
async fn incorrect_affected_counts_roll_back_and_report_attempted_operation() {
    for fault in [Fault::MultiAffected, Fault::ZeroAffected] {
        let (manager, _, job, _) = start(fault).await;
        let receipt = job.await.unwrap();
        assert_eq!(receipt.status, CommitStatus::RolledBack);
        assert_eq!(
            receipt.error_code.as_deref(),
            Some("AFFECTED_ROWS_MISMATCH")
        );
        assert_eq!(
            receipt.operation_results[0].outcome,
            OperationOutcome::RolledBack
        );
        assert_eq!(value(&manager).await, "原值");
    }
}
#[tokio::test]
async fn rollback_ack_failure_does_not_claim_safe_rollback() {
    let (manager, request, job, _) = start(Fault::RollbackAckLost).await;
    let receipt = job.await.unwrap();
    assert_eq!(receipt.status, CommitStatus::Unknown);
    assert_eq!(receipt.commit_state, CommitState::Unknown);
    assert_eq!(receipt.rollback_state, CleanupState::Failed);
    assert_eq!(
        receipt.operation_results[0].outcome,
        OperationOutcome::Unknown
    );
    assert_eq!(value(&manager).await, "原值");
    assert_eq!(manager.tasks.active_count(), 0);
    assert_unknown_snapshot_consumed(&manager, &request).await;
}

#[tokio::test]
async fn abort_during_commit_keeps_unknown_even_when_database_committed() {
    let (manager, request, job, notify) = start(Fault::CommitPending).await;
    tokio::time::timeout(Duration::from_secs(3), notify.notified())
        .await
        .unwrap();
    job.abort();
    assert!(job.await.unwrap_err().is_cancelled());
    let receipt = manager
        .commit_receipts
        .get(&request.change_set_id, &request.result_id, 1)
        .unwrap();
    assert_eq!(receipt.status, CommitStatus::Unknown);
    assert_eq!(value(&manager).await, "新值");
    assert_eq!(manager.tasks.active_count(), 0);
    assert!(manager
        .result_contexts
        .get_context(&request.result_id, 1, now())
        .is_err());
}
#[tokio::test]
async fn abort_after_write_rolls_back_and_keeps_unknown_receipt() {
    let (manager, request, job, notify) = start(Fault::WritePending).await;
    tokio::time::timeout(Duration::from_secs(3), notify.notified())
        .await
        .unwrap();
    job.abort();
    assert!(job.await.unwrap_err().is_cancelled());
    let receipt = manager
        .commit_receipts
        .get(&request.change_set_id, &request.result_id, 1)
        .unwrap();
    assert_eq!(receipt.status, CommitStatus::Unknown);
    assert_eq!(value(&manager).await, "原值");
    assert_eq!(manager.tasks.active_count(), 0);
    assert_eq!(
        commit_internal(&manager, request.clone(), None, true)
            .await
            .unwrap()
            .status,
        CommitStatus::Unknown
    );
    assert_unknown_snapshot_consumed(&manager, &request).await;
}

async fn assert_unknown_snapshot_consumed(
    manager: &ConnectionPoolManager,
    request: &ChangeSetRequest,
) {
    assert!(manager
        .result_contexts
        .get_context(&request.result_id, request.revision, now())
        .is_err());
    let mut replay = request.clone();
    replay.change_set_id = Uuid::new_v4().to_string();
    assert_eq!(
        commit_internal(manager, replay, None, true)
            .await
            .unwrap_err()
            .code,
        "RESULT_CONTEXT_NOT_FOUND"
    );
}
#[tokio::test]
async fn requested_cancellation_after_row_check_rolls_back_before_writes() {
    let (manager, _, job, _) = start(Fault::CancelAfterCheck).await;
    let receipt = job.await.unwrap();
    assert_eq!(receipt.status, CommitStatus::RolledBack);
    assert_eq!(receipt.error_code.as_deref(), Some("QUERY_CANCELLED"));
    assert_eq!(
        receipt.operation_results[0].outcome,
        OperationOutcome::NotExecuted
    );
    assert_eq!(value(&manager).await, "原值");
    assert_eq!(manager.tasks.active_count(), 0);
}
