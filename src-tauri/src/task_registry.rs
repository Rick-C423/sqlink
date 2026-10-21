//! 进程内任务生命周期。取消请求不等于取消完成，COMMIT 附近的中断必须保留未知状态。

use crate::session::{SessionCancellation, SessionCapabilities, SessionLease};
use serde::Serialize;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum TaskPhase {
    Queued,
    Preparing,
    Running,
    Committing,
    Succeeded,
    Failed,
    Cancelled,
    Unknown,
}
impl TaskPhase {
    pub fn terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::Cancelled | Self::Unknown
        )
    }
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum CommitState {
    NotStarted,
    NotApplicable,
    Committed,
    RolledBack,
    Unknown,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum CleanupState {
    NotStarted,
    Succeeded,
    Failed,
    Unknown,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskProgress {
    pub stage: String,
    pub processed_bytes: u64,
    pub total_bytes: Option<u64>,
    pub processed_statements: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskSnapshot {
    pub protocol_version: u32,
    pub task_id: String,
    pub query_id: Option<u64>,
    pub connection_id: String,
    pub connection_generation: Option<String>,
    pub operation: String,
    pub phase: TaskPhase,
    pub sequence: u64,
    pub cancel_requested: bool,
    pub capabilities: Option<SessionCapabilities>,
    pub session_id: Option<String>,
    pub started_at: i64,
    pub completed_at: Option<i64>,
    pub commit_state: CommitState,
    pub rollback_state: CleanupState,
    pub cleanup_state: CleanupState,
    pub error_code: Option<String>,
    pub progress: Option<TaskProgress>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TaskError {
    pub code: String,
    pub message: String,
}
impl TaskError {
    pub fn new(code: &str, message: &str) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}
impl std::fmt::Display for TaskError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}
impl std::error::Error for TaskError {}

struct TaskRecord {
    snapshot: TaskSnapshot,
    cancel: Option<Arc<dyn SessionCancellation>>,
    legacy: bool,
    cancel_notify: Arc<tokio::sync::Notify>,
    order: u64,
}
#[derive(Default)]
struct RegistryState {
    tasks: HashMap<String, TaskRecord>,
    next_order: u64,
}

pub struct TaskRegistry {
    state: Mutex<RegistryState>,
    max_active: usize,
    max_terminal: usize,
    terminal_ttl_ms: i64,
}
impl Default for TaskRegistry {
    fn default() -> Self {
        Self::new(128, 256, 5 * 60 * 1000)
    }
}
impl TaskRegistry {
    pub fn new(max_active: usize, max_terminal: usize, terminal_ttl_ms: i64) -> Self {
        Self {
            state: Mutex::new(RegistryState::default()),
            max_active,
            max_terminal,
            terminal_ttl_ms,
        }
    }
    fn lock(&self) -> MutexGuard<'_, RegistryState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
    fn purge(&self, state: &mut RegistryState, now: i64) {
        state.tasks.retain(|_, r| {
            r.snapshot
                .completed_at
                .is_none_or(|time| now.saturating_sub(time) < self.terminal_ttl_ms)
        });
        let mut terminal: Vec<_> = state
            .tasks
            .iter()
            .filter_map(|(id, r)| {
                r.snapshot
                    .completed_at
                    .map(|time| (id.clone(), time, r.order))
            })
            .collect();
        terminal.sort_by_key(|(_, time, order)| (*time, *order));
        let excess = terminal.len().saturating_sub(self.max_terminal);
        for (id, _, _) in terminal.into_iter().take(excess) {
            state.tasks.remove(&id);
        }
    }
    pub fn start(
        self: &Arc<Self>,
        task_id: Option<String>,
        connection_id: &str,
        query_id: Option<u64>,
        operation: &str,
    ) -> Result<TaskGuard, TaskError> {
        let legacy = task_id.is_none();
        let id = task_id.unwrap_or_else(|| Uuid::new_v4().to_string());
        if Uuid::parse_str(&id).is_err() || connection_id.is_empty() {
            return Err(TaskError::new("INVALID_TASK", "任务身份或连接无效"));
        }
        let id = id.to_ascii_lowercase();
        let mut state = self.lock();
        self.purge(&mut state, chrono::Utc::now().timestamp_millis());
        if state.tasks.contains_key(&id) {
            return Err(TaskError::new(
                "TASK_ALREADY_EXISTS",
                "任务身份已使用，不能重绑定",
            ));
        }
        if state
            .tasks
            .values()
            .filter(|r| !r.snapshot.phase.terminal())
            .count()
            >= self.max_active
        {
            return Err(TaskError::new(
                "TASK_CAPACITY_EXCEEDED",
                "运行任务数已达上限",
            ));
        }
        if query_id.is_some()
            && state.tasks.values().any(|r| {
                r.snapshot.connection_id == connection_id && r.snapshot.query_id == query_id
            })
        {
            return Err(TaskError::new(
                "QUERY_ID_ALREADY_EXISTS",
                "该连接的查询标识已使用，请使用新的任务标识",
            ));
        }
        let order = state.next_order;
        state.next_order = state.next_order.saturating_add(1);
        state.tasks.insert(
            id.clone(),
            TaskRecord {
                legacy,
                order,
                cancel_notify: Arc::new(tokio::sync::Notify::new()),
                cancel: None,
                snapshot: TaskSnapshot {
                    protocol_version: 1,
                    task_id: id.clone(),
                    query_id,
                    connection_id: connection_id.into(),
                    connection_generation: None,
                    operation: operation.into(),
                    phase: TaskPhase::Queued,
                    sequence: 1,
                    cancel_requested: false,
                    capabilities: None,
                    session_id: None,
                    started_at: chrono::Utc::now().timestamp_millis(),
                    completed_at: None,
                    commit_state: CommitState::NotStarted,
                    rollback_state: CleanupState::NotStarted,
                    cleanup_state: CleanupState::NotStarted,
                    error_code: None,
                    progress: None,
                },
            },
        );
        Ok(TaskGuard {
            registry: Arc::clone(self),
            id,
            finished: false,
        })
    }
    pub fn snapshot(&self, id: &str) -> Result<TaskSnapshot, TaskError> {
        let mut state = self.lock();
        self.purge(&mut state, chrono::Utc::now().timestamp_millis());
        state
            .tasks
            .get(&id.to_ascii_lowercase())
            .map(|r| r.snapshot.clone())
            .ok_or_else(|| TaskError::new("TASK_NOT_FOUND", "任务不存在或已过期"))
    }
    pub fn active_count(&self) -> usize {
        self.lock()
            .tasks
            .values()
            .filter(|r| !r.snapshot.phase.terminal())
            .count()
    }
    pub fn resolve_cancel_id(
        &self,
        connection_id: &str,
        query_id: u64,
        task_id: Option<&str>,
    ) -> Result<String, TaskError> {
        let mut state = self.lock();
        self.purge(&mut state, chrono::Utc::now().timestamp_millis());
        let record = if let Some(id) = task_id {
            state.tasks.get(&id.to_ascii_lowercase())
        } else {
            state.tasks.values().find(|r| {
                r.legacy
                    && r.snapshot.connection_id == connection_id
                    && r.snapshot.query_id == Some(query_id)
            })
        }
        .ok_or_else(|| {
            TaskError::new("TASK_NOT_FOUND", "任务不存在，不能通过旧引用取消其他任务")
        })?;
        if record.snapshot.connection_id != connection_id
            || record.snapshot.query_id != Some(query_id)
        {
            return Err(TaskError::new(
                "TASK_SOURCE_MISMATCH",
                "取消来源与原任务不一致",
            ));
        }
        Ok(record.snapshot.task_id.clone())
    }
    pub async fn request_cancel(&self, id: &str) -> Result<TaskSnapshot, TaskError> {
        let handle = {
            let mut state = self.lock();
            let record = state
                .tasks
                .get_mut(&id.to_ascii_lowercase())
                .ok_or_else(|| TaskError::new("TASK_NOT_FOUND", "任务不存在或已过期"))?;
            if record.snapshot.phase.terminal() {
                return Err(TaskError::new("TASK_ALREADY_FINISHED", "任务已经结束"));
            }
            if record.snapshot.phase == TaskPhase::Committing {
                return Err(TaskError::new(
                    "COMMIT_IN_PROGRESS",
                    "正在提交，无法确认取消，等待提交结果后核查",
                ));
            }
            if record.snapshot.phase == TaskPhase::Running
                && record
                    .snapshot
                    .capabilities
                    .as_ref()
                    .is_some_and(|c| !c.cancellation)
            {
                return Err(TaskError::new(
                    "CANCELLATION_UNSUPPORTED",
                    "该驱动不支持取消正在执行的任务",
                ));
            }
            record.snapshot.cancel_requested = true;
            record.snapshot.sequence += 1;
            record.cancel_notify.notify_waiters();
            record.cancel.clone()
        };
        if let Some(handle) = handle {
            tokio::time::timeout(std::time::Duration::from_secs(5), handle.cancel())
                .await
                .map_err(|_| {
                    TaskError::new("CANCEL_REQUEST_TIMEOUT", "取消请求超时，执行状态仍需核查")
                })?
                .map_err(|_| {
                    TaskError::new(
                        "CANCEL_REQUEST_FAILED",
                        "取消请求未确认，请继续等待任务终态",
                    )
                })?;
        }
        self.snapshot(id)
    }
    /// 逻辑断开时只请求可支持的取消；不能把 SQL Server 等未中断任务标为已取消。
    pub async fn request_cancel_connection(&self, connection_id: &str) {
        let ids: Vec<_> = self
            .lock()
            .tasks
            .values()
            .filter(|r| r.snapshot.connection_id == connection_id && !r.snapshot.phase.terminal())
            .map(|r| r.snapshot.task_id.clone())
            .collect();
        for id in ids {
            let _ = self.request_cancel(&id).await;
        }
    }
}

pub struct TaskGuard {
    registry: Arc<TaskRegistry>,
    id: String,
    finished: bool,
}
impl TaskGuard {
    pub fn task_id(&self) -> &str {
        &self.id
    }
    pub fn snapshot(&self) -> Result<TaskSnapshot, TaskError> {
        self.registry.snapshot(&self.id)
    }
    fn update(
        &self,
        f: impl FnOnce(&mut TaskRecord) -> Result<(), TaskError>,
    ) -> Result<(), TaskError> {
        let mut state = self.registry.lock();
        let record = state
            .tasks
            .get_mut(&self.id)
            .ok_or_else(|| TaskError::new("TASK_NOT_FOUND", "任务不存在"))?;
        if record.snapshot.phase.terminal() {
            return Err(TaskError::new("TASK_ALREADY_FINISHED", "任务已经结束"));
        }
        f(record)?;
        record.snapshot.sequence += 1;
        Ok(())
    }
    pub fn prepare(
        &self,
        generation: &str,
        capabilities: SessionCapabilities,
    ) -> Result<(), TaskError> {
        self.update(|r| {
            if r.snapshot.phase != TaskPhase::Queued {
                return Err(TaskError::new(
                    "INVALID_TASK_TRANSITION",
                    "任务不能重复准备",
                ));
            }
            r.snapshot.connection_generation = Some(generation.into());
            r.snapshot.capabilities = Some(capabilities);
            r.snapshot.phase = TaskPhase::Preparing;
            Ok(())
        })
    }
    pub fn attach(&self, lease: &dyn SessionLease) -> Result<(), TaskError> {
        let capabilities = lease.capabilities();
        let cancellation = lease.cancel_handle();
        if !capabilities.exclusive_session
            || capabilities.cancellation != cancellation.is_some()
            || Uuid::parse_str(lease.session_id()).is_err()
        {
            return Err(TaskError::new(
                "SESSION_CAPABILITY_MISMATCH",
                "租约身份或取消能力与实际句柄不一致",
            ));
        }
        self.update(|r| {
            if r.snapshot.phase != TaskPhase::Preparing || r.snapshot.session_id.is_some() {
                return Err(TaskError::new(
                    "INVALID_TASK_TRANSITION",
                    "任务不能重绑定租约",
                ));
            }
            r.snapshot.session_id = Some(lease.session_id().into());
            r.snapshot.capabilities = Some(capabilities);
            r.cancel = cancellation;
            Ok(())
        })
    }
    pub fn cancel_requested(&self) -> bool {
        self.registry
            .lock()
            .tasks
            .get(&self.id)
            .is_some_and(|r| r.snapshot.cancel_requested)
    }
    pub async fn cancelled(&self) {
        let notify = {
            self.registry
                .lock()
                .tasks
                .get(&self.id)
                .map(|r| Arc::clone(&r.cancel_notify))
        };
        let Some(notify) = notify else {
            return;
        };
        loop {
            let notified = notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.cancel_requested() {
                return;
            }
            notified.await;
        }
    }
    pub fn running(&self) -> Result<(), TaskError> {
        self.update(|r| {
            if r.snapshot.phase != TaskPhase::Preparing {
                return Err(TaskError::new(
                    "INVALID_TASK_TRANSITION",
                    "任务不能进入执行阶段",
                ));
            }
            if r.snapshot.cancel_requested {
                return Err(TaskError::new(
                    "CANCELLED_BEFORE_EXECUTION",
                    "任务已取消，未执行 SQL",
                ));
            }
            r.snapshot.phase = TaskPhase::Running;
            Ok(())
        })
    }
    /// 仅登记固定阶段和计数，不持有 SQL、路径、参数或其他敏感业务值。
    pub fn progress(
        &self,
        stage: &str,
        processed_bytes: u64,
        total_bytes: Option<u64>,
        processed_statements: u64,
    ) -> Result<(), TaskError> {
        if ![
            "validating",
            "parsing",
            "executing",
            "cleanup",
            "reading",
            "counting",
        ]
        .contains(&stage)
            || total_bytes.is_some_and(|total| processed_bytes > total)
        {
            return Err(TaskError::new(
                "INVALID_TASK_PROGRESS",
                "任务阶段或计数无效",
            ));
        }
        self.update(|record| {
            record.snapshot.progress = Some(TaskProgress {
                stage: stage.into(),
                processed_bytes,
                total_bytes,
                processed_statements,
            });
            Ok(())
        })
    }
    /// 显式脚本可能包含自动/隐式提交，已发送 SQL 后不能继续宣称尚未执行。
    pub fn script_sql_sent(&self) -> Result<(), TaskError> {
        self.update(|record| {
            if record.snapshot.phase != TaskPhase::Running {
                return Err(TaskError::new(
                    "INVALID_TASK_TRANSITION",
                    "脚本任务尚未进入执行阶段",
                ));
            }
            if record.snapshot.cancel_requested {
                return Err(TaskError::new(
                    "CANCELLED_BEFORE_EXECUTION",
                    "已请求取消，未发送下一条脚本 SQL",
                ));
            }
            record.snapshot.commit_state = CommitState::Unknown;
            Ok(())
        })
    }
    pub fn committing(&self) -> Result<(), TaskError> {
        self.update(|r| {
            if r.snapshot.phase != TaskPhase::Running {
                return Err(TaskError::new(
                    "INVALID_TASK_TRANSITION",
                    "任务不能进入提交阶段",
                ));
            }
            if r.snapshot.cancel_requested {
                return Err(TaskError::new(
                    "CANCEL_REQUESTED",
                    "任务已请求取消，不能开始提交",
                ));
            }
            r.snapshot.phase = TaskPhase::Committing;
            r.snapshot.commit_state = CommitState::Unknown;
            Ok(())
        })
    }
    /// COMMIT 已收到成功回执后立即同步记录，不能让后续清理的取消抹掉已知提交。
    pub fn committed(&self) -> Result<(), TaskError> {
        self.update(|r| {
            if r.snapshot.phase != TaskPhase::Committing {
                return Err(TaskError::new(
                    "INVALID_TASK_TRANSITION",
                    "任务尚未进入提交阶段",
                ));
            }
            r.snapshot.commit_state = CommitState::Committed;
            Ok(())
        })
    }
    pub fn finish(
        &mut self,
        phase: TaskPhase,
        commit: CommitState,
        rollback: CleanupState,
        cleanup: CleanupState,
        error_code: Option<&str>,
    ) -> Result<TaskSnapshot, TaskError> {
        if !phase.terminal() {
            return Err(TaskError::new(
                "INVALID_TASK_TRANSITION",
                "必须提供任务终态",
            ));
        }
        self.update(|r| {
            if r.snapshot.phase == TaskPhase::Committing
                && !matches!(phase, TaskPhase::Succeeded | TaskPhase::Unknown)
            {
                return Err(TaskError::new(
                    "INVALID_TASK_TRANSITION",
                    "提交阶段失败必须保留未知状态",
                ));
            }
            r.snapshot.phase = phase;
            r.snapshot.commit_state = commit;
            r.snapshot.rollback_state = rollback;
            r.snapshot.cleanup_state = cleanup;
            r.snapshot.completed_at = Some(chrono::Utc::now().timestamp_millis());
            r.snapshot.error_code = error_code.map(str::to_owned);
            r.cancel = None;
            Ok(())
        })?;
        self.finished = true;
        self.snapshot()
    }
}
impl Drop for TaskGuard {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        let mut state = self.registry.lock();
        if let Some(record) = state.tasks.get_mut(&self.id) {
            if record.snapshot.phase.terminal() {
                return;
            }
            let committed = record.snapshot.commit_state == CommitState::Committed;
            let uncertain = !committed
                && matches!(
                    record.snapshot.phase,
                    TaskPhase::Running | TaskPhase::Committing
                );
            record.snapshot.phase = if committed {
                TaskPhase::Succeeded
            } else if uncertain {
                TaskPhase::Unknown
            } else {
                TaskPhase::Failed
            };
            if uncertain {
                record.snapshot.commit_state = CommitState::Unknown;
            }
            record.snapshot.cleanup_state = CleanupState::Unknown;
            record.snapshot.sequence += 1;
            record.snapshot.completed_at = Some(chrono::Utc::now().timestamp_millis());
            record.snapshot.error_code = Some(
                if committed {
                    "COMMITTED_CLEANUP_UNKNOWN"
                } else {
                    "TASK_ABANDONED"
                }
                .into(),
            );
            record.cancel = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn start(registry: &Arc<TaskRegistry>) -> TaskGuard {
        registry
            .start(Some(Uuid::new_v4().to_string()), "A", None, "query")
            .unwrap()
    }
    #[test]
    fn confirmed_commit_survives_drop_before_cleanup() {
        let registry = Arc::new(TaskRegistry::default());
        let task = start(&registry);
        let id = task.task_id().to_owned();
        task.prepare("gen", SessionCapabilities::supported())
            .unwrap();
        task.running().unwrap();
        task.committing().unwrap();
        task.committed().unwrap();
        drop(task);
        let state = registry.snapshot(&id).unwrap();
        assert_eq!(state.phase, TaskPhase::Succeeded);
        assert_eq!(state.commit_state, CommitState::Committed);
        assert_eq!(state.cleanup_state, CleanupState::Unknown);
        assert_eq!(
            state.error_code.as_deref(),
            Some("COMMITTED_CLEANUP_UNKNOWN")
        );
        assert_eq!(registry.active_count(), 0);
    }
    #[tokio::test]
    async fn cancellation_prevents_the_next_script_statement_without_erasing_prior_effects() {
        let registry = Arc::new(TaskRegistry::default());
        let task = start(&registry);
        task.prepare("gen", SessionCapabilities::supported())
            .unwrap();
        task.running().unwrap();
        // 第一条已发送，随后取消必须阻止第二条，同时保留前一条提交状态未知。
        task.script_sql_sent().unwrap();
        registry.request_cancel(task.task_id()).await.unwrap();
        assert_eq!(
            task.script_sql_sent().unwrap_err().code,
            "CANCELLED_BEFORE_EXECUTION"
        );
        let state = task.snapshot().unwrap();
        assert_eq!(state.phase, TaskPhase::Running);
        assert_eq!(state.commit_state, CommitState::Unknown);
        assert!(state.cancel_requested);
    }
    #[tokio::test]
    async fn requested_cancellation_is_not_a_terminal_state() {
        let registry = Arc::new(TaskRegistry::default());
        let mut task = start(&registry);
        let before = task.snapshot().unwrap();
        let requested = registry.request_cancel(task.task_id()).await.unwrap();
        assert!(requested.cancel_requested);
        assert_eq!(requested.phase, TaskPhase::Queued);
        assert!(requested.sequence > before.sequence);
        task.prepare("gen", SessionCapabilities::supported())
            .unwrap();
        assert!(task.running().is_err());
        let done = task
            .finish(
                TaskPhase::Cancelled,
                CommitState::NotStarted,
                CleanupState::NotStarted,
                CleanupState::Succeeded,
                Some("QUERY_CANCELLED"),
            )
            .unwrap();
        assert_eq!(done.phase, TaskPhase::Cancelled);
        assert_eq!(registry.active_count(), 0);
    }
    #[tokio::test]
    async fn unsupported_and_committing_cancellation_cannot_claim_success() {
        let registry = Arc::new(TaskRegistry::default());
        let mut task = start(&registry);
        task.prepare("gen", SessionCapabilities::without_cancellation("不支持"))
            .unwrap();
        task.running().unwrap();
        assert_eq!(
            registry
                .request_cancel(task.task_id())
                .await
                .unwrap_err()
                .code,
            "CANCELLATION_UNSUPPORTED"
        );
        assert!(!task.cancel_requested());
        task.committing().unwrap();
        assert_eq!(
            registry
                .request_cancel(task.task_id())
                .await
                .unwrap_err()
                .code,
            "COMMIT_IN_PROGRESS"
        );
        assert!(task
            .finish(
                TaskPhase::Failed,
                CommitState::Unknown,
                CleanupState::Failed,
                CleanupState::Failed,
                None
            )
            .is_err());
        task.finish(
            TaskPhase::Unknown,
            CommitState::Unknown,
            CleanupState::Unknown,
            CleanupState::Unknown,
            Some("COMMIT_UNKNOWN"),
        )
        .unwrap();
    }
    #[test]
    fn task_identity_source_duplicate_and_drop_are_bounded() {
        let registry = Arc::new(TaskRegistry::new(1, 2, 300_000));
        let id = Uuid::new_v4().to_string();
        let task = registry
            .start(Some(id.clone()), "A", Some(7), "query")
            .unwrap();
        assert!(registry
            .start(Some(id.clone()), "B", Some(8), "query")
            .is_err());
        assert!(registry.start(None, "A", Some(9), "query").is_err());
        assert!(registry.resolve_cancel_id("B", 7, Some(&id)).is_err());
        assert!(registry.resolve_cancel_id("A", 7, None).is_err());
        assert_eq!(registry.resolve_cancel_id("A", 7, Some(&id)).unwrap(), id);
        drop(task);
        assert_eq!(registry.active_count(), 0);
        assert_eq!(registry.snapshot(&id).unwrap().phase, TaskPhase::Failed);
        let task = start(&registry);
        task.prepare("gen", SessionCapabilities::supported())
            .unwrap();
        task.running().unwrap();
        let id = task.task_id().to_owned();
        drop(task);
        let done = registry.snapshot(&id).unwrap();
        assert_eq!(done.phase, TaskPhase::Unknown);
        assert_eq!(done.commit_state, CommitState::Unknown);
        let mut task = start(&registry);
        task.prepare("gen", SessionCapabilities::supported())
            .unwrap();
        assert!(task
            .prepare("replacement", SessionCapabilities::supported())
            .is_err());
        task.finish(
            TaskPhase::Failed,
            CommitState::NotStarted,
            CleanupState::NotStarted,
            CleanupState::Succeeded,
            None,
        )
        .unwrap();
        let mut task = start(&registry);
        task.finish(
            TaskPhase::Failed,
            CommitState::NotStarted,
            CleanupState::NotStarted,
            CleanupState::Succeeded,
            None,
        )
        .unwrap();
        assert!(registry.snapshot(&id).is_err());
    }
    #[tokio::test]
    async fn future_abort_and_panic_clear_active_registration() {
        let registry = Arc::new(TaskRegistry::default());
        let task = start(&registry);
        let id = task.task_id().to_owned();
        task.prepare("gen", SessionCapabilities::supported())
            .unwrap();
        task.running().unwrap();
        let (ready, wait) = tokio::sync::oneshot::channel();
        let join = tokio::spawn(async move {
            let _guard = task;
            let _ = ready.send(());
            std::future::pending::<()>().await;
        });
        wait.await.unwrap();
        join.abort();
        let _ = join.await;
        assert_eq!(registry.active_count(), 0);
        assert_eq!(registry.snapshot(&id).unwrap().phase, TaskPhase::Unknown);
        let task = start(&registry);
        let id = task.task_id().to_owned();
        let join = tokio::spawn(async move {
            let _guard = task;
            panic!("任务生命周期回归故障注入")
        });
        assert!(join.await.is_err());
        assert_eq!(registry.active_count(), 0);
        assert_eq!(registry.snapshot(&id).unwrap().phase, TaskPhase::Failed);
    }
}
