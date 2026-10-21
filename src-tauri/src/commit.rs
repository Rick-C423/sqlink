//! 受控提交：同一租约内核验对象、锁定旧行、参数化写入和确认回执。

use crate::changeset::{self, CellInput, ChangeOperation, ChangeSetRequest};
use crate::commit_receipt::{
    CommitReceipt, CommitStatus, OperationOutcome, OperationResult, ReceiptGuard,
    ReceiptReservation, RefreshState,
};
use crate::database::{pool::ConnectionPoolManager, DatabaseDriver};
use crate::models::{DatabaseError, ResolvedTable};
use crate::result_context::{ContextError, ResultContext, StoredRowIdentity};
use crate::session::SessionLease;
use crate::table_result::{metadata_fingerprint, object_fingerprint, quote, table_sql};
use crate::task_registry::{CleanupState, CommitState, TaskGuard, TaskPhase};
use crate::typed_value::TypedValue;
use sha2::{Digest, Sha256};
use std::collections::HashMap;

fn error(code: &str, message: &str) -> ContextError {
    ContextError::new(code, message)
}
fn now() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// 发布入口只使用后端登记的授权，不接受调用方提供来源或授权开关。
pub async fn commit_changeset(
    manager: &ConnectionPoolManager,
    request: ChangeSetRequest,
    task_id: Option<String>,
) -> Result<CommitReceipt, ContextError> {
    commit_internal(manager, request, task_id, false).await
}

fn base_receipt(request: &ChangeSetRequest, context: &ResultContext) -> CommitReceipt {
    CommitReceipt {
        protocol_version: 1,
        change_set_id: request.change_set_id.clone(),
        result_id: request.result_id.clone(),
        revision: request.revision,
        connection_id: context.connection_id.clone(),
        connection_generation: context.connection_generation.clone(),
        task_id: None,
        status: CommitStatus::FailedBeforeCommit,
        commit_state: CommitState::NotStarted,
        rollback_state: CleanupState::NotStarted,
        cleanup_state: CleanupState::Succeeded,
        refresh_state: RefreshState::NotRequested,
        operation_results: request
            .operations
            .iter()
            .map(|op| OperationResult {
                op_id: op.op_id().into(),
                outcome: OperationOutcome::NotExecuted,
                matched_rows: None,
                affected_rows: None,
                error_code: None,
                current_values: None,
            })
            .collect(),
        error_code: None,
        reason_codes: vec![],
        completed_at: now(),
    }
}

fn reject(mut guard: ReceiptGuard, code: &str) -> CommitReceipt {
    guard.receipt.error_code = Some(code.into());
    guard.receipt.reason_codes.push(code.into());
    guard.finish()
}

