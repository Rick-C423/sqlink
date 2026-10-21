//! 受控数据集续页与独立统计。游标只保留后端已核验的排序锚点。

use crate::database::pool::ConnectionPoolManager;
use crate::models::ResolvedTable;
use crate::result_context::ContextError;
use crate::table_result::{self, ObservedOrder, PageContinuation, TablePage, TablePageRequest};
use crate::task_registry::{CleanupState, CommitState, TaskPhase};
use crate::typed_value::TypedValue;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use uuid::Uuid;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DatasetPageRequest {
    pub request: TablePageRequest,
    pub task_id: Option<String>,
    pub cursor: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DatasetPage {
    #[serde(flatten)]
    pub page: TablePage,
    pub has_more: bool,
    pub next_cursor: Option<String>,
    pub pagination_mode: String,
    pub observed_order: Vec<ObservedOrder>,
    pub approx_bytes: u64,
    pub truncated_reason: Option<String>,
    pub pagination_fallback_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TableCountRequest {
    pub protocol_version: u32,
    pub result_id: String,
    pub revision: u64,
    pub task_id: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TableCountResult {
    pub result_id: String,
    pub revision: u64,
    pub connection_id: String,
    pub connection_generation: String,
    pub total_rows: String,
}

const CURSOR_TTL_MS: i64 = 5 * 60 * 1000;
const MAX_CURSORS: usize = 512;
const MAX_CURSOR_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Serialize)]
struct CursorEntry {
    connection_id: String,
    generation: String,
    database: String,
    table_parts: Vec<String>,
    fingerprint: String,
    order: Vec<ObservedOrder>,
    page_size: u32,
    next_page: u32,
    sort: Option<(String, table_result::SortDirection)>,
    result_id: String,
    revision: u64,
    anchor: Vec<TypedValue>,
    object_identity: Option<String>,
    expires_at: i64,
}

#[derive(Default)]
struct CursorRegistry {
    entries: HashMap<String, (CursorEntry, usize)>,
    bytes: usize,
}

impl CursorRegistry {
    fn purge(&mut self, now: i64) {
        self.entries.retain(|_, (entry, _)| now < entry.expires_at);
        self.bytes = self.entries.values().map(|(_, bytes)| bytes).sum();
    }
    fn insert(&mut self, entry: CursorEntry) -> Result<String, ContextError> {
        self.purge(now());
        let encoded_bytes = serde_json::to_vec(&entry)
            .map_err(|_| err("CURSOR_UNAVAILABLE", "无法保存续页游标"))?
            .len();
        // 同时覆盖序列化体量、字符串载荷、Vec 元素及注册表节点，避免小整数绕过内存预算。
        let bytes = encoded_bytes
            .saturating_mul(2)
            .saturating_add(std::mem::size_of::<CursorEntry>() + 128)
            .saturating_add(
                entry
                    .anchor
                    .capacity()
                    .saturating_mul(std::mem::size_of::<TypedValue>()),
            )
            .saturating_add(
                entry
                    .order
                    .capacity()
                    .saturating_mul(std::mem::size_of::<ObservedOrder>()),
            )
            .saturating_add(
                entry
                    .table_parts
                    .capacity()
                    .saturating_mul(std::mem::size_of::<String>()),
            );
        if bytes > MAX_CURSOR_BYTES {
            return Err(err("CURSOR_BUDGET_EXCEEDED", "续页锚点超过预算"));
        }
        while self.entries.len() >= MAX_CURSORS || self.bytes + bytes > MAX_CURSOR_BYTES {
            let oldest = self
                .entries
                .iter()
                .min_by_key(|(_, (entry, _))| entry.expires_at)
                .map(|(id, _)| id.clone());
            if let Some(id) = oldest {
                if let Some((_, bytes)) = self.entries.remove(&id) {
                    self.bytes -= bytes;
                }
            } else {
                break;
            }
        }
        let token = Uuid::new_v4().to_string();
        self.entries.insert(token.clone(), (entry, bytes));
        self.bytes += bytes;
        Ok(token)
    }
}

fn registry() -> &'static Mutex<CursorRegistry> {
    static REGISTRY: OnceLock<Mutex<CursorRegistry>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(CursorRegistry::default()))
}
fn now() -> i64 {
    chrono::Utc::now().timestamp_millis()
}
fn err(code: &str, message: &str) -> ContextError {
    ContextError::new(code, message)
}
fn db_error(error: crate::models::DatabaseError) -> ContextError {
    if matches!(error, crate::models::DatabaseError::QueryCancelled) {
        return err("TABLE_TASK_CANCELLED", "统计已取消");
    }
    ContextError::new("DATABASE_ERROR", error.to_string())
}

