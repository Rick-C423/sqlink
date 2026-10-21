//! 有界进程内提交回执。详情过期只回收载荷，不允许同一 ID 重新执行。

use crate::result_context::{ContextError, ResultContextRegistry};
use crate::task_registry::{CleanupState, CommitState};
use crate::typed_value::TypedValue;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum CommitStatus {
    Committed,
    Conflict,
    RolledBack,
    FailedBeforeCommit,
    Unknown,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum RefreshState {
    NotRequested,
    Required,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum OperationOutcome {
    Applied,
    Unchanged,
    Conflict,
    RolledBack,
    NotExecuted,
    Unknown,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OperationResult {
    pub op_id: String,
    pub outcome: OperationOutcome,
    pub matched_rows: Option<u64>,
    pub affected_rows: Option<u64>,
    pub error_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_values: Option<Vec<Option<TypedValue>>>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommitReceipt {
    pub protocol_version: u32,
    pub change_set_id: String,
    pub result_id: String,
    pub revision: u64,
    pub connection_id: String,
    pub connection_generation: String,
    pub task_id: Option<String>,
    pub status: CommitStatus,
    pub commit_state: CommitState,
    pub rollback_state: CleanupState,
    pub cleanup_state: CleanupState,
    pub refresh_state: RefreshState,
    pub operation_results: Vec<OperationResult>,
    pub error_code: Option<String>,
    pub reason_codes: Vec<String>,
    pub completed_at: i64,
}

struct Entry {
    digest: String,
    result_id: String,
    revision: u64,
    active: bool,
    receipt: Option<CommitReceipt>,
    bytes: usize,
    completed_at: i64,
}
#[derive(Default)]
struct State {
    entries: HashMap<String, Entry>,
    // 运行中、已提交或结果未知的快照不能换一个变更ID再次提交。
    context_claims: HashMap<(String, u64), String>,
    bytes: usize,
}

pub struct ReceiptRegistry {
    state: Mutex<State>,
    max_ids: usize,
    max_bytes: usize,
    ttl_ms: i64,
}
impl Default for ReceiptRegistry {
    fn default() -> Self {
        Self {
            state: Mutex::new(State::default()),
            max_ids: 4096,
            max_bytes: 16 * 1024 * 1024,
            ttl_ms: 30 * 60 * 1000,
        }
    }
}
fn err(code: &str, message: &str) -> ContextError {
    ContextError::new(code, message)
}
fn key(id: &str) -> Result<String, ContextError> {
    uuid::Uuid::parse_str(id)
        .map(|id| id.to_string())
        .map_err(|_| err("INVALID_REFERENCE", "提交引用无效"))
}
impl ReceiptRegistry {
    fn purge(&self, state: &mut State, now: i64) {
        // 摘要保留到进程结束；同 ID 详情过期后只能核查，不能重放。
        for entry in state.entries.values_mut() {
            if !entry.active && now.saturating_sub(entry.completed_at) >= self.ttl_ms {
                entry.receipt = None;
                state.bytes = state.bytes.saturating_sub(entry.bytes);
                entry.bytes = 0;
            }
        }
    }
    pub(crate) fn existing(
        &self,
        id: &str,
        digest: &str,
    ) -> Result<Option<CommitReceipt>, ContextError> {
        let id = key(id)?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| err("RECEIPT_UNAVAILABLE", "回执暂不可用，禁止重发"))?;
        self.purge(&mut state, chrono::Utc::now().timestamp_millis());
        let Some(entry) = state.entries.get(&id) else {
            return Ok(None);
        };
        if entry.digest != digest {
            return Err(err("CHANGESET_ID_REUSED", "同一变更 ID 不能用于不同内容"));
        }
        if let Some(receipt) = &entry.receipt {
            return Ok(Some(receipt.clone()));
        }
        Err(if entry.active {
            err("CHANGESET_IN_PROGRESS", "该变更正在执行，请查询原回执")
        } else {
            err("RECEIPT_EXPIRED", "回执详情已过期，禁止重发，请核查数据库")
        })
    }
    pub(crate) fn reserve(
        self: &Arc<Self>,
        digest: String,
        receipt: CommitReceipt,
    ) -> Result<ReceiptReservation, ContextError> {
        let id = key(&receipt.change_set_id)?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| err("RECEIPT_UNAVAILABLE", "回执暂不可用，禁止重发"))?;
        self.purge(&mut state, chrono::Utc::now().timestamp_millis());
        if let Some(entry) = state.entries.get(&id) {
            if entry.digest != digest {
                return Err(err("CHANGESET_ID_REUSED", "同一变更 ID 不能用于不同内容"));
            }
            return entry
                .receipt
                .clone()
                .map(ReceiptReservation::Existing)
                .ok_or_else(|| {
                    if entry.active {
                        err("CHANGESET_IN_PROGRESS", "该变更正在执行，请查询原回执")
                    } else {
                        err("RECEIPT_EXPIRED", "回执详情已过期，禁止重发")
                    }
                });
        }
        if state.entries.len() >= self.max_ids {
            return Err(err(
                "RECEIPT_BUDGET_EXCEEDED",
                "本进程提交引用已达上限，请先核查并保存回执",
            ));
        }
        let context_key = (receipt.result_id.clone(), receipt.revision);
        if state.context_claims.contains_key(&context_key) {
            return Err(err(
                "RESULT_CONTEXT_CONSUMED",
                "该结果已有正在执行或结果未知的提交，请核查原回执并重新读取",
            ));
        }
        // 来源标识也可能较长：先计入完整基础回执，再为结果码和最多64KiB冲突值的JSON转义预留空间。
        let base_bytes = serde_json::to_vec(&receipt)
            .map_err(|_| err("INVALID_RECEIPT", "回执无法计入预算"))?
            .len();
        let reservation_bytes = base_bytes
            .checked_add(512 * 1024)
            .ok_or_else(|| err("RECEIPT_BUDGET_EXCEEDED", "回执超过字节预算"))?;
        if reservation_bytes > self.max_bytes || state.bytes > self.max_bytes - reservation_bytes {
            return Err(err(
                "RECEIPT_BUDGET_EXCEEDED",
                "提交回执缓存已满，未执行写入",
            ));
        }
        state.bytes += reservation_bytes;
        state.context_claims.insert(context_key, id.clone());
        state.entries.insert(
            id.clone(),
            Entry {
                digest,
                result_id: receipt.result_id.clone(),
                revision: receipt.revision,
                active: true,
                receipt: None,
                bytes: reservation_bytes,
                completed_at: 0,
            },
        );
        Ok(ReceiptReservation::New(ReceiptGuard {
            registry: self.clone(),
            id,
            receipt,
            wrote: false,
            committed: false,
            finished: false,
            contexts: None,
        }))
    }
    pub fn get(
        &self,
        id: &str,
        result_id: &str,
        revision: u64,
    ) -> Result<CommitReceipt, ContextError> {
        let id = key(id)?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| err("RECEIPT_UNAVAILABLE", "回执暂不可用，禁止重发"))?;
        self.purge(&mut state, chrono::Utc::now().timestamp_millis());
        let entry = state
            .entries
            .get(&id)
            .ok_or_else(|| err("RECEIPT_NOT_FOUND", "没有本进程回执，不代表可以重新提交"))?;
        if entry.result_id != result_id || entry.revision != revision {
            return Err(err("RECEIPT_REFERENCE_MISMATCH", "回执不属于该结果版本"));
        }
        entry.receipt.clone().ok_or_else(|| {
            if entry.active {
                err("CHANGESET_IN_PROGRESS", "该变更仍在执行")
            } else {
                err("RECEIPT_EXPIRED", "回执详情已过期，请核查数据库，禁止重发")
            }
        })
    }
}

pub(crate) enum ReceiptReservation {
    Existing(CommitReceipt),
    New(ReceiptGuard),
}
pub(crate) struct ReceiptGuard {
    registry: Arc<ReceiptRegistry>,
    id: String,
    pub receipt: CommitReceipt,
    wrote: bool,
    committed: bool,
    finished: bool,
    contexts: Option<Arc<ResultContextRegistry>>,
}
impl ReceiptGuard {
    pub fn bind_contexts(&mut self, contexts: Arc<ResultContextRegistry>) {
        self.contexts = Some(contexts);
    }
    pub fn writing(&mut self) {
        self.wrote = true;
    }
    fn publish(&self, active: bool) {
        if matches!(
            self.receipt.status,
            CommitStatus::Unknown | CommitStatus::Committed
        ) {
            if let Some(contexts) = &self.contexts {
                let _ = contexts.release(
                    &self.receipt.result_id,
                    self.receipt.revision,
                    chrono::Utc::now().timestamp_millis(),
                );
            }
        }
        // panic 展开中也保留回执，不执行任何网络或落盘操作。
        let mut state = self
            .registry
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let actual_bytes = serde_json::to_vec(&self.receipt)
            .map(|bytes| bytes.len())
            .unwrap_or(512 * 1024);
        let previous_bytes = state
            .entries
            .get(&self.id)
            .map(|entry| entry.bytes)
            .unwrap_or(0);
        if let Some(entry) = state.entries.get_mut(&self.id) {
            entry.receipt = Some(self.receipt.clone());
            entry.active = active;
            entry.completed_at = self.receipt.completed_at;
            if !active {
                entry.bytes = actual_bytes;
            }
        }
        if !active {
            state.bytes = state
                .bytes
                .saturating_sub(previous_bytes)
                .saturating_add(actual_bytes);
            if !matches!(
                self.receipt.status,
                CommitStatus::Unknown | CommitStatus::Committed
            ) {
                let context_key = (self.receipt.result_id.clone(), self.receipt.revision);
                if state.context_claims.get(&context_key) == Some(&self.id) {
                    state.context_claims.remove(&context_key);
                }
            }
        }
    }
    pub fn committed(&mut self) {
        self.committed = true;
        self.receipt.status = CommitStatus::Committed;
        self.receipt.commit_state = CommitState::Committed;
        self.receipt.refresh_state = RefreshState::Required;
        self.receipt.cleanup_state = CleanupState::Unknown;
        self.receipt.completed_at = chrono::Utc::now().timestamp_millis();
        self.publish(true);
    }
    pub fn finish(mut self) -> CommitReceipt {
        self.receipt.completed_at = chrono::Utc::now().timestamp_millis();
        self.publish(false);
        self.finished = true;
        self.receipt.clone()
    }
}
impl Drop for ReceiptGuard {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        self.receipt.cleanup_state = CleanupState::Unknown;
        if self.committed {
            self.receipt.error_code = Some("COMMITTED_CLEANUP_UNKNOWN".into());
        } else if self.wrote {
            self.receipt.status = CommitStatus::Unknown;
            self.receipt.commit_state = CommitState::Unknown;
            self.receipt.error_code = Some("COMMIT_RESULT_UNKNOWN".into());
            for result in &mut self.receipt.operation_results {
                result.outcome = OperationOutcome::Unknown;
            }
        } else {
            self.receipt.status = CommitStatus::Unknown;
            self.receipt.commit_state = CommitState::Unknown;
            self.receipt.error_code = Some("TASK_ABANDONED_BEFORE_WRITE".into());
        }
        self.receipt.completed_at = chrono::Utc::now().timestamp_millis();
        self.publish(false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn receipt() -> CommitReceipt {
        CommitReceipt {
            protocol_version: 1,
            change_set_id: uuid::Uuid::new_v4().to_string(),
            result_id: uuid::Uuid::new_v4().to_string(),
            revision: 1,
            connection_id: "A".into(),
            connection_generation: uuid::Uuid::new_v4().to_string(),
            task_id: None,
            status: CommitStatus::FailedBeforeCommit,
            commit_state: CommitState::NotStarted,
            rollback_state: CleanupState::NotStarted,
            cleanup_state: CleanupState::Succeeded,
            refresh_state: RefreshState::NotRequested,
            operation_results: vec![],
            error_code: None,
            reason_codes: vec![],
            completed_at: chrono::Utc::now().timestamp_millis(),
        }
    }
    fn reserve(registry: &Arc<ReceiptRegistry>, receipt: CommitReceipt) -> ReceiptGuard {
        match registry.reserve("digest".into(), receipt).unwrap() {
            ReceiptReservation::New(guard) => guard,
            _ => panic!("测试必须预留新引用"),
        }
    }
    #[test]
    fn duplicate_requests_cannot_run_in_parallel_or_change_content() {
        let registry = Arc::new(ReceiptRegistry::default());
        let original = receipt();
        let guard = reserve(&registry, original.clone());
        assert_eq!(
            registry
                .existing(&original.change_set_id, "digest")
                .unwrap_err()
                .code,
            "CHANGESET_IN_PROGRESS"
        );
        assert_eq!(
            registry
                .existing(&original.change_set_id, "other")
                .unwrap_err()
                .code,
            "CHANGESET_ID_REUSED"
        );
        assert!(registry.reserve("digest".into(), original.clone()).is_err());
        let completed = guard.finish();
        assert_eq!(
            registry
                .existing(&original.change_set_id, "digest")
                .unwrap()
                .unwrap()
                .completed_at,
            completed.completed_at
        );
        assert_eq!(
            registry
                .get(&original.change_set_id, &original.result_id, 2)
                .unwrap_err()
                .code,
            "RECEIPT_REFERENCE_MISMATCH"
        );
    }

    #[test]
    fn different_ids_cannot_replay_active_or_unknown_contexts_even_after_details_expire() {
        let registry = Arc::new(ReceiptRegistry::default());
        let original = receipt();
        let mut guard = reserve(&registry, original.clone());
        let mut second = original.clone();
        second.change_set_id = uuid::Uuid::new_v4().to_string();
        assert_eq!(
            registry
                .reserve("another".into(), second.clone())
                .err()
                .unwrap()
                .code,
            "RESULT_CONTEXT_CONSUMED"
        );
        guard.writing();
        drop(guard);
        registry
            .state
            .lock()
            .unwrap()
            .entries
            .get_mut(&original.change_set_id)
            .unwrap()
            .completed_at = 0;
        assert!(registry
            .existing(&original.change_set_id, "digest")
            .is_err());
        assert_eq!(
            registry
                .reserve("another".into(), second)
                .err()
                .unwrap()
                .code,
            "RESULT_CONTEXT_CONSUMED"
        );
    }

    #[test]
    fn confirmed_before_commit_failure_releases_context_claim_without_forgetting_original_id() {
        let registry = Arc::new(ReceiptRegistry::default());
        let original = receipt();
        reserve(&registry, original.clone()).finish();
        let mut second = original.clone();
        second.change_set_id = uuid::Uuid::new_v4().to_string();
        assert!(matches!(
            registry.reserve("next".into(), second),
            Ok(ReceiptReservation::New(_))
        ));
        assert!(registry
            .existing(&original.change_set_id, "digest")
            .unwrap()
            .is_some());
    }
    #[test]
    fn expired_details_leave_tombstones_and_cannot_reexecute() {
        let registry = Arc::new(ReceiptRegistry::default());
        let original = receipt();
        reserve(&registry, original.clone()).finish();
        registry
            .state
            .lock()
            .unwrap()
            .entries
            .get_mut(&original.change_set_id)
            .unwrap()
            .completed_at = 0;
        assert_eq!(
            registry
                .existing(&original.change_set_id, "digest")
                .unwrap_err()
                .code,
            "RECEIPT_EXPIRED"
        );
        assert!(registry.reserve("digest".into(), original.clone()).is_err());
        assert_eq!(registry.state.lock().unwrap().entries.len(), 1);
        assert_eq!(registry.state.lock().unwrap().bytes, 0);
    }
    #[test]
    fn future_drop_preserves_unknown_but_confirmed_commit_stays_committed() {
        let registry = Arc::new(ReceiptRegistry::default());
        let original = receipt();
        let mut guard = reserve(&registry, original.clone());
        guard.writing();
        drop(guard);
        assert_eq!(
            registry
                .get(&original.change_set_id, &original.result_id, 1)
                .unwrap()
                .status,
            CommitStatus::Unknown
        );
        let original = receipt();
        let mut guard = reserve(&registry, original.clone());
        guard.committed();
        drop(guard);
        let stored = registry
            .get(&original.change_set_id, &original.result_id, 1)
            .unwrap();
        assert_eq!(stored.status, CommitStatus::Committed);
        assert_eq!(stored.cleanup_state, CleanupState::Unknown);
    }
    #[test]
    fn byte_budget_includes_long_source_identity_before_any_execution() {
        let registry = Arc::new(ReceiptRegistry {
            state: Mutex::new(State::default()),
            max_ids: 2,
            max_bytes: 1024 * 1024,
            ttl_ms: 30 * 60 * 1000,
        });
        let mut original = receipt();
        original.connection_id = "长".repeat(256 * 1024);
        assert_eq!(
            registry
                .reserve("digest".into(), original)
                .err()
                .unwrap()
                .code,
            "RECEIPT_BUDGET_EXCEEDED"
        );
        assert_eq!(registry.state.lock().unwrap().entries.len(), 0);
        let original = receipt();
        let guard = reserve(&registry, original.clone());
        assert!(registry.state.lock().unwrap().bytes > 512 * 1024);
        guard.finish();
        assert!(registry.state.lock().unwrap().bytes < 512 * 1024);
    }
}
