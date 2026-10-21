//! 后端可信结果注册表。公开对象只含随机引用，行主键及原值始终保留在进程内。

use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, MutexGuard};
use uuid::Uuid;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResultSource {
    pub database: String,
    pub schema: Option<String>,
    pub table: String,
    pub kind: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ColumnOrigin {
    pub database: String,
    pub schema: Option<String>,
    pub table: String,
    pub column: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResultColumn {
    pub column_id: String,
    pub display_name: String,
    pub database_type: String,
    pub nullable: bool,
    pub origin: Option<ColumnOrigin>,
    pub writable: bool,
    pub generated: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EditPolicy {
    pub allowed_operations: Vec<String>,
    pub reason_codes: Vec<String>,
    pub column_input_kinds: HashMap<String, String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResultContext {
    pub protocol_version: u32,
    pub result_id: String,
    pub revision: u64,
    pub connection_id: String,
    pub connection_generation: String,
    pub source: ResultSource,
    pub columns: Vec<ResultColumn>,
    pub edit_policy: EditPolicy,
    pub created_at: i64,
    pub expires_at: i64,
}

/// 只供后端构造；不实现反序列化，前端不能提供来源或重新绑定上下文。
pub struct ResultContextSeed {
    pub revision: u64,
    pub connection_id: String,
    pub connection_generation: String,
    pub source: ResultSource,
    pub columns: Vec<ResultColumn>,
    pub metadata_fingerprint: String,
    pub reason_codes: Vec<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PublishedResultContext {
    pub context: ResultContext,
    pub row_handles: Vec<Option<String>>,
}

/// 定位值复用无损单元格类型；IPC 不接受前端传回的键映射。
pub use crate::typed_value::TypedValue as RowIdentityValue;

/// Serialize 仅用于进程内预算计算；键映射和原快照不得作为命令返回类型。
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct StoredRowIdentity {
    pub keys: Vec<(String, RowIdentityValue)>,
    /// None 表示该列无法无损读取；SQL NULL 使用 Some(TypedValue::Null)。
    pub original_values: Vec<Option<crate::typed_value::TypedValue>>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ContextError {
    pub code: String,
    pub message: String,
}

impl ContextError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ContextError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for ContextError {}

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub ttl_ms: i64,
    pub max_contexts: usize,
    pub max_row_handles: usize,
    pub max_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            ttl_ms: 30 * 60 * 1000,
            max_contexts: 128,
            max_row_handles: 100_000,
            max_bytes: 64 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RegistryStats {
    pub contexts: usize,
    pub row_handles: usize,
    pub bytes: usize,
}

struct StoredContext {
    context: ResultContext,
    metadata_fingerprint: String,
    object_identity: Option<String>,
    identities: HashMap<String, StoredRowIdentity>,
    // 计入无身份行，防止大量 None 行绕过结果行数上限。
    row_count: usize,
    bytes: usize,
    order: u128,
}

#[derive(Default)]
struct RegistryState {
    results: HashMap<String, StoredContext>,
    rows: usize,
    bytes: usize,
    next_order: u128,
}

impl RegistryState {
    fn remove(&mut self, result_id: &str) {
        if let Some(removed) = self.results.remove(result_id) {
            self.rows -= removed.row_count;
            self.bytes -= removed.bytes;
        }
    }

    fn purge_expired(&mut self, now_ms: i64) {
        // 上下文数量有硬上限；不扫描任何行身份。
        let expired: Vec<String> = self
            .results
            .iter()
            .filter(|(_, stored)| now_ms >= stored.context.expires_at)
            .map(|(id, _)| id.clone())
            .collect();
        for id in expired {
            self.remove(&id);
        }
    }

    fn lookup(&self, result_id: &str, revision: u64) -> Result<&StoredContext, ContextError> {
        let stored = self.results.get(result_id).ok_or_else(|| {
            ContextError::new("RESULT_CONTEXT_NOT_FOUND", "结果上下文不存在或已回收")
        })?;
        if stored.context.revision != revision {
            return Err(ContextError::new("STALE_REVISION", "结果版本已失效"));
        }
        Ok(stored)
    }
}

pub struct ResultContextRegistry {
    limits: Limits,
    state: Mutex<RegistryState>,
}

impl Default for ResultContextRegistry {
    fn default() -> Self {
        Self::new(Limits::default())
    }
}

impl ResultContextRegistry {
    pub fn new(limits: Limits) -> Self {
        Self {
            limits,
            state: Mutex::new(RegistryState::default()),
        }
    }

    fn lock(&self) -> Result<MutexGuard<'_, RegistryState>, ContextError> {
        self.state
            .lock()
            .map_err(|_| ContextError::new("REGISTRY_UNAVAILABLE", "结果注册表暂不可用"))
    }

    fn lock_live(
        &self,
        result_id: &str,
        now_ms: i64,
    ) -> Result<MutexGuard<'_, RegistryState>, ContextError> {
        let mut state = self.lock()?;
        let expired = state
            .results
            .get(result_id)
            .is_some_and(|stored| now_ms >= stored.context.expires_at);
        state.purge_expired(now_ms);
        if expired {
            return Err(ContextError::new(
                "RESULT_CONTEXT_EXPIRED",
                "结果上下文已过期，请重新查询",
            ));
        }
        Ok(state)
    }

    pub fn register(
        &self,
        seed: ResultContextSeed,
        rows: Vec<Option<StoredRowIdentity>>,
        now_ms: i64,
    ) -> Result<PublishedResultContext, ContextError> {
        self.register_with_identity(seed, rows, now_ms, None, None)
    }

    pub(crate) fn register_with_identity(
        &self,
        mut seed: ResultContextSeed,
        rows: Vec<Option<StoredRowIdentity>>,
        now_ms: i64,
        object_identity: Option<String>,
        policy: Option<EditPolicy>,
    ) -> Result<PublishedResultContext, ContextError> {
        if object_identity
            .as_ref()
            .is_some_and(|identity| identity.is_empty() || identity.len() > 64 * 1024)
        {
            return Err(ContextError::new(
                "INVALID_OBJECT_IDENTITY",
                "对象身份无效或超限",
            ));
        }
        if self.limits.ttl_ms <= 0
            || self.limits.max_contexts == 0
            || rows.len() > self.limits.max_row_handles
        {
            return Err(ContextError::new(
                "CONTEXT_BUDGET_EXCEEDED",
                "结果超过注册表数量或生存期限制",
            ));
        }
        if !matches!(seed.source.kind.as_str(), "table" | "view" | "unknown")
            || Uuid::parse_str(&seed.connection_generation).is_err()
        {
            return Err(ContextError::new(
                "INVALID_CONTEXT_SEED",
                "结果来源或连接代次无效",
            ));
        }
        if rows.iter().flatten().any(|row| {
            row.keys.is_empty()
                || row.keys.iter().any(|(_, value)| {
                    matches!(value, RowIdentityValue::Null | RowIdentityValue::Float(_))
                        || value.validate().is_err()
                })
                || row
                    .original_values
                    .iter()
                    .flatten()
                    .any(|value| value.validate().is_err())
        }) {
            return Err(ContextError::new(
                "INVALID_ROW_IDENTITY",
                "可信行身份必须包含主键",
            ));
        }
        let expires_at = now_ms
            .checked_add(self.limits.ttl_ms)
            .ok_or_else(|| ContextError::new("INVALID_CONTEXT_TIME", "结果生存期超出支持范围"))?;
        let authorized =
            policy.is_some() && object_identity.is_some() && seed.source.kind == "table";
        for column in &mut seed.columns {
            column.writable = authorized
                && !column.generated
                && policy
                    .as_ref()
                    .is_some_and(|p| p.column_input_kinds.contains_key(&column.column_id));
        }
        if !authorized
            && !seed
                .reason_codes
                .iter()
                .any(|reason| reason == "PARAMETERIZED_COMMIT_REQUIRED")
        {
            seed.reason_codes
                .push("PARAMETERIZED_COMMIT_REQUIRED".to_owned());
        }
        let context = ResultContext {
            protocol_version: 1,
            result_id: Uuid::new_v4().to_string(),
            revision: seed.revision,
            connection_id: seed.connection_id,
            connection_generation: seed.connection_generation,
            source: seed.source,
            columns: seed.columns,
            edit_policy: policy.filter(|_| authorized).unwrap_or_else(|| EditPolicy {
                allowed_operations: vec![],
                reason_codes: seed.reason_codes,
                column_input_kinds: HashMap::new(),
            }),
            created_at: now_ms,
            expires_at,
        };
        // 预算计算与随机句柄分配在锁外执行。序列化仅计数，不复制敏感原值。
        let mut counter = ByteCounter {
            bytes: 0,
            limit: self.limits.max_bytes / 4,
        };
        serde_json::to_writer(
            &mut counter,
            &(
                &context,
                &seed.metadata_fingerprint,
                &object_identity,
                &rows,
            ),
        )
        .map_err(|_| ContextError::new("CONTEXT_BUDGET_EXCEEDED", "结果大小无法计入预算"))?;
        // 保守计入 JSON 树、字符串和哈希节点的容器开销，包含空身份行。
        let bytes = counter
            .bytes
            .checked_mul(4)
            .and_then(|value| value.checked_add(identity_container_bytes(&rows)?))
            .and_then(|value| value.checked_add(rows.len().checked_mul(256)?))
            .and_then(|value| value.checked_add(context.columns.len().checked_mul(256)?))
            .and_then(|value| value.checked_add(1024))
            .ok_or_else(|| ContextError::new("CONTEXT_BUDGET_EXCEEDED", "结果超过字节预算"))?;
        if bytes > self.limits.max_bytes {
            return Err(ContextError::new(
                "CONTEXT_BUDGET_EXCEEDED",
                "单个结果超过字节预算",
            ));
        }
        let row_count = rows.len();
        let mut identities = HashMap::new();
        let row_handles = rows
            .into_iter()
            .map(|row| {
                row.map(|identity| {
                    let handle = Uuid::new_v4().to_string();
                    identities.insert(handle.clone(), identity);
                    handle
                })
            })
            .collect();
        let mut state = self.lock()?;
        state.purge_expired(now_ms);
        // 先完成全部可失败校验，再淘汰旧结果；拒绝注册不会损坏现有记录。
        while state.results.len() >= self.limits.max_contexts
            || state.rows > self.limits.max_row_handles - row_count
            || state.bytes > self.limits.max_bytes - bytes
        {
            let oldest = state
                .results
                .iter()
                .min_by_key(|(_, value)| value.order)
                .map(|(id, _)| id.clone());
            match oldest {
                Some(id) => state.remove(&id),
                None => break,
            }
        }
        let order = state.next_order;
        state.next_order = state.next_order.saturating_add(1);
        state.rows += row_count;
        state.bytes += bytes;
        state.results.insert(
            context.result_id.clone(),
            StoredContext {
                context: context.clone(),
                metadata_fingerprint: seed.metadata_fingerprint,
                object_identity,
                identities,
                row_count,
                bytes,
                order,
            },
        );
        Ok(PublishedResultContext {
            context,
            row_handles,
        })
    }

    pub fn get_context(
        &self,
        result_id: &str,
        revision: u64,
        now_ms: i64,
    ) -> Result<ResultContext, ContextError> {
        let state = self.lock_live(result_id, now_ms)?;
        Ok(state.lookup(result_id, revision)?.context.clone())
    }

    pub fn metadata_fingerprint(
        &self,
        result_id: &str,
        revision: u64,
        now_ms: i64,
    ) -> Result<String, ContextError> {
        let state = self.lock_live(result_id, now_ms)?;
        Ok(state
            .lookup(result_id, revision)?
            .metadata_fingerprint
            .clone())
    }

    pub(crate) fn object_identity(
        &self,
        result_id: &str,
        revision: u64,
        now_ms: i64,
    ) -> Result<String, ContextError> {
        let state = self.lock_live(result_id, now_ms)?;
        state
            .lookup(result_id, revision)?
            .object_identity
            .clone()
            .ok_or_else(|| {
                ContextError::new(
                    "OBJECT_IDENTITY_REQUIRED",
                    "该结果没有同会话对象身份，请重新读取或保持只读",
                )
            })
    }

    pub fn validate(
        &self,
        result_id: &str,
        revision: u64,
        connection_id: &str,
        current_generation: &str,
        handles: &[String],
        now_ms: i64,
    ) -> Result<Vec<StoredRowIdentity>, ContextError> {
        let state = self.lock_live(result_id, now_ms)?;
        let stored = state.lookup(result_id, revision)?;
        if stored.context.connection_id != connection_id {
            return Err(ContextError::new(
                "CONNECTION_MISMATCH",
                "结果不属于当前连接",
            ));
        }
        if stored.context.connection_generation != current_generation {
            return Err(ContextError::new(
                "STALE_CONNECTION_GENERATION",
                "连接已重建，请重新查询",
            ));
        }
        // 全部验证后才克隆；重复句柄也有数量上限，防止放大内存。
        if handles.len() > self.limits.max_row_handles {
            return Err(ContextError::new(
                "CONTEXT_BUDGET_EXCEEDED",
                "请求的行句柄数量超过限制",
            ));
        }
        let mut unique = HashSet::with_capacity(handles.len());
        for handle in handles {
            if !unique.insert(handle.as_str()) {
                return Err(ContextError::new(
                    "DUPLICATE_ROW_HANDLE",
                    "同一请求不能重复引用行句柄",
                ));
            }
            if !stored.identities.contains_key(handle) {
                return Err(ContextError::new(
                    "INVALID_ROW_HANDLE",
                    "行句柄无效或不属于当前结果",
                ));
            }
        }
        Ok(handles
            .iter()
            .map(|handle| stored.identities[handle].clone())
            .collect())
    }

    pub fn release(&self, result_id: &str, revision: u64, now_ms: i64) -> Result<(), ContextError> {
        let mut state = self.lock()?;
        state.purge_expired(now_ms);
        if state.results.contains_key(result_id) {
            state.lookup(result_id, revision)?;
        }
        state.remove(result_id);
        Ok(())
    }

    pub fn invalidate_connection(
        &self,
        connection_id: &str,
        now_ms: i64,
    ) -> Result<(), ContextError> {
        let mut state = self.lock()?;
        state.purge_expired(now_ms);
        let affected: Vec<String> = state
            .results
            .iter()
            .filter(|(_, stored)| stored.context.connection_id == connection_id)
            .map(|(id, _)| id.clone())
            .collect();
        for id in affected {
            state.remove(&id);
        }
        Ok(())
    }

    pub fn stats(&self, now_ms: i64) -> Result<RegistryStats, ContextError> {
        let mut state = self.lock()?;
        state.purge_expired(now_ms);
        Ok(RegistryStats {
            contexts: state.results.len(),
            row_handles: state.rows,
            bytes: state.bytes,
        })
    }
}

/// JSON 紧凑数字可能比 Value 容器小很多，额外计入实际容器容量。
fn identity_container_bytes(rows: &[Option<StoredRowIdentity>]) -> Option<usize> {
    let mut total = 0usize;
    for row in rows.iter().flatten() {
        total = total.checked_add(
            row.keys
                .capacity()
                .checked_mul(std::mem::size_of::<(String, RowIdentityValue)>())?,
        )?;
        for (name, key) in &row.keys {
            total = total.checked_add(name.capacity())?;
            total = total.checked_add(key.allocated_bytes())?;
        }
        total = total.checked_add(
            row.original_values
                .capacity()
                .checked_mul(std::mem::size_of::<Option<crate::typed_value::TypedValue>>())?,
        )?;
        for value in row.original_values.iter().flatten() {
            total = total.checked_add(value.allocated_bytes())?;
        }
    }
    Some(total)
}

struct ByteCounter {
    bytes: usize,
    limit: usize,
}

impl std::io::Write for ByteCounter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.bytes = self
            .bytes
            .checked_add(bytes.len())
            .filter(|value| *value <= self.limit)
            .ok_or_else(|| std::io::Error::other("结果超过字节预算"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed() -> ResultContextSeed {
        ResultContextSeed {
            revision: 7,
            connection_id: "连接甲".to_owned(),
            connection_generation: "1388ef41-659d-43fa-ab20-e7536f45a620".to_owned(),
            source: ResultSource {
                database: "库".to_owned(),
                schema: None,
                table: "表".to_owned(),
                kind: "table".to_owned(),
            },
            columns: vec![ResultColumn {
                column_id: "c0".to_owned(),
                display_name: "名称".to_owned(),
                database_type: "TEXT".to_owned(),
                nullable: false,
                origin: Some(ColumnOrigin {
                    database: "库".to_owned(),
                    schema: None,
                    table: "表".to_owned(),
                    column: "名称".to_owned(),
                }),
                writable: true,
                generated: false,
            }],
            metadata_fingerprint: "后端元数据指纹".to_owned(),
            reason_codes: vec!["NO_PRIMARY_KEY".to_owned()],
        }
    }

    fn identity() -> StoredRowIdentity {
        StoredRowIdentity {
            keys: vec![
                (
                    "主键甲".to_owned(),
                    RowIdentityValue::Integer("9223372036854775807".to_owned()),
                ),
                (
                    "主键乙".to_owned(),
                    RowIdentityValue::UnsignedInteger("18446744073709551615".to_owned()),
                ),
                (
                    "主键丙".to_owned(),
                    RowIdentityValue::Text("secret-key".to_owned()),
                ),
                ("主键丁".to_owned(), RowIdentityValue::Boolean(true)),
            ],
            original_values: vec![Some(RowIdentityValue::Text("secret-original".into())), None],
        }
    }

    fn register(registry: &ResultContextRegistry, now: i64) -> PublishedResultContext {
        registry
            .register(seed(), vec![None, Some(identity()), None], now)
            .unwrap()
    }

    fn validate(
        registry: &ResultContextRegistry,
        published: &PublishedResultContext,
        now: i64,
    ) -> Result<Vec<StoredRowIdentity>, ContextError> {
        registry.validate(
            &published.context.result_id,
            7,
            &published.context.connection_id,
            &published.context.connection_generation,
            &[published.row_handles[1].clone().unwrap()],
            now,
        )
    }

    #[test]
    fn published_protocol_is_read_only_and_never_contains_identity() {
        let registry = ResultContextRegistry::default();
        let published = register(&registry, 10);
        assert_eq!(published.row_handles.len(), 3);
        assert!(published.row_handles[0].is_none());
        assert!(published.row_handles[2].is_none());
        assert!(Uuid::parse_str(&published.context.result_id).is_ok());
        assert!(Uuid::parse_str(published.row_handles[1].as_ref().unwrap()).is_ok());
        assert!(!published.context.columns[0].writable);
        assert!(published.context.edit_policy.allowed_operations.is_empty());
        assert!(published
            .context
            .edit_policy
            .reason_codes
            .iter()
            .any(|reason| reason == "PARAMETERIZED_COMMIT_REQUIRED"));
        assert!(published
            .context
            .edit_policy
            .reason_codes
            .iter()
            .any(|reason| reason == "NO_PRIMARY_KEY"));
        let json = serde_json::to_string(&published).unwrap();
        for secret in [
            "secret-key",
            "secret-original",
            "9223372036854775807",
            "18446744073709551615",
            "后端元数据指纹",
        ] {
            assert!(!json.contains(secret));
        }
        assert!(json.contains("protocolVersion"));
        assert!(json.contains("rowHandles"));
        assert_eq!(published.context.expires_at, 10 + 30 * 60 * 1000);
    }

    #[test]
    fn exact_keys_and_original_values_survive_display_changes() {
        let registry = ResultContextRegistry::default();
        let mut published = register(&registry, 0);
        published.context.columns[0].display_name = "前端修改过的显示列".to_owned();
        published.context.source.table = "伪造表".to_owned();
        assert_eq!(
            validate(&registry, &published, 1).unwrap(),
            vec![identity()]
        );
        let stored = registry
            .get_context(&published.context.result_id, 7, 1)
            .unwrap();
        assert_eq!(stored.source.table, "表");
        assert_eq!(stored.columns[0].display_name, "名称");
        assert_eq!(
            registry
                .metadata_fingerprint(&stored.result_id, 7, 1)
                .unwrap(),
            "后端元数据指纹"
        );
    }

    #[test]
    fn scope_and_handle_errors_are_structured() {
        let registry = ResultContextRegistry::default();
        let first = register(&registry, 0);
        let second = register(&registry, 1);
        let id = &first.context.result_id;
        let generation = &first.context.connection_generation;
        let handles = [first.row_handles[1].clone().unwrap()];
        assert_eq!(
            registry
                .validate(id, 8, "连接甲", generation, &handles, 2)
                .unwrap_err()
                .code,
            "STALE_REVISION"
        );
        assert_eq!(
            registry
                .validate(id, 7, "连接乙", generation, &handles, 2)
                .unwrap_err()
                .code,
            "CONNECTION_MISMATCH"
        );
        assert_eq!(
            registry
                .validate(id, 7, "连接甲", "其他代次", &handles, 2)
                .unwrap_err()
                .code,
            "STALE_CONNECTION_GENERATION"
        );
        assert_eq!(
            registry
                .validate(
                    &second.context.result_id,
                    7,
                    "连接甲",
                    generation,
                    &handles,
                    2
                )
                .unwrap_err()
                .code,
            "INVALID_ROW_HANDLE"
        );
        assert_eq!(
            registry
                .validate(id, 7, "连接甲", generation, &["伪造".to_owned()], 2)
                .unwrap_err()
                .code,
            "INVALID_ROW_HANDLE"
        );
        assert_eq!(
            registry
                .validate(
                    id,
                    7,
                    "连接甲",
                    generation,
                    &[handles[0].clone(), handles[0].clone()],
                    2
                )
                .unwrap_err()
                .code,
            "DUPLICATE_ROW_HANDLE"
        );
        assert_eq!(
            registry.get_context("不存在", 7, 2).unwrap_err().code,
            "RESULT_CONTEXT_NOT_FOUND"
        );
        assert!(validate(&registry, &first, 2).is_ok());
    }

    #[test]
    fn ttl_boundary_purges_all_expired_records_without_sleep() {
        let registry = ResultContextRegistry::new(Limits {
            ttl_ms: 10,
            ..Limits::default()
        });
        let first = register(&registry, 100);
        let second = register(&registry, 101);
        assert!(validate(&registry, &first, 109).is_ok());
        assert_eq!(
            validate(&registry, &first, 110).unwrap_err().code,
            "RESULT_CONTEXT_EXPIRED"
        );
        assert_eq!(registry.stats(110).unwrap().contexts, 1);
        assert_eq!(
            registry
                .metadata_fingerprint(&second.context.result_id, 7, 111)
                .unwrap_err()
                .code,
            "RESULT_CONTEXT_EXPIRED"
        );
        assert_eq!(registry.stats(111).unwrap(), RegistryStats::default());
    }

    #[test]
    fn release_is_idempotent_and_wrong_revision_preserves_record() {
        let registry = ResultContextRegistry::default();
        let result = register(&registry, 0);
        let id = &result.context.result_id;
        assert_eq!(
            registry.release(id, 8, 1).unwrap_err().code,
            "STALE_REVISION"
        );
        assert!(validate(&registry, &result, 1).is_ok());
        registry.release(id, 7, 1).unwrap();
        registry.release(id, 7, 1).unwrap();
        registry.release("不存在", 0, 1).unwrap();
        assert_eq!(registry.stats(1).unwrap(), RegistryStats::default());
    }

    #[test]
    fn connection_invalidation_removes_only_its_results() {
        let registry = ResultContextRegistry::default();
        let first = register(&registry, 0);
        let mut other_seed = seed();
        other_seed.connection_id = "连接乙".to_owned();
        let other = registry.register(other_seed, vec![], 0).unwrap();
        registry.invalidate_connection("连接甲", 1).unwrap();
        assert_eq!(
            validate(&registry, &first, 1).unwrap_err().code,
            "RESULT_CONTEXT_NOT_FOUND"
        );
        assert!(registry.get_context(&other.context.result_id, 7, 1).is_ok());
        assert_eq!(registry.stats(1).unwrap().contexts, 1);
    }

    #[test]
    fn count_and_row_budgets_evict_oldest_without_touching_rejected_registration() {
        let registry = ResultContextRegistry::new(Limits {
            max_contexts: 2,
            max_row_handles: 6,
            ..Limits::default()
        });
        let first = register(&registry, 0);
        let second = register(&registry, 1);
        let before = registry.stats(2).unwrap();
        assert_eq!(
            registry
                .register(seed(), vec![None; 7], 2)
                .unwrap_err()
                .code,
            "CONTEXT_BUDGET_EXCEEDED"
        );
        assert_eq!(registry.stats(2).unwrap(), before);
        let third = register(&registry, 3);
        assert_eq!(
            validate(&registry, &first, 3).unwrap_err().code,
            "RESULT_CONTEXT_NOT_FOUND"
        );
        assert!(validate(&registry, &second, 3).is_ok());
        assert!(validate(&registry, &third, 3).is_ok());
        assert_eq!(registry.stats(3).unwrap().row_handles, 6);
        let registry = ResultContextRegistry::new(Limits {
            max_contexts: 9,
            max_row_handles: 4,
            ..Limits::default()
        });
        let first = register(&registry, 0);
        register(&registry, 1);
        assert_eq!(
            validate(&registry, &first, 1).unwrap_err().code,
            "RESULT_CONTEXT_NOT_FOUND"
        );
        assert_eq!(registry.stats(1).unwrap().contexts, 1);
    }

    #[test]
    fn byte_budget_accepts_exact_boundary_and_rejects_oversized_without_eviction() {
        let probe = ResultContextRegistry::default();
        register(&probe, 0);
        let bytes = probe.stats(0).unwrap().bytes;
        let registry = ResultContextRegistry::new(Limits {
            max_bytes: bytes,
            ..Limits::default()
        });
        let first = register(&registry, 0);
        let mut giant = identity();
        giant.original_values = vec![Some(RowIdentityValue::Text("巨型".repeat(bytes)))];
        assert_eq!(
            registry
                .register(seed(), vec![Some(giant)], 1)
                .unwrap_err()
                .code,
            "CONTEXT_BUDGET_EXCEEDED"
        );
        assert!(validate(&registry, &first, 1).is_ok());
        let second = register(&registry, 2);
        assert_eq!(
            validate(&registry, &first, 2).unwrap_err().code,
            "RESULT_CONTEXT_NOT_FOUND"
        );
        assert!(validate(&registry, &second, 2).is_ok());
        assert_eq!(registry.stats(2).unwrap().bytes, bytes);
    }

    #[test]
    fn invalid_seeds_and_empty_identity_are_rejected() {
        let registry = ResultContextRegistry::default();
        let mut invalid = seed();
        invalid.source.kind = "sql".to_owned();
        assert_eq!(
            registry.register(invalid, vec![], 0).unwrap_err().code,
            "INVALID_CONTEXT_SEED"
        );
        let mut invalid = seed();
        invalid.connection_generation = "非代次".to_owned();
        assert_eq!(
            registry.register(invalid, vec![], 0).unwrap_err().code,
            "INVALID_CONTEXT_SEED"
        );
        let invalid_identity = StoredRowIdentity {
            keys: vec![],
            original_values: vec![],
        };
        assert_eq!(
            registry
                .register(seed(), vec![Some(invalid_identity)], 0)
                .unwrap_err()
                .code,
            "INVALID_ROW_IDENTITY"
        );
        let registry = ResultContextRegistry::new(Limits {
            ttl_ms: 0,
            ..Limits::default()
        });
        assert_eq!(
            registry.register(seed(), vec![], 0).unwrap_err().code,
            "CONTEXT_BUDGET_EXCEEDED"
        );
        assert_eq!(
            ResultContextRegistry::default()
                .register(seed(), vec![], i64::MAX)
                .unwrap_err()
                .code,
            "INVALID_CONTEXT_TIME"
        );
    }
}