/// 使用保守的整数/布尔排序白名单，不借续页扩大网格可编辑类型。
fn keyset_reason(
    table: &ResolvedTable,
    driver: &str,
    order: &[ObservedOrder],
) -> Option<&'static str> {
    if table.object_kind.as_deref() != Some("table") {
        return Some("KEYSET_SOURCE_NOT_BASE_TABLE");
    }
    if !table.columns.iter().any(|column| column.is_primary_key) {
        return Some("KEYSET_NO_UNIQUE_PRIMARY_KEY");
    }
    for item in order {
        let Some(column) = table
            .columns
            .iter()
            .find(|column| column.name == item.column)
        else {
            return Some("KEYSET_UNKNOWN_COLUMN");
        };
        if column.nullable {
            return Some("KEYSET_NULLABLE_ORDER");
        }
        let declared = column.data_type.to_ascii_lowercase();
        let base = declared.split(['(', ' ']).next().unwrap_or("");
        let supported = match driver {
            "sqlite" => declared.contains("int"),
            "mysql" | "mariadb" => [
                "tinyint",
                "smallint",
                "mediumint",
                "int",
                "integer",
                "bigint",
            ]
            .contains(&base),
            "postgres" | "postgresql" => [
                "smallint", "integer", "bigint", "int2", "int4", "int8", "boolean",
            ]
            .contains(&base),
            "mssql" | "sqlserver" => {
                ["tinyint", "smallint", "int", "bigint", "bit"].contains(&base)
            }
            _ => false,
        };
        if !supported {
            return Some("KEYSET_ORDER_TYPE_UNSUPPORTED");
        }
    }
    None
}

pub async fn read_page(
    manager: &ConnectionPoolManager,
    request: DatasetPageRequest,
) -> Result<DatasetPage, ContextError> {
    let source = request.request;
    let continuation = if let Some(token) = request.cursor {
        let entry = {
            let mut registry = registry()
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            registry.purge(now());
            registry
                .entries
                .get(&token)
                .map(|(entry, _)| entry.clone())
                .ok_or_else(|| err("CURSOR_EXPIRED", "续页游标不存在或已过期，请重新加载"))?
        };
        let sort = source
            .sort
            .as_ref()
            .map(|sort| (sort.column.clone(), sort.direction));
        if entry.connection_id != source.connection_id
            || entry.database != source.database
            || entry.table_parts != source.table_parts
            || entry.page_size != source.page_size
            || entry.next_page != source.page
            || entry.sort != sort
        {
            return Err(err(
                "CURSOR_SOURCE_MISMATCH",
                "续页来源、页码或排序已变化，请使用普通分页",
            ));
        }
        let context =
            manager
                .result_contexts
                .get_context(&entry.result_id, entry.revision, now())?;
        if context.connection_generation != entry.generation {
            return Err(err("CONNECTION_CHANGED", "续页上下文已换代"));
        }
        Some(PageContinuation {
            generation: entry.generation,
            fingerprint: entry.fingerprint,
            order: entry.order,
            anchor: entry.anchor,
            object_identity: entry.object_identity,
        })
    } else {
        None
    };
    let keyset = continuation.is_some();
    let result =
        table_result::read_dataset_page(manager, source.clone(), request.task_id, continuation)
            .await?;
    let snapshot = manager
        .active_connection_snapshot(&source.connection_id)
        .await?;
    if snapshot.generation != result.page.context.connection_generation {
        return Err(err("CONNECTION_CHANGED", "读取后连接已换代"));
    }
    let mut reason = keyset_reason(
        &result.page.table,
        snapshot.driver.driver_type(),
        &result.order,
    )
    .map(str::to_owned);
    let mut next_cursor = None;
    if result.has_more && reason.is_none() {
        let anchor = result.page.typed_rows.last().and_then(|row| {
            result
                .order
                .iter()
                .map(|item| {
                    let index = result
                        .page
                        .table
                        .columns
                        .iter()
                        .position(|column| column.name == item.column)?;
                    let value = row.get(index)?.as_ref()?;
                    (matches!(
                        value,
                        TypedValue::Integer(_)
                            | TypedValue::UnsignedInteger(_)
                            | TypedValue::Boolean(_)
                    ) && value.validate().is_ok())
                    .then(|| value.clone())
                })
                .collect::<Option<Vec<_>>>()
        });
        if let (Some(anchor), Some(next_page)) = (anchor, source.page.checked_add(1)) {
            let object_identity = manager
                .result_contexts
                .object_identity(
                    &result.page.context.result_id,
                    result.page.context.revision,
                    now(),
                )
                .ok();
            let entry = CursorEntry {
                connection_id: source.connection_id,
                generation: snapshot.generation,
                database: source.database,
                table_parts: source.table_parts,
                fingerprint: table_result::metadata_fingerprint(&result.page.table)?,
                order: result.order.clone(),
                page_size: source.page_size,
                next_page,
                sort: source.sort.map(|sort| (sort.column, sort.direction)),
                result_id: result.page.context.result_id.clone(),
                revision: result.page.context.revision,
                anchor,
                object_identity,
                expires_at: now() + CURSOR_TTL_MS,
            };
            match registry()
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .insert(entry)
            {
                Ok(token) => next_cursor = Some(token),
                Err(_) => reason = Some("KEYSET_CURSOR_BUDGET_EXCEEDED".into()),
            }
        } else {
            reason = Some("KEYSET_TYPED_ANCHOR_UNAVAILABLE".into());
        }
    }
    let approx_bytes = serde_json::to_vec(&result.page)
        .map_err(|_| err("RESULT_ENCODING_FAILED", "无法计算页面预算"))?
        .len() as u64;
    Ok(DatasetPage {
        page: result.page,
        has_more: result.has_more,
        next_cursor,
        pagination_mode: if keyset { "keyset" } else { "offset" }.into(),
        observed_order: result.order,
        approx_bytes,
        truncated_reason: None,
        pagination_fallback_reason: reason,
    })
}