/// 模块测试可绕过发布门禁验证协调器；该参数不在 IPC、配置或环境变量中暴露。
async fn commit_internal(
    manager: &ConnectionPoolManager,
    request: ChangeSetRequest,
    task_id: Option<String>,
    permitted: bool,
) -> Result<CommitReceipt, ContextError> {
    let handles = changeset::validate_shape(&request)?;
    let payload =
        serde_json::to_vec(&request).map_err(|_| error("INVALID_CHANGESET", "变更集无法编码"))?;
    let digest = format!("{:x}", Sha256::digest(payload));
    if let Some(receipt) = manager
        .commit_receipts
        .existing(&request.change_set_id, &digest)?
    {
        return Ok(receipt);
    }
    let context =
        manager
            .result_contexts
            .get_context(&request.result_id, request.revision, now())?;
    manager.validate_result_handles(&context, &handles).await?;
    let guard = manager
        .commit_receipts
        .reserve(digest, base_receipt(&request, &context))?;
    let mut guard = match guard {
        ReceiptReservation::Existing(receipt) => return Ok(receipt),
        ReceiptReservation::New(guard) => guard,
    };
    guard.bind_contexts(manager.result_contexts.clone());
    if !permitted {
        if manager.credential_health().state != "ready" {
            return Ok(reject(guard, "CREDENTIAL_RECOVERY_REQUIRED"));
        }
        if let Err(code) = validate_public_permission(&request, &context) {
            return Ok(reject(guard, code));
        }
    }
    let snapshot = match manager
        .active_connection_snapshot(&context.connection_id)
        .await
    {
        Ok(snapshot) if snapshot.generation == context.connection_generation => snapshot,
        _ => return Ok(reject(guard, "CONNECTION_CHANGED")),
    };
    let mut task = match manager
        .tasks
        .start(task_id, &context.connection_id, None, "changeset")
    {
        Ok(task) => task,
        Err(error) => return Ok(reject(guard, &error.code)),
    };
    guard.receipt.task_id = Some(task.task_id().into());
    task.prepare(&snapshot.generation, snapshot.driver.session_capabilities())
        .map_err(|e| error(&e.code, "提交任务无法准备"))?;
    let acquired = tokio::select! {
        biased;
        _=task.cancelled()=>Err(DatabaseError::QueryCancelled),
        result=snapshot.driver.lease_session(Some(&context.source.database))=>result,
    };
    let lease = match acquired {
        Ok(lease) => lease,
        Err(error) => {
            let code = if matches!(error, DatabaseError::QueryCancelled) {
                "QUERY_CANCELLED"
            } else {
                "SESSION_ACQUIRE_FAILED"
            };
            let _ = task.finish(
                if code == "QUERY_CANCELLED" {
                    TaskPhase::Cancelled
                } else {
                    TaskPhase::Failed
                },
                CommitState::NotStarted,
                CleanupState::NotStarted,
                CleanupState::Succeeded,
                Some(code),
            );
            return Ok(reject(guard, code));
        }
    };
    if let Err(error) = task.attach(&*lease).and_then(|_| task.running()) {
        let cleanup = lease.release().await;
        if cleanup.is_err() {
            guard.receipt.status = CommitStatus::Unknown;
            guard.receipt.commit_state = CommitState::Unknown;
            guard.receipt.cleanup_state = CleanupState::Failed;
        }
        let _ = task.finish(
            if cleanup.is_ok() {
                TaskPhase::Failed
            } else {
                TaskPhase::Unknown
            },
            guard.receipt.commit_state,
            CleanupState::NotStarted,
            guard.receipt.cleanup_state,
            Some(&error.code),
        );
        return Ok(reject(guard, &error.code));
    }
    Ok(run_on_lease(
        manager,
        &request,
        &context,
        &*snapshot.driver,
        lease,
        guard,
        task,
    )
    .await)
}

fn validate_public_permission(
    request: &ChangeSetRequest,
    context: &ResultContext,
) -> Result<(), &'static str> {
    for operation in &request.operations {
        let (kind, changes) = match operation {
            ChangeOperation::Insert { changes, .. } => ("insert", Some(changes)),
            ChangeOperation::Update { changes, .. } => ("update", Some(changes)),
            ChangeOperation::Delete { .. } => ("delete", None),
        };
        if !context
            .edit_policy
            .allowed_operations
            .iter()
            .any(|allowed| allowed == kind)
        {
            return Err("DRIVER_WRITE_VALIDATION_REQUIRED");
        }
        if let Some(changes) = changes {
            for (id, input) in changes {
                let column = context
                    .columns
                    .iter()
                    .find(|column| &column.column_id == id)
                    .filter(|column| column.writable && !column.generated)
                    .ok_or("COLUMN_NOT_WRITABLE")?;
                let kind = context
                    .edit_policy
                    .column_input_kinds
                    .get(&column.column_id)
                    .ok_or("COLUMN_NOT_WRITABLE")?;
                if let CellInput::Set { value } = input {
                    let matches = match value {
                        TypedValue::Null => column.nullable,
                        TypedValue::Integer(_) => kind == "integer",
                        TypedValue::UnsignedInteger(_) => kind == "unsignedInteger",
                        TypedValue::Text(_) => kind == "text",
                        TypedValue::Boolean(_) => kind == "boolean",
                        TypedValue::Binary(_) => kind == "binary",
                        TypedValue::Float(_) => kind == "float",
                        _ => false,
                    };
                    if !matches {
                        return Err("GRID_VALUE_TYPE_NOT_VALIDATED");
                    }
                }
            }
        }
    }
    Ok(())
}

