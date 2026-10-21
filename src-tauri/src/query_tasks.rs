//! 编辑器任务服务：固定连接代次，贯穿租约、取消与提交阶段，保持原查询 IPC 的结果字段兼容。

use crate::database::{pool::ConnectionPoolManager, returns_rows};
use crate::models::{DatabaseError, QueryResult};
use crate::session::SessionLease;
use crate::task_registry::{CleanupState, CommitState, TaskGuard, TaskPhase, TaskSnapshot};
use serde::Serialize;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskQueryResult {
    #[serde(flatten)]
    pub result: QueryResult,
    pub task: TaskSnapshot,
}

fn finish_before(task: &mut TaskGuard, cancelled: bool, code: &str) {
    let _ = task.finish(
        if cancelled {
            TaskPhase::Cancelled
        } else {
            TaskPhase::Failed
        },
        CommitState::NotStarted,
        CleanupState::NotStarted,
        CleanupState::Succeeded,
        Some(code),
    );
}

async fn release_unstarted(
    task: &mut TaskGuard,
    lease: Box<dyn SessionLease>,
    cancelled: bool,
) -> Result<(), String> {
    if lease.release().await.is_err() {
        let _ = task.finish(
            TaskPhase::Unknown,
            CommitState::NotStarted,
            CleanupState::NotStarted,
            CleanupState::Failed,
            Some("SESSION_RELEASE_FAILED"),
        );
        Err("SESSION_RELEASE_FAILED: 未执行用户 SQL，但会话清理无法确认".into())
    } else {
        finish_before(
            task,
            cancelled,
            if cancelled {
                "QUERY_CANCELLED"
            } else {
                "TASK_START_FAILED"
            },
        );
        Ok(())
    }
}

async fn acquire(
    manager: &ConnectionPoolManager,
    task: &mut TaskGuard,
    database: Option<&str>,
    expected_generation: Option<&str>,
) -> Result<(Box<dyn SessionLease>, String), String> {
    let connection_id = task.snapshot().map_err(|e| e.to_string())?.connection_id;
    let snapshot = tokio::select! {
        biased;
        _=task.cancelled()=>{finish_before(task,true,"QUERY_CANCELLED");return Err(DatabaseError::QueryCancelled.to_string());},
        result=manager.get_connection_snapshot(&connection_id)=>match result{Ok(value)=>value,Err(error)=>{finish_before(task,false,"CONNECTION_UNAVAILABLE");return Err(error.to_string());}}
    };
    if expected_generation.is_some_and(|expected| snapshot.generation != expected) {
        finish_before(task, false, "METADATA_CONNECTION_CHANGED");
        return Err("METADATA_CONNECTION_CHANGED: 结构操作属于旧连接，未执行 SQL".into());
    }
    task.prepare(&snapshot.generation, snapshot.driver.session_capabilities())
        .map_err(|e| e.to_string())?;
    let lease = tokio::select! {
        biased;
        _=task.cancelled()=>{finish_before(task,true,"QUERY_CANCELLED");return Err(DatabaseError::QueryCancelled.to_string());},
        result=snapshot.driver.lease_session(database)=>match result{Ok(value)=>value,Err(error)=>{finish_before(task,false,"SESSION_ACQUIRE_FAILED");return Err(error.to_string());}}
    };
    task.attach(&*lease).map_err(|e| e.to_string())?;
    if task.cancel_requested() {
        let cleanup = lease.release().await;
        if cleanup.is_ok() {
            finish_before(task, true, "QUERY_CANCELLED");
        } else {
            let _ = task.finish(
                TaskPhase::Unknown,
                CommitState::NotStarted,
                CleanupState::Unknown,
                CleanupState::Failed,
                Some("SESSION_RELEASE_FAILED"),
            );
        }
        return Err(DatabaseError::QueryCancelled.to_string());
    }
    Ok((lease, snapshot.generation))
}

async fn current_generation(
    manager: &ConnectionPoolManager,
    connection_id: &str,
    generation: &str,
) -> bool {
    manager
        .active_connection_snapshot(connection_id)
        .await
        .is_ok_and(|s| s.generation == generation && s.driver.is_connected())
}