pub async fn count_table(
    manager: &ConnectionPoolManager,
    request: TableCountRequest,
) -> Result<TableCountResult, ContextError> {
    count_table_with_completion_hook(manager, request, |_| async {}).await
}

async fn count_table_with_completion_hook<F, Completion>(
    manager: &ConnectionPoolManager,
    request: TableCountRequest,
    before_finish: F,
) -> Result<TableCountResult, ContextError>
where
    F: FnOnce(String) -> Completion,
    Completion: std::future::Future<Output = ()>,
{
    if request.protocol_version != 1 {
        return Err(err("UNSUPPORTED_PROTOCOL", "结果协议不兼容"));
    }
    if request.revision == 0 || Uuid::parse_str(&request.result_id).is_err() {
        return Err(err("INVALID_REFERENCE", "统计结果引用无效"));
    }
    let context =
        manager
            .result_contexts
            .get_context(&request.result_id, request.revision, now())?;
    let mut task = manager
        .tasks
        .start(request.task_id, &context.connection_id, None, "tableCount")
        .map_err(|value| ContextError::new(value.code, value.message))?;
    let result = async {
        let snapshot = tokio::select! {
            biased;
            _ = task.cancelled() => return Err(err("TABLE_TASK_CANCELLED", "统计已取消")),
            value = manager.active_connection_snapshot(&context.connection_id) => value?,
        };
        if snapshot.generation != context.connection_generation {
            return Err(err("CONNECTION_CHANGED", "统计来源连接已换代"));
        }
        task.prepare(&snapshot.generation, snapshot.driver.session_capabilities())
            .map_err(|value| ContextError::new(value.code, value.message))?;
        let expected = manager.result_contexts.metadata_fingerprint(
            &context.result_id,
            context.revision,
            now(),
        )?;
        let source = ResolvedTable {
            database: context.source.database.clone(),
            schema: context.source.schema.clone(),
            table: context.source.table.clone(),
            columns: vec![],
            object_kind: Some(context.source.kind.clone()),
        };
        let driver = &*snapshot.driver;
        let parts = table_result::source_parts(driver.driver_type(), &source);
        let current_table = tokio::select! {
            biased;
            _ = task.cancelled() => return Err(err("TABLE_TASK_CANCELLED", "统计已取消")),
            value = driver.resolve_table(&source.database, &parts) => value.map_err(db_error)?,
        };
        if table_result::metadata_fingerprint(&current_table)? != expected {
            return Err(err("METADATA_CHANGED", "统计来源表结构已变化"));
        }
        let expected_identity = manager
            .result_contexts
            .object_identity(&context.result_id, context.revision, now())
            .ok();
        let mut lease = tokio::select! {
            biased;
            _ = task.cancelled() => return Err(err("TABLE_TASK_CANCELLED", "统计已取消")),
            value = driver.lease_session(Some(&source.database)) => value.map_err(db_error)?,
        };
        let operation = async {
            task.attach(&*lease)
                .and_then(|_| task.running())
                .map_err(|value| ContextError::new(value.code, value.message))?;
            lease.begin().await.map_err(db_error)?;
            let first = match lease.inspect_target(&current_table, false).await {
                Ok(first) => Some(first),
                Err(error) if expected_identity.is_some() => return Err(db_error(error)),
                Err(_) => {
                    // 视图或只读降级来源没有可编辑身份；统计仍核验上下文的前后目录元数据。
                    lease.rollback().await.map_err(db_error)?;
                    lease.begin().await.map_err(db_error)?;
                    None
                }
            };
            if let Some(first) = &first {
                if table_result::metadata_fingerprint(&first.table)? != expected {
                    return Err(err("METADATA_CHANGED", "统计来源表结构已变化"));
                }
                if expected_identity.as_ref().is_some_and(|identity| {
                    *identity != table_result::object_fingerprint(&first.identity)
                }) {
                    return Err(err("OBJECT_IDENTITY_CHANGED", "统计来源对象身份已变化"));
                }
            }
            let function = if matches!(driver.driver_type(), "mssql" | "sqlserver") {
                "COUNT_BIG(*)"
            } else {
                "COUNT(*)"
            };
            let sql = format!(
                "SELECT {function} AS cnt FROM {}",
                table_result::table_sql(driver, &current_table)
            );
            let count = lease.query_typed(&sql, &[]).await.map_err(db_error)?;
            if count.result.truncated {
                return Err(err("RESULT_BUDGET_EXCEEDED", "统计结果超过预算"));
            }
            let value = count
                .typed_rows
                .first()
                .and_then(|row| row.first())
                .and_then(Option::as_ref);
            let text = match value {
                Some(TypedValue::Integer(value) | TypedValue::UnsignedInteger(value)) => value,
                _ => return Err(err("COUNT_VALUE_UNSUPPORTED", "统计结果无法无损读取")),
            };
            if text
                .parse::<u64>()
                .ok()
                .is_none_or(|value| value.to_string() != *text)
            {
                return Err(err("COUNT_VALUE_UNSUPPORTED", "统计结果不是规范非负整数"));
            }
            if let Some(first) = first {
                let last = lease
                    .inspect_target(&current_table, false)
                    .await
                    .map_err(db_error)?;
                if first.identity != last.identity
                    || table_result::metadata_fingerprint(&last.table)? != expected
                {
                    return Err(err(
                        "OBJECT_IDENTITY_CHANGED",
                        "统计期间表结构或对象身份变化",
                    ));
                }
            }
            Ok(text.clone())
        }
        .await;
        let rollback = lease.rollback().await;
        let cleanup = lease.release().await;
        if rollback.is_err() || cleanup.is_err() {
            return Err(err("SESSION_CLEANUP_FAILED", "统计会话清理失败"));
        }
        if task.cancel_requested() {
            return Err(err("TABLE_TASK_CANCELLED", "统计已取消"));
        }
        let total_rows = operation?;
        let after_table = tokio::select! {
            biased;
            _ = task.cancelled() => return Err(err("TABLE_TASK_CANCELLED", "统计已取消")),
            value = driver.resolve_table(&source.database, &parts) => value.map_err(db_error)?,
        };
        if table_result::metadata_fingerprint(&after_table)? != expected {
            return Err(err("METADATA_CHANGED", "统计期间表结构发生变化"));
        }
        let current = manager
            .active_connection_snapshot(&context.connection_id)
            .await?;
        if current.generation != context.connection_generation {
            return Err(err("CONNECTION_CHANGED", "统计完成时连接已换代"));
        }
        manager
            .result_contexts
            .get_context(&context.result_id, context.revision, now())?;
        Ok(TableCountResult {
            result_id: context.result_id.clone(),
            revision: context.revision,
            connection_id: context.connection_id.clone(),
            connection_generation: context.connection_generation.clone(),
            total_rows,
        })
    }
    .await;
    // 完成边界的回调只用于精确竞争验证，生产入口使用空回调。
    before_finish(task.task_id().to_owned()).await;
    let cleanup_failed = result
        .as_ref()
        .err()
        .is_some_and(|value| value.code == "SESSION_CLEANUP_FAILED");
    let _ = task.finish(
        if cleanup_failed {
            TaskPhase::Unknown
        } else if result.is_ok() {
            TaskPhase::Succeeded
        } else if result.as_ref().err().is_some_and(|value| {
            matches!(
                value.code.as_str(),
                "TABLE_TASK_CANCELLED" | "CANCELLED_BEFORE_EXECUTION"
            )
        }) {
            TaskPhase::Cancelled
        } else {
            TaskPhase::Failed
        },
        CommitState::NotApplicable,
        if cleanup_failed {
            CleanupState::Failed
        } else {
            CleanupState::Succeeded
        },
        if cleanup_failed {
            CleanupState::Failed
        } else {
            CleanupState::Succeeded
        },
        result.as_ref().err().map(|value| value.code.as_str()),
    );
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn completed_count_and_metadata_failure_keep_actual_outcome_after_late_cancel() {
        for succeeds in [true, false] {
            let manager = ConnectionPoolManager::new(None);
            let config = serde_json::from_value(serde_json::json!({"id":"统计完成竞争","name":"统计完成竞争","driverType":"sqlite","host":"","port":0,"user":"","password":"","database":":memory:"})).unwrap();
            manager.create_connection(config).await.unwrap();
            let driver = manager.get_connection("统计完成竞争").await.unwrap();
            driver
                .execute(
                    "CREATE TABLE items(id INTEGER NOT NULL PRIMARY KEY)",
                    None,
                    None,
                )
                .await
                .unwrap();
            driver
                .execute("INSERT INTO items VALUES(1)", None, None)
                .await
                .unwrap();
            let page_request = serde_json::from_value(serde_json::json!({"protocolVersion":1,"connectionId":"统计完成竞争","database":"main","tableParts":["items"],"page":1,"pageSize":1,"sort":null})).unwrap();
            let page = table_result::read_table_page(&manager, page_request)
                .await
                .unwrap();
            if !succeeds {
                driver
                    .execute("ALTER TABLE items ADD COLUMN added INTEGER", None, None)
                    .await
                    .unwrap();
            }
            let task_id = Uuid::new_v4().to_string();
            let request = TableCountRequest {
                protocol_version: 1,
                result_id: page.context.result_id.clone(),
                revision: page.context.revision,
                task_id: Some(task_id.clone()),
            };
            let manager_ref = &manager;
            let outcome =
                count_table_with_completion_hook(&manager, request, move |task_id| async move {
                    // SQL 结果或元数据错误已经确定；在终态写入前请求取消，不依赖 sleep 调度。
                    let _ = manager_ref.tasks.request_cancel(&task_id).await;
                    assert!(
                        manager_ref
                            .tasks
                            .snapshot(&task_id)
                            .unwrap()
                            .cancel_requested
                    );
                })
                .await;
            let state = manager.tasks.snapshot(&task_id).unwrap();
            assert!(state.cancel_requested);
            assert_eq!(
                state.phase,
                if succeeds {
                    TaskPhase::Succeeded
                } else {
                    TaskPhase::Failed
                }
            );
            assert_eq!(state.cleanup_state, CleanupState::Succeeded);
            if succeeds {
                assert_eq!(outcome.unwrap().total_rows, "1");
            } else {
                assert_eq!(outcome.unwrap_err().code, "METADATA_CHANGED");
            }
            assert_eq!(manager.tasks.active_count(), 0);
        }
    }

    #[test]
    fn interrupted_count_maps_to_explicit_cancellation_outcome() {
        assert_eq!(
            db_error(crate::models::DatabaseError::QueryCancelled).code,
            "TABLE_TASK_CANCELLED"
        );
    }

    fn entry() -> CursorEntry {
        CursorEntry {
            connection_id: "测试连接".into(),
            generation: "代次".into(),
            database: "测试库".into(),
            table_parts: vec!["测试表".into()],
            fingerprint: "元数据摘要".into(),
            order: vec![],
            page_size: 10,
            next_page: 2,
            sort: None,
            result_id: Uuid::new_v4().to_string(),
            revision: 1,
            anchor: vec![TypedValue::Integer("1".into())],
            object_identity: Some("对象摘要".into()),
            expires_at: now() + CURSOR_TTL_MS,
        }
    }

    #[test]
    fn cursor_registry_enforces_count_ttl_and_byte_bounds() {
        let mut registry = CursorRegistry::default();
        let mut oldest = entry();
        oldest.expires_at -= 10_000;
        let first = registry.insert(oldest).unwrap();
        for _ in 0..MAX_CURSORS {
            registry.insert(entry()).unwrap();
        }
        assert_eq!(registry.entries.len(), MAX_CURSORS);
        assert!(!registry.entries.contains_key(&first));
        assert!(registry.bytes <= MAX_CURSOR_BYTES);
        registry.purge(now() + CURSOR_TTL_MS + 1);
        assert!(registry.entries.is_empty());
        assert_eq!(registry.bytes, 0);
        let mut huge = entry();
        huge.database = "x".repeat(MAX_CURSOR_BYTES);
        assert_eq!(
            registry.insert(huge).unwrap_err().code,
            "CURSOR_BUDGET_EXCEEDED"
        );
        assert_eq!(registry.bytes, 0);
    }
}