fn select_row(
    driver: &dyn DatabaseDriver,
    table: &ResolvedTable,
    identity: &StoredRowIdentity,
) -> (String, Vec<TypedValue>) {
    let kind = driver.driver_type();
    let fields = table
        .columns
        .iter()
        .map(|c| quote(driver, &c.name))
        .collect::<Vec<_>>()
        .join(", ");
    let params = identity
        .keys
        .iter()
        .map(|(_, value)| value.clone())
        .collect::<Vec<_>>();
    let predicate = identity
        .keys
        .iter()
        .enumerate()
        .map(|(i, (name, _))| {
            format!(
                "{} = {}",
                quote(driver, name),
                changeset::placeholder(kind, i + 1)
            )
        })
        .collect::<Vec<_>>()
        .join(" AND ");
    let target = table_sql(driver, table);
    let sql = if matches!(kind, "mssql" | "sqlserver") {
        format!("SELECT TOP (2) {fields} FROM {target} WITH (UPDLOCK,HOLDLOCK) WHERE {predicate}")
    } else {
        format!(
            "SELECT {fields} FROM {target} WHERE {predicate} LIMIT 2{}",
            if matches!(kind, "mysql" | "mariadb" | "postgres" | "postgresql") {
                " FOR UPDATE"
            } else {
                ""
            }
        )
    };
    (sql, params)
}

fn unchanged(op: &ChangeOperation, context: &ResultContext, identity: &StoredRowIdentity) -> bool {
    let ChangeOperation::Update { changes, .. } = op else {
        return false;
    };
    changes.iter().all(|(id, input)| {
        let CellInput::Set { value } = input else {
            return false;
        };
        context
            .columns
            .iter()
            .position(|c| &c.column_id == id)
            .is_some_and(|i| identity.original_values.get(i) == Some(&Some(value.clone())))
    })
}

async fn rolled_back(
    mut guard: ReceiptGuard,
    mut task: TaskGuard,
    mut lease: Box<dyn SessionLease>,
    desired: CommitStatus,
    code: &str,
) -> CommitReceipt {
    let rollback = lease.rollback().await;
    let cleanup = lease.release().await;
    let known = rollback.is_ok() && cleanup.is_ok();
    guard.receipt.status = if known {
        desired
    } else {
        CommitStatus::Unknown
    };
    guard.receipt.commit_state = if !known {
        CommitState::Unknown
    } else if desired == CommitStatus::FailedBeforeCommit {
        CommitState::NotStarted
    } else {
        CommitState::RolledBack
    };
    guard.receipt.rollback_state = if rollback.is_ok() {
        CleanupState::Succeeded
    } else {
        CleanupState::Failed
    };
    guard.receipt.cleanup_state = if cleanup.is_ok() {
        CleanupState::Succeeded
    } else {
        CleanupState::Failed
    };
    guard.receipt.error_code = Some(if known { code } else { "ROLLBACK_UNKNOWN" }.into());
    for result in &mut guard.receipt.operation_results {
        if matches!(
            result.outcome,
            OperationOutcome::Applied | OperationOutcome::Unchanged | OperationOutcome::Unknown
        ) {
            result.outcome = if known {
                OperationOutcome::RolledBack
            } else {
                OperationOutcome::Unknown
            };
        }
    }
    let phase = if !known {
        TaskPhase::Unknown
    } else if code == "QUERY_CANCELLED" {
        TaskPhase::Cancelled
    } else {
        TaskPhase::Failed
    };
    let _ = task.finish(
        phase,
        guard.receipt.commit_state,
        guard.receipt.rollback_state,
        guard.receipt.cleanup_state,
        guard.receipt.error_code.as_deref(),
    );
    guard.finish()
}