pub async fn execute_query(
    manager: &ConnectionPoolManager,
    connection_id: String,
    sql: String,
    database: Option<String>,
    query_id: Option<u64>,
    task_id: Option<String>,
    record: bool,
) -> Result<TaskQueryResult, String> {
    let mut task = manager
        .tasks
        .start(task_id, &connection_id, query_id, "query")
        .map_err(|e| e.to_string())?;
    let (mut lease, generation) = acquire(manager, &mut task, database.as_deref(), None).await?;
    if let Err(error) = task.running() {
        let cancelled = task.cancel_requested();
        release_unstarted(&mut task, lease, cancelled).await?;
        return Err(error.to_string());
    }
    let result = if returns_rows(&sql) {
        lease.query(&sql).await
    } else {
        lease.execute(&sql).await
    };
    let cleanup = lease.release().await;
    if cleanup.is_err() || !current_generation(manager, &connection_id, &generation).await {
        let _ = task.finish(
            TaskPhase::Unknown,
            CommitState::Unknown,
            CleanupState::Unknown,
            if cleanup.is_ok() {
                CleanupState::Succeeded
            } else {
                CleanupState::Failed
            },
            Some("SESSION_RESULT_UNKNOWN"),
        );
        return Err("SESSION_RESULT_UNKNOWN: 执行后的连接状态无法确认，语句可能已生效，请核查数据库，不要自动重发".into());
    }
    match result {
        Ok(result) => {
            let snapshot = task
                .finish(
                    TaskPhase::Succeeded,
                    CommitState::NotApplicable,
                    CleanupState::NotStarted,
                    CleanupState::Succeeded,
                    None,
                )
                .map_err(|e| e.to_string())?;
            if result.execution_time_ms > 2000 {
                tracing::warn!(task_id=%snapshot.task_id,conn=%connection_id,db=?database,kind=%crate::database::first_keyword(&sql),ms=result.execution_time_ms,"慢查询");
            } else {
                tracing::debug!(task_id=%snapshot.task_id,conn=%connection_id,db=?database,ms=result.execution_time_ms,rows_affected=?result.rows_affected,"查询任务完成");
            }
            if record {
                manager.record_history(&connection_id, database.as_deref(), &sql, &result);
            }
            Ok(TaskQueryResult {
                result,
                task: snapshot,
            })
        }
        Err(error) => {
            let cancelled = matches!(error, DatabaseError::QueryCancelled);
            let _ = task.finish(
                if cancelled {
                    TaskPhase::Cancelled
                } else {
                    TaskPhase::Failed
                },
                if cancelled {
                    CommitState::NotApplicable
                } else {
                    CommitState::Unknown
                },
                CleanupState::NotStarted,
                CleanupState::Succeeded,
                Some(if cancelled {
                    "QUERY_CANCELLED"
                } else {
                    "QUERY_FAILED"
                }),
            );
            Err(error.to_string())
        }
    }
}

async fn rollback_failure(
    task: &mut TaskGuard,
    mut lease: Box<dyn SessionLease>,
    error: DatabaseError,
) -> String {
    let rollback = lease.rollback().await;
    let cleanup = lease.release().await;
    let known = rollback.is_ok() && cleanup.is_ok();
    let cancelled = matches!(error, DatabaseError::QueryCancelled);
    let _ = task.finish(
        if !known {
            TaskPhase::Unknown
        } else if cancelled {
            TaskPhase::Cancelled
        } else {
            TaskPhase::Failed
        },
        if known {
            CommitState::RolledBack
        } else {
            CommitState::Unknown
        },
        if rollback.is_ok() {
            CleanupState::Succeeded
        } else {
            CleanupState::Failed
        },
        if cleanup.is_ok() {
            CleanupState::Succeeded
        } else {
            CleanupState::Failed
        },
        Some(if known && cancelled {
            "QUERY_CANCELLED"
        } else if known {
            "BATCH_FAILED"
        } else {
            "ROLLBACK_UNKNOWN"
        }),
    );
    if known {
        error.to_string()
    } else {
        "ROLLBACK_UNKNOWN: 执行失败且无法确认回滚或清理结果，请核查数据库，不要自动重发".into()
    }
}

pub async fn execute_batch(
    manager: &ConnectionPoolManager,
    connection_id: String,
    database: Option<String>,
    statements: Vec<String>,
    query_id: Option<u64>,
    task_id: Option<String>,
) -> Result<u64, String> {
    execute_batch_checked(
        manager,
        connection_id,
        database,
        statements,
        query_id,
        task_id,
        None,
    )
    .await
}