async fn run_on_lease(
    manager: &ConnectionPoolManager,
    request: &ChangeSetRequest,
    context: &ResultContext,
    driver: &dyn DatabaseDriver,
    mut lease: Box<dyn SessionLease>,
    mut guard: ReceiptGuard,
    mut task: TaskGuard,
) -> CommitReceipt {
    if lease.begin_write().await.is_err() {
        return rolled_back(
            guard,
            task,
            lease,
            CommitStatus::FailedBeforeCommit,
            "BEGIN_FAILED",
        )
        .await;
    }
    let source = ResolvedTable {
        database: context.source.database.clone(),
        schema: context.source.schema.clone(),
        table: context.source.table.clone(),
        columns: vec![],
        object_kind: Some(context.source.kind.clone()),
    };
    let target = match lease.inspect_target(&source, true).await {
        Ok(target) => target,
        Err(_) => {
            return rolled_back(
                guard,
                task,
                lease,
                CommitStatus::FailedBeforeCommit,
                "OBJECT_IDENTITY_UNSUPPORTED",
            )
            .await
        }
    };
    let metadata =
        manager
            .result_contexts
            .metadata_fingerprint(&context.result_id, context.revision, now());
    let identity =
        manager
            .result_contexts
            .object_identity(&context.result_id, context.revision, now());
    if !identity
        .as_ref()
        .is_ok_and(|identity| identity == &object_fingerprint(&target.identity))
    {
        return rolled_back(
            guard,
            task,
            lease,
            CommitStatus::FailedBeforeCommit,
            "OBJECT_IDENTITY_CHANGED",
        )
        .await;
    }
    if !metadata.as_ref().is_ok_and(|fingerprint| {
        metadata_fingerprint(&target.table).is_ok_and(|actual| &actual == fingerprint)
    }) {
        return rolled_back(
            guard,
            task,
            lease,
            CommitStatus::FailedBeforeCommit,
            "METADATA_CHANGED",
        )
        .await;
    }
    if !target.write_reasons.is_empty() {
        guard.receipt.reason_codes = target.write_reasons;
        return rolled_back(
            guard,
            task,
            lease,
            CommitStatus::FailedBeforeCommit,
            "TARGET_WRITE_UNSUPPORTED",
        )
        .await;
    }
    let handles = request
        .operations
        .iter()
        .filter_map(|op| op.handle().map(str::to_owned))
        .collect::<Vec<_>>();
    let identities = match manager.validate_result_handles(context, &handles).await {
        Ok(identities) => handles
            .iter()
            .cloned()
            .zip(identities)
            .collect::<HashMap<_, _>>(),
        Err(error) => {
            return rolled_back(
                guard,
                task,
                lease,
                CommitStatus::FailedBeforeCommit,
                &error.code,
            )
            .await
        }
    };
    let plan = match changeset::compile(driver, context, &target.table, &identities, request) {
        Ok(plan) => plan,
        Err(error) => {
            return rolled_back(
                guard,
                task,
                lease,
                CommitStatus::FailedBeforeCommit,
                &error.code,
            )
            .await
        }
    };
    let mut skip = vec![false; request.operations.len()];
    // 先锁定并核对所有原行，再执行任何写入，避免前一操作改变后一操作的快照。
    for (i, op) in request.operations.iter().enumerate() {
        if task.cancel_requested() {
            return rolled_back(
                guard,
                task,
                lease,
                CommitStatus::RolledBack,
                "QUERY_CANCELLED",
            )
            .await;
        }
        let Some(handle) = op.handle() else { continue };
        let original = &identities[handle];
        if original.original_values.iter().any(Option::is_none) {
            return rolled_back(
                guard,
                task,
                lease,
                CommitStatus::FailedBeforeCommit,
                "ORIGINAL_VALUE_UNSUPPORTED",
            )
            .await;
        }
        let (sql, params) = select_row(driver, &target.table, original);
        let read = match lease.query_typed(&sql, &params).await {
            Ok(read) => read,
            Err(error) => {
                return rolled_back(
                    guard,
                    task,
                    lease,
                    CommitStatus::RolledBack,
                    if matches!(error, DatabaseError::QueryCancelled) {
                        "QUERY_CANCELLED"
                    } else {
                        "ROW_CHECK_FAILED"
                    },
                )
                .await
            }
        };
        let row_count = read.typed_rows.len();
        let result = &mut guard.receipt.operation_results[i];
        result.matched_rows = Some(row_count as u64);
        let conflict = if read.result.truncated || row_count > 1 {
            Some("MULTI_ROW_MATCH")
        } else if row_count == 0 {
            Some("ROW_DELETED")
        } else if read.typed_rows[0] != original.original_values {
            Some("ROW_CHANGED")
        } else {
            None
        };
        if let Some(code) = conflict {
            result.outcome = OperationOutcome::Conflict;
            result.error_code = Some(code.into());
            if row_count == 1
                && read.typed_rows[0]
                    .iter()
                    .flatten()
                    .map(TypedValue::allocated_bytes)
                    .sum::<usize>()
                    <= 64 * 1024
            {
                result.current_values = Some(read.typed_rows[0].clone());
            }
            return rolled_back(guard, task, lease, CommitStatus::Conflict, code).await;
        }
        skip[i] = unchanged(op, context, original);
    }
    for (i, operation) in plan.iter().enumerate() {
        if task.cancel_requested() {
            return rolled_back(
                guard,
                task,
                lease,
                CommitStatus::RolledBack,
                "QUERY_CANCELLED",
            )
            .await;
        }
        if skip[i] {
            let result = &mut guard.receipt.operation_results[i];
            result.outcome = OperationOutcome::Unchanged;
            result.affected_rows = Some(0);
            continue;
        }
        guard.writing();
        guard.receipt.operation_results[i].outcome = OperationOutcome::Unknown;
        let affected = match lease.execute_bound(&operation.sql, &operation.params).await {
            Ok(affected) => affected,
            Err(error) => {
                let code = if matches!(error, DatabaseError::QueryCancelled) {
                    "QUERY_CANCELLED"
                } else {
                    "OPERATION_FAILED"
                };
                guard.receipt.operation_results[i].error_code = Some(code.into());
                return rolled_back(guard, task, lease, CommitStatus::RolledBack, code).await;
            }
        };
        let mysql_unchanged = affected == 0
            && matches!(driver.driver_type(), "mysql" | "mariadb")
            && matches!(request.operations[i], ChangeOperation::Update { .. });
        let result = &mut guard.receipt.operation_results[i];
        result.affected_rows = Some(affected);
        if affected != 1 && !mysql_unchanged {
            result.error_code = Some("AFFECTED_ROWS_MISMATCH".into());
            return rolled_back(
                guard,
                task,
                lease,
                CommitStatus::RolledBack,
                "AFFECTED_ROWS_MISMATCH",
            )
            .await;
        }
        result.outcome = if mysql_unchanged {
            OperationOutcome::Unchanged
        } else {
            OperationOutcome::Applied
        };
    }
    if let Err(error) = manager.validate_result_handles(context, &[]).await {
        return rolled_back(guard, task, lease, CommitStatus::RolledBack, &error.code).await;
    }
    if task.committing().is_err() {
        return rolled_back(
            guard,
            task,
            lease,
            CommitStatus::RolledBack,
            "QUERY_CANCELLED",
        )
        .await;
    }
    // 提交一旦开始，原快照不再可用于新的变更；未知提交也必须重新读取，不能换 ID 重放。
    let _ = manager
        .result_contexts
        .release(&context.result_id, context.revision, now());
    guard.writing();
    if lease.commit().await.is_err() {
        guard.receipt.status = CommitStatus::Unknown;
        guard.receipt.commit_state = CommitState::Unknown;
        guard.receipt.error_code = Some("COMMIT_UNKNOWN".into());
        for result in &mut guard.receipt.operation_results {
            result.outcome = OperationOutcome::Unknown;
        }
        let cleanup = lease.release().await;
        guard.receipt.cleanup_state = if cleanup.is_ok() {
            CleanupState::Succeeded
        } else {
            CleanupState::Failed
        };
        let _ = task.finish(
            TaskPhase::Unknown,
            CommitState::Unknown,
            CleanupState::Unknown,
            guard.receipt.cleanup_state,
            Some("COMMIT_UNKNOWN"),
        );
        return guard.finish();
    }
    guard.committed();
    let _ = task.committed();
    let cleanup = lease.release().await;
    guard.receipt.cleanup_state = if cleanup.is_ok() {
        CleanupState::Succeeded
    } else {
        CleanupState::Failed
    };
    if cleanup.is_err() {
        guard.receipt.error_code = Some("COMMITTED_CLEANUP_FAILED".into());
    }
    let _ = task.finish(
        TaskPhase::Succeeded,
        CommitState::Committed,
        CleanupState::NotStarted,
        guard.receipt.cleanup_state,
        guard.receipt.error_code.as_deref(),
    );
    guard.finish()
}

#[cfg(test)]
#[path = "commit_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "commit_fault_tests.rs"]
mod fault_tests;

#[cfg(test)]
#[path = "commit_network_tests.rs"]
mod network_tests;