pub async fn execute_batch_checked(
    manager: &ConnectionPoolManager,
    connection_id: String,
    database: Option<String>,
    statements: Vec<String>,
    query_id: Option<u64>,
    task_id: Option<String>,
    expected_generation: Option<String>,
) -> Result<u64, String> {
    let mut task = manager
        .tasks
        .start(task_id, &connection_id, query_id, "batch")
        .map_err(|e| e.to_string())?;
    let (mut lease, generation) = acquire(
        manager,
        &mut task,
        database.as_deref(),
        expected_generation.as_deref(),
    )
    .await?;
    if let Err(error) = task.running() {
        let cancelled = task.cancel_requested();
        release_unstarted(&mut task, lease, cancelled).await?;
        return Err(error.to_string());
    }
    if let Err(error) = lease.begin().await {
        return Err(rollback_failure(&mut task, lease, error).await);
    }
    let mut affected = 0u64;
    for sql in statements {
        if task.cancel_requested() {
            return Err(rollback_failure(&mut task, lease, DatabaseError::QueryCancelled).await);
        }
        let result = if returns_rows(&sql) {
            lease.query(&sql).await
        } else {
            lease.execute(&sql).await
        };
        match result {
            Ok(result) => affected = affected.saturating_add(result.rows_affected.unwrap_or(0)),
            Err(error) => return Err(rollback_failure(&mut task, lease, error).await),
        }
    }
    if task.committing().is_err() {
        return Err(rollback_failure(&mut task, lease, DatabaseError::QueryCancelled).await);
    }
    if lease.commit().await.is_err() {
        let cleanup = lease.release().await;
        let _ = task.finish(
            TaskPhase::Unknown,
            CommitState::Unknown,
            CleanupState::Unknown,
            if cleanup.is_ok() {
                CleanupState::Succeeded
            } else {
                CleanupState::Failed
            },
            Some("COMMIT_UNKNOWN"),
        );
        return Err("COMMIT_UNKNOWN: 提交结果无法确认，请核查数据库，不要自动重发".into());
    }
    task.committed().map_err(|e| e.to_string())?;
    // COMMIT 已确认，随后清理/连接变化不能把已提交内容改报为未保存。
    let cleanup = lease.release().await;
    let same = current_generation(manager, &connection_id, &generation).await;
    let _ = task.finish(
        TaskPhase::Succeeded,
        CommitState::Committed,
        CleanupState::NotStarted,
        if cleanup.is_ok() {
            CleanupState::Succeeded
        } else {
            CleanupState::Failed
        },
        if cleanup.is_err() {
            Some("COMMITTED_CLEANUP_FAILED")
        } else if !same {
            Some("COMMITTED_CONNECTION_CHANGED")
        } else {
            None
        },
    );
    Ok(affected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{SessionCancellation, SessionCapabilities};
    use crate::task_registry::TaskRegistry;
    use crate::typed_value::{TypedQueryResult, TypedValue};
    use std::sync::Arc;
    struct FailedRelease;
    #[async_trait::async_trait]
    impl SessionLease for FailedRelease {
        fn session_id(&self) -> &str {
            "00000000-0000-4000-8000-000000000001"
        }
        fn capabilities(&self) -> SessionCapabilities {
            SessionCapabilities::without_cancellation("故障注入")
        }
        fn cancel_handle(&self) -> Option<Arc<dyn SessionCancellation>> {
            None
        }
        async fn query(&mut self, _: &str) -> Result<QueryResult, DatabaseError> {
            panic!("不应执行 SQL")
        }
        async fn execute(&mut self, _: &str) -> Result<QueryResult, DatabaseError> {
            panic!("不应执行 SQL")
        }
        async fn query_typed(
            &mut self,
            _: &str,
            _: &[TypedValue],
        ) -> Result<TypedQueryResult, DatabaseError> {
            panic!("不应执行 SQL")
        }
        async fn execute_bound(&mut self, _: &str, _: &[TypedValue]) -> Result<u64, DatabaseError> {
            panic!("不应执行 SQL")
        }
        async fn begin(&mut self) -> Result<(), DatabaseError> {
            panic!("不应开始事务")
        }
        async fn commit(&mut self) -> Result<(), DatabaseError> {
            panic!("不应提交")
        }
        async fn rollback(&mut self) -> Result<(), DatabaseError> {
            panic!("不应回滚未开始的事务")
        }
        async fn release(self: Box<Self>) -> Result<(), DatabaseError> {
            Err(DatabaseError::ConnectionClosed)
        }
    }
    #[tokio::test]
    async fn pre_execution_cancel_does_not_claim_failed_cleanup_succeeded() {
        let registry = Arc::new(TaskRegistry::default());
        let mut task = registry.start(None, "A", None, "query").unwrap();
        let id = task.task_id().to_owned();
        task.prepare("gen", SessionCapabilities::supported())
            .unwrap();
        registry.request_cancel(&id).await.unwrap();
        assert!(task.running().is_err());
        assert!(release_unstarted(&mut task, Box::new(FailedRelease), true)
            .await
            .is_err());
        let status = registry.snapshot(&id).unwrap();
        assert_eq!(status.phase, TaskPhase::Unknown);
        assert_eq!(status.commit_state, CommitState::NotStarted);
        assert_eq!(status.cleanup_state, CleanupState::Failed);
        assert_eq!(registry.active_count(), 0);
    }
}
