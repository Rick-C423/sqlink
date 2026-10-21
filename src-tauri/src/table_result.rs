//! 受控表读取与可信上下文入口。原始 SQL 不能进入此处的来源/行身份登记。

use crate::database::pool::ConnectionPoolManager;
use crate::database::DatabaseDriver;
use crate::models::{ColumnInfo, QueryResult, ResolvedTable};
use crate::result_context::*;
use crate::task_registry::{CleanupState, CommitState, TaskGuard, TaskPhase};
use crate::typed_value::TypedValue;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use uuid::Uuid;

pub const PROTOCOL_VERSION: u32 = 1;
const MAX_PAGE_SIZE: u32 = 1000;
const JS_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TablePageRequest {
    pub protocol_version: u32,
    pub connection_id: String,
    pub database: String,
    pub table_parts: Vec<String>,
    pub page: u32,
    pub page_size: u32,
    pub sort: Option<TableSort>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TableSort {
    pub column: String,
    pub direction: SortDirection,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SortDirection {
    Asc,
    Desc,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TablePage {
    pub result: QueryResult,
    pub context: ResultContext,
    pub row_handles: Vec<Option<String>>,
    pub table: ResolvedTable,
    pub total_rows: Option<u64>,
    pub typed_rows: Vec<Vec<Option<TypedValue>>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContextReference {
    pub protocol_version: u32,
    pub result_id: String,
    pub revision: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContextValidationRequest {
    pub protocol_version: u32,
    pub result_id: String,
    pub revision: u64,
    pub row_handles: Vec<String>,
}

fn error(code: &str, message: &str) -> ContextError {
    ContextError::new(code, message)
}
fn database_error(value: crate::models::DatabaseError) -> ContextError {
    if matches!(value, crate::models::DatabaseError::QueryCancelled) {
        return error("TABLE_TASK_CANCELLED", "表读取已取消");
    }
    ContextError::new("DATABASE_ERROR", value.to_string())
}

fn check_version(version: u32) -> Result<(), ContextError> {
    if version != PROTOCOL_VERSION {
        return Err(error(
            "UNSUPPORTED_PROTOCOL",
            "结果协议不兼容，请更新应用后重新读取",
        ));
    }
    Ok(())
}

fn check_reference(version: u32, id: &str, revision: u64) -> Result<(), ContextError> {
    check_version(version)?;
    if revision == 0 || Uuid::parse_str(id).is_err() {
        return Err(error("INVALID_REFERENCE", "结果引用无效，请重新加载"));
    }
    Ok(())
}

pub(crate) fn quote(driver: &dyn DatabaseDriver, name: &str) -> String {
    let escaped = driver.escape_identifier(name);
    match driver.driver_type() {
        "mysql" | "mariadb" => format!("`{escaped}`"),
        "mssql" | "sqlserver" => format!("[{escaped}]"),
        _ => format!("\"{escaped}\""),
    }
}

pub(crate) fn source_parts(driver_type: &str, table: &ResolvedTable) -> Vec<String> {
    match driver_type {
        "mysql" | "mariadb" => vec![table.database.clone(), table.table.clone()],
        "mssql" | "sqlserver" => vec![
            table.database.clone(),
            table.schema.clone().unwrap_or_default(),
            table.table.clone(),
        ],
        _ => table
            .schema
            .iter()
            .cloned()
            .chain(std::iter::once(table.table.clone()))
            .collect(),
    }
}

pub(crate) fn table_sql(driver: &dyn DatabaseDriver, table: &ResolvedTable) -> String {
    source_parts(driver.driver_type(), table)
        .iter()
        .map(|part| quote(driver, part))
        .collect::<Vec<_>>()
        .join(".")
}

pub fn metadata_fingerprint(table: &ResolvedTable) -> Result<String, ContextError> {
    let encoded = serde_json::to_vec(table)
        .map_err(|_| error("METADATA_UNAVAILABLE", "无法核验结果元数据"))?;
    Ok(format!("{:x}", Sha256::digest(encoded)))
}

pub(crate) fn object_fingerprint(identity: &str) -> String {
    format!("{:x}", Sha256::digest(identity.as_bytes()))
}

fn check_typed_read_support(driver: &str, table: &ResolvedTable) -> Result<(), ContextError> {
    // tiberius 0.12 在收到 scale=38 的 Numeric 时会在库内断言，应用层尚来不及降级。
    // 受控路径已有目录信息，必须在发出 SELECT 前拒绝这类页面。
    if matches!(driver, "mssql" | "sqlserver")
        && table.columns.iter().any(|column| {
            let declared = column.data_type.to_ascii_lowercase();
            let base = declared.split('(').next().unwrap_or("");
            ["numeric", "decimal"].contains(&base)
                && declared
                    .split_once('(')
                    .and_then(|(_, rest)| rest.split_once(')'))
                    .and_then(|(args, _)| args.split(',').nth(1))
                    .and_then(|scale| scale.trim().parse::<u32>().ok())
                    .is_some_and(|scale| scale >= 38)
        })
    {
        return Err(error(
            "UNSUPPORTED_VALUE_TYPE",
            "当前 SQL Server 驱动无法安全读取 scale 为 38 的精确小数列",
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ObservedOrder {
    pub column: String,
    pub direction: SortDirection,
}

pub(crate) struct PageContinuation {
    pub generation: String,
    pub fingerprint: String,
    pub order: Vec<ObservedOrder>,
    pub anchor: Vec<TypedValue>,
    pub object_identity: Option<String>,
}

pub(crate) fn observed_order(
    table: &ResolvedTable,
    request: &TablePageRequest,
) -> Result<Vec<ObservedOrder>, ContextError> {
    let mut order = Vec::new();
    if let Some(sort) = &request.sort {
        let exact: Vec<_> = table
            .columns
            .iter()
            .filter(|column| column.name == sort.column)
            .collect();
        let candidates = if exact.is_empty() {
            table
                .columns
                .iter()
                .filter(|column| column.name.eq_ignore_ascii_case(&sort.column))
                .collect()
        } else {
            exact
        };
        if candidates.len() != 1 {
            return Err(error("INVALID_SORT", "排序字段不存在或存在歧义"));
        }
        order.push(ObservedOrder {
            column: candidates[0].name.clone(),
            direction: sort.direction,
        });
    }
    for column in table.columns.iter().filter(|column| column.is_primary_key) {
        if !order.iter().any(|item| item.column == column.name) {
            order.push(ObservedOrder {
                column: column.name.clone(),
                direction: SortDirection::Asc,
            });
        }
    }
    Ok(order)
}

/// 排序项引用元数据字段，绝不接收表达式；补齐主键时按列身份去重。
fn page_sql_with_limit(
    driver: &dyn DatabaseDriver,
    table: &ResolvedTable,
    request: &TablePageRequest,
    limit: u32,
    continuation: Option<&PageContinuation>,
) -> Result<String, ContextError> {
    if request.page == 0 || request.page_size == 0 || request.page_size > MAX_PAGE_SIZE {
        return Err(error(
            "INVALID_PAGE",
            "页码必须大于零，每页行数须在 1 至 1000 之间",
        ));
    }
    let mut names = HashSet::new();
    if table.columns.is_empty()
        || table
            .columns
            .iter()
            .any(|column| !names.insert(column.name.as_str()))
    {
        return Err(error("AMBIGUOUS_COLUMNS", "表字段为空或存在重复映射"));
    }
    let mut order = Vec::new();
    let mut seen = HashSet::new();
    if let Some(sort) = &request.sort {
        let exact: Vec<usize> = table
            .columns
            .iter()
            .enumerate()
            .filter(|(_, column)| column.name == sort.column)
            .map(|(index, _)| index)
            .collect();
        let candidates = if exact.is_empty() {
            table
                .columns
                .iter()
                .enumerate()
                .filter(|(_, column)| column.name.eq_ignore_ascii_case(&sort.column))
                .map(|(index, _)| index)
                .collect()
        } else {
            exact
        };
        if candidates.len() != 1 {
            return Err(error("INVALID_SORT", "排序字段不存在或存在歧义"));
        }
        let index = candidates[0];
        let direction = match sort.direction {
            SortDirection::Asc => "ASC",
            SortDirection::Desc => "DESC",
        };
        order.push(format!(
            "{} {direction}",
            quote(driver, &table.columns[index].name)
        ));
        seen.insert(index);
    }
    for (index, column) in table.columns.iter().enumerate() {
        if column.is_primary_key && seen.insert(index) {
            order.push(format!("{} ASC", quote(driver, &column.name)));
        }
    }
    let columns = table
        .columns
        .iter()
        .map(|column| quote(driver, &column.name))
        .collect::<Vec<_>>();
    let target = table_sql(driver, table);
    let offset = if continuation.is_some() {
        0
    } else {
        u64::from(request.page - 1) * u64::from(request.page_size)
    };
    let predicate = if let Some(cursor) = continuation {
        let expected = observed_order(table, request)?;
        if cursor.order != expected || cursor.anchor.len() != expected.len() || expected.is_empty()
        {
            return Err(error("CURSOR_SOURCE_MISMATCH", "续页排序与原游标不一致"));
        }
        // 字典序谓词，每层前缀保持相等，当前列按自己的升降序推进。
        let mut binding = 0;
        let branches = expected
            .iter()
            .enumerate()
            .map(|(index, item)| {
                let mut terms = expected
                    .iter()
                    .take(index)
                    .map(|item| {
                        binding += 1;
                        format!(
                            "{} = {}",
                            quote(driver, &item.column),
                            crate::changeset::placeholder(driver.driver_type(), binding)
                        )
                    })
                    .collect::<Vec<_>>();
                let comparison = if item.direction == SortDirection::Asc {
                    ">"
                } else {
                    "<"
                };
                binding += 1;
                terms.push(format!(
                    "{} {comparison} {}",
                    quote(driver, &item.column),
                    crate::changeset::placeholder(driver.driver_type(), binding)
                ));
                format!("({})", terms.join(" AND "))
            })
            .collect::<Vec<_>>();
        format!(" WHERE {}", branches.join(" OR "))
    } else {
        String::new()
    };
    if matches!(driver.driver_type(), "mssql" | "sqlserver") {
        let mut synthetic = "__sqlink_row_number".to_string();
        while table
            .columns
            .iter()
            .any(|column| column.name.eq_ignore_ascii_case(&synthetic))
        {
            synthetic.push('_');
        }
        let row_number = quote(driver, &synthetic);
        let alias = quote(driver, "__sqlink_page");
        let projection = columns
            .iter()
            .map(|column| format!("{alias}.{column}"))
            .collect::<Vec<_>>()
            .join(", ");
        let order = if order.is_empty() {
            "(SELECT 1)".into()
        } else {
            order.join(", ")
        };
        return Ok(format!("SELECT {projection} FROM (SELECT {}, ROW_NUMBER() OVER (ORDER BY {order}) AS {row_number} FROM {target}{predicate}) AS {alias} WHERE {alias}.{row_number} BETWEEN {} AND {} ORDER BY {alias}.{row_number}", columns.join(", "), offset + 1, offset + u64::from(limit)));
    }
    let order = if order.is_empty() {
        String::new()
    } else {
        format!(" ORDER BY {}", order.join(", "))
    };
    Ok(format!(
        "SELECT {} FROM {target}{predicate}{order} LIMIT {} OFFSET {offset}",
        columns.join(", "),
        limit
    ))
}

#[cfg(test)]
fn integer_key(value: &Value, allow_text: bool, unsigned: bool) -> Option<RowIdentityValue> {
    let text = match value {
        Value::Number(number) if number.as_i64().is_some() || number.as_u64().is_some() => {
            number.to_string()
        }
        Value::String(text) if allow_text => text.clone(),
        _ => return None,
    };
    if unsigned {
        let number = text.parse::<u64>().ok()?;
        (number.to_string() == text).then(|| RowIdentityValue::UnsignedInteger(text))
    } else {
        let number = text.parse::<i64>().ok()?;
        (number.to_string() == text).then(|| RowIdentityValue::Integer(text))
    }
}

/// 白名单只覆盖已核对驱动无损返回的身份类型；文本、decimal、时间及未知转换暂不放行。
#[cfg(test)]
fn key_value(driver: &str, column: &ColumnInfo, value: &Value) -> Option<RowIdentityValue> {
    let declared = column.data_type.to_ascii_lowercase();
    let base = declared.split(['(', ' ']).next().unwrap_or("");
    match driver {
        "mysql" | "mariadb"
            if [
                "tinyint",
                "smallint",
                "mediumint",
                "int",
                "integer",
                "bigint",
            ]
            .contains(&base) =>
        {
            integer_key(value, true, declared.contains("unsigned"))
        }
        "postgres" | "postgresql"
            if ["smallint", "integer", "bigint", "int2", "int4", "int8"].contains(&base) =>
        {
            integer_key(value, false, false)
        }
        "mssql" | "sqlserver" if ["tinyint", "smallint", "int", "bigint"].contains(&base) => {
            integer_key(value, false, false)
        }
        "postgres" | "postgresql" if declared == "boolean" => {
            value.as_bool().map(RowIdentityValue::Boolean)
        }
        "mssql" | "sqlserver" if declared == "bit" => {
            value.as_bool().map(RowIdentityValue::Boolean)
        }
        "sqlite" if declared.contains("int") => integer_key(value, false, false),
        _ => None,
    }
}

fn capture_identities(
    driver: &str,
    table: &ResolvedTable,
    typed_rows: &[Vec<Option<TypedValue>>],
) -> (Vec<Option<StoredRowIdentity>>, Vec<String>) {
    let mut reasons = vec!["PARAMETERIZED_COMMIT_REQUIRED".to_string()];
    if table.object_kind.as_deref() != Some("table") {
        reasons.push("SOURCE_NOT_BASE_TABLE".into());
        return (vec![None; typed_rows.len()], reasons);
    }
    let keys: Vec<(usize, &ColumnInfo)> = table
        .columns
        .iter()
        .enumerate()
        .filter(|(_, column)| column.is_primary_key)
        .collect();
    if keys.is_empty() {
        reasons.push("NO_PRIMARY_KEY".into());
        return (vec![None; typed_rows.len()], reasons);
    }
    let mut seen = HashSet::new();
    let mut duplicate = false;
    let identities = typed_rows
        .iter()
        .map(|row| {
            let values = keys
                .iter()
                .map(|(index, column)| {
                    let value = row.get(*index)?.as_ref()?;
                    // P1 02 改为真实驱动值，但暂不扩宽可定位主键类型白名单。
                    let declared = column.data_type.to_ascii_lowercase();
                    let base = declared.split(['(', ' ']).next().unwrap_or("");
                    let supported = match value {
                        TypedValue::Integer(_) | TypedValue::UnsignedInteger(_) => match driver {
                            "mysql" | "mariadb" => [
                                "tinyint",
                                "smallint",
                                "mediumint",
                                "int",
                                "integer",
                                "bigint",
                            ]
                            .contains(&base),
                            "postgres" | "postgresql" => {
                                ["smallint", "integer", "bigint", "int2", "int4", "int8"]
                                    .contains(&base)
                            }
                            "mssql" | "sqlserver" => {
                                ["tinyint", "smallint", "int", "bigint"].contains(&base)
                            }
                            "sqlite" => declared.contains("int"),
                            _ => false,
                        },
                        TypedValue::Boolean(_) => {
                            (matches!(driver, "postgres" | "postgresql") && declared == "boolean")
                                || (matches!(driver, "mssql" | "sqlserver") && declared == "bit")
                        }
                        _ => false,
                    };
                    (supported && value.validate().is_ok())
                        .then(|| (column.name.clone(), value.clone()))
                })
                .collect::<Option<Vec<_>>>()?;
            // 此序列仅在后端做重复检测，不是公开句柄。
            let encoded = serde_json::to_string(&values).ok()?;
            if !seen.insert(encoded) {
                duplicate = true;
            }
            Some(StoredRowIdentity {
                keys: values,
                original_values: row.clone(),
            })
        })
        .collect::<Vec<_>>();
    if duplicate {
        reasons.push("DUPLICATE_ROW_IDENTITY".into());
        return (vec![None; typed_rows.len()], reasons);
    }
    if identities.iter().any(Option::is_none) {
        reasons.push("ROW_IDENTITY_UNAVAILABLE".into());
    }
    (identities, reasons)
}

/// 受控读取的大整数用十进制文本显示，真实定位值已在此转换前保存在后端。
fn protect_display_integers(result: &mut QueryResult) {
    for row in &mut result.rows {
        for value in row.values.iter_mut().flatten() {
            if let Value::Number(number) = value {
                if number
                    .as_i64()
                    .is_some_and(|value| value.unsigned_abs() > JS_SAFE_INTEGER)
                    || number.as_u64().is_some_and(|value| value > JS_SAFE_INTEGER)
                {
                    *value = Value::String(number.to_string());
                }
            }
        }
    }
}

/// 页面 SQL、目录身份及取消句柄全部固定到同一租约。
async fn verified_read(
    lease: &mut dyn crate::session::SessionLease,
    table: &ResolvedTable,
    sql: &str,
    params: &[TypedValue],
    expected_identity: Option<&str>,
) -> Result<
    (
        crate::typed_value::TypedQueryResult,
        Option<String>,
        Vec<String>,
    ),
    ContextError,
> {
    lease.begin().await.map_err(database_error)?;
    let first = match lease.inspect_target(table, false).await {
        Ok(first) => Some(first),
        Err(_) => {
            // 沿用不支持对象身份时的只读降级，但仍使用原租约且重新开始事务。
            lease.rollback().await.map_err(database_error)?;
            lease.begin().await.map_err(database_error)?;
            None
        }
    };
    if let Some(first) = &first {
        if metadata_fingerprint(&first.table)? != metadata_fingerprint(table)? {
            return Err(error("METADATA_CHANGED", "读取前表结构已变化，请重试"));
        }
        if expected_identity.is_some_and(|expected| expected != object_fingerprint(&first.identity))
        {
            return Err(error("OBJECT_IDENTITY_CHANGED", "续页来源对象身份已变化"));
        }
    } else if expected_identity.is_some() {
        return Err(error(
            "OBJECT_IDENTITY_UNAVAILABLE",
            "无法核验续页来源对象身份",
        ));
    }
    let read = lease
        .query_typed(sql, params)
        .await
        .map_err(database_error)?;
    if read.result.truncated {
        return Err(error(
            "RESULT_BUDGET_EXCEEDED",
            "页面超过查询结果预算，请减少每页行数；未发布编辑身份",
        ));
    }
    if let Some(first) = first {
        let last = lease
            .inspect_target(table, false)
            .await
            .map_err(database_error)?;
        if first.identity != last.identity
            || metadata_fingerprint(&first.table)? != metadata_fingerprint(&last.table)?
        {
            return Err(error("OBJECT_IDENTITY_CHANGED", "读取期间对象身份发生变化"));
        }
        Ok((
            read,
            Some(object_fingerprint(&first.identity)),
            first.write_reasons,
        ))
    } else {
        Ok((read, None, vec!["OBJECT_IDENTITY_UNAVAILABLE".into()]))
    }
}

pub(crate) struct ReadPage {
    pub page: TablePage,
    pub has_more: bool,
    pub order: Vec<ObservedOrder>,
}

pub async fn read_table_page(
    manager: &ConnectionPoolManager,
    request: TablePageRequest,
) -> Result<TablePage, ContextError> {
    read_dataset_page(manager, request, None, None)
        .await
        .map(|value| value.page)
}

pub(crate) async fn read_dataset_page(
    manager: &ConnectionPoolManager,
    request: TablePageRequest,
    task_id: Option<String>,
    continuation: Option<PageContinuation>,
) -> Result<ReadPage, ContextError> {
    read_dataset_page_with_completion_hook(manager, request, task_id, continuation, |_| async {})
        .await
}

async fn read_dataset_page_with_completion_hook<F, Completion>(
    manager: &ConnectionPoolManager,
    request: TablePageRequest,
    task_id: Option<String>,
    continuation: Option<PageContinuation>,
    before_finish: F,
) -> Result<ReadPage, ContextError>
where
    F: FnOnce(String) -> Completion,
    Completion: std::future::Future<Output = ()>,
{
    let mut task = manager
        .tasks
        .start(task_id, &request.connection_id, None, "tableRead")
        .map_err(|error| ContextError::new(error.code, error.message))?;
    let outcome = read_dataset_page_inner(manager, request, continuation, &mut task).await;
    // 测试可在业务结果已确定、终态尚未落盘的精确边界注入取消；生产调用为空回调。
    before_finish(task.task_id().to_owned()).await;
    let cancelled = outcome.as_ref().err().is_some_and(|value| {
        matches!(
            value.code.as_str(),
            "TABLE_TASK_CANCELLED" | "CANCELLED_BEFORE_EXECUTION"
        )
    });
    let cleanup_failed = outcome
        .as_ref()
        .err()
        .is_some_and(|value| value.code == "SESSION_CLEANUP_FAILED");
    let _ = task.finish(
        if cleanup_failed {
            TaskPhase::Unknown
        } else if outcome.is_ok() {
            TaskPhase::Succeeded
        } else if cancelled {
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
        outcome.as_ref().err().map(|error| error.code.as_str()),
    );
    outcome
}

async fn read_dataset_page_inner(
    manager: &ConnectionPoolManager,
    request: TablePageRequest,
    continuation: Option<PageContinuation>,
    task: &mut TaskGuard,
) -> Result<ReadPage, ContextError> {
    check_version(request.protocol_version)?;
    if request.connection_id.is_empty()
        || request.database.is_empty()
        || request.table_parts.is_empty()
        || request.table_parts.len() > 3
        || request
            .table_parts
            .iter()
            .any(|part| part.is_empty() || part.contains('\0'))
    {
        return Err(error("INVALID_SOURCE", "连接、数据库及表名各段必须有效"));
    }
    if request.page == 0 || request.page_size == 0 || request.page_size > MAX_PAGE_SIZE {
        return Err(error(
            "INVALID_PAGE",
            "页码必须大于零，每页行数须在 1 至 1000 之间",
        ));
    }
    let snapshot = tokio::select! {
        biased;
        _ = task.cancelled() => return Err(error("TABLE_TASK_CANCELLED", "表读取已取消")),
        value = manager.get_connection_snapshot(&request.connection_id) => value.map_err(database_error)?,
    };
    task.prepare(&snapshot.generation, snapshot.driver.session_capabilities())
        .map_err(|value| ContextError::new(value.code, value.message))?;
    let driver = &*snapshot.driver;
    if continuation
        .as_ref()
        .is_some_and(|cursor| cursor.generation != snapshot.generation)
    {
        return Err(error(
            "CONNECTION_CHANGED",
            "游标所属连接已换代，请重新加载",
        ));
    }
    let (database, parts) = if driver.driver_type() == "sqlite" {
        let file = snapshot
            .database
            .as_deref()
            .ok_or_else(|| error("INVALID_SOURCE", "SQLite 文件来源不可用"))?;
        let parts = if request.table_parts.len() == 1 && request.database != file {
            vec![request.database.clone(), request.table_parts[0].clone()]
        } else {
            request.table_parts.clone()
        };
        (file, parts)
    } else {
        (request.database.as_str(), request.table_parts.clone())
    };
    let table = tokio::select! {
        biased;
        _ = task.cancelled() => return Err(error("TABLE_TASK_CANCELLED", "表读取已取消")),
        value = driver.resolve_table(database, &parts) => value.map_err(database_error)?,
    };
    let fingerprint = metadata_fingerprint(&table)?;
    if continuation
        .as_ref()
        .is_some_and(|cursor| cursor.fingerprint != fingerprint)
    {
        return Err(error(
            "METADATA_CHANGED",
            "游标所属表结构已变化，请重新加载",
        ));
    }
    check_typed_read_support(driver.driver_type(), &table)?;
    let order = observed_order(&table, &request)?;
    let sql = page_sql_with_limit(
        driver,
        &table,
        &request,
        request.page_size + 1,
        continuation.as_ref(),
    )?;
    let params = continuation
        .as_ref()
        .map(|cursor| {
            (0..cursor.anchor.len())
                .flat_map(|index| cursor.anchor.iter().take(index + 1).cloned())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let mut lease = tokio::select! {
        biased;
        _ = task.cancelled() => return Err(error("TABLE_TASK_CANCELLED", "表读取已取消")),
        value = driver.lease_session(Some(&table.database)) => value.map_err(database_error)?,
    };
    let attached = task.attach(&*lease).and_then(|_| task.running());
    let read = if let Err(value) = attached {
        Err(ContextError::new(value.code, value.message))
    } else {
        // 执行期间取消由固定租约句柄中断，必须等待回滚与归还完成再发布终态。
        verified_read(
            &mut *lease,
            &table,
            &sql,
            &params,
            continuation
                .as_ref()
                .and_then(|cursor| cursor.object_identity.as_deref()),
        )
        .await
    };
    let rollback = lease.rollback().await;
    let cleanup = lease.release().await;
    if rollback.is_err() || cleanup.is_err() {
        return Err(error(
            "SESSION_CLEANUP_FAILED",
            "表读取会话清理失败，结果未发布",
        ));
    }
    if task.cancel_requested() {
        return Err(error("TABLE_TASK_CANCELLED", "表读取已取消"));
    }
    let (mut read, object_identity, identity_reasons) = read?;
    // 探测行不能进入上下文、身份句柄或公开 typedRows。
    let has_more = read.result.rows.len() > request.page_size as usize;
    read.result.rows.truncate(request.page_size as usize);
    read.typed_rows.truncate(request.page_size as usize);
    read.result.total_count = Some(read.result.rows.len());
    let mut result = read.result;
    if result.truncated {
        return Err(error(
            "RESULT_BUDGET_EXCEEDED",
            "页面超过查询结果预算，请减少每页行数；未发布编辑身份",
        ));
    }
    let typed_rows = read.typed_rows;
    if result.columns.len() != table.columns.len()
        || typed_rows.len() != result.rows.len()
        || typed_rows
            .iter()
            .any(|row| row.len() != table.columns.len())
        || result
            .columns
            .iter()
            .zip(&table.columns)
            .any(|(actual, expected)| actual.name != expected.name)
        || result
            .rows
            .iter()
            .any(|row| row.values.len() != table.columns.len())
    {
        return Err(error(
            "COLUMN_MAPPING_CHANGED",
            "读取字段映射不一致，请重新加载",
        ));
    }
    let current_parts = source_parts(driver.driver_type(), &table);
    let current = tokio::select! {
        biased;
        _ = task.cancelled() => return Err(error("TABLE_TASK_CANCELLED", "表读取已取消")),
        value = driver.resolve_table(&table.database, &current_parts) => value.map_err(database_error)?,
    };
    if metadata_fingerprint(&current)? != fingerprint {
        return Err(error(
            "METADATA_CHANGED",
            "读取期间表结构发生变化，请重新加载",
        ));
    }
    let total_rows = None;
    let (rows, mut reason_codes) = capture_identities(driver.driver_type(), &table, &typed_rows);
    reason_codes.extend(identity_reasons);
    if typed_rows.iter().flatten().any(Option::is_none) {
        reason_codes.push("UNSUPPORTED_VALUE_TYPE".into());
    }
    if !table.columns.iter().any(|column| column.is_primary_key) {
        reason_codes.push("PAGINATION_ORDER_NOT_UNIQUE".into());
    }
    let columns: Vec<ResultColumn> = table
        .columns
        .iter()
        .map(|column| ResultColumn {
            column_id: Uuid::new_v4().to_string(),
            display_name: column.name.clone(),
            database_type: column.data_type.clone(),
            nullable: column.nullable,
            origin: (table.object_kind.as_deref() != Some("unknown")).then(|| ColumnOrigin {
                database: table.database.clone(),
                schema: table.schema.clone(),
                table: table.table.clone(),
                column: column.name.clone(),
            }),
            writable: false,
            generated: column.extra.as_deref().is_some_and(|extra| {
                let value = extra.to_ascii_lowercase();
                value.contains("generated")
                    || value.contains("identity")
                    || value.contains("auto_increment")
            }),
        })
        .collect();
    let credential_ready = manager.credential_health().state == "ready";
    if !credential_ready {
        reason_codes.push("CREDENTIAL_RECOVERY_REQUIRED".into());
    }
    if table
        .columns
        .iter()
        .any(|column| crate::changeset::grid_input_kind(driver.driver_type(), column).is_none())
    {
        reason_codes.push("GRID_VALUE_TYPE_NOT_VALIDATED".into());
    }
    let policy = (object_identity.is_some()
        && credential_ready
        && reason_codes
            .iter()
            .all(|code| code == "PARAMETERIZED_COMMIT_REQUIRED"))
    .then(|| EditPolicy {
        allowed_operations: vec!["insert".into(), "update".into(), "delete".into()],
        reason_codes: vec![],
        column_input_kinds: columns
            .iter()
            .zip(&table.columns)
            .filter(|(column, _)| !column.generated)
            .filter_map(|(column, metadata)| {
                crate::changeset::grid_input_kind(driver.driver_type(), metadata)
                    .map(|kind| (column.column_id.clone(), kind.into()))
            })
            .collect(),
    });
    if task.cancel_requested() {
        return Err(error("TABLE_TASK_CANCELLED", "表读取已取消"));
    }
    let published = manager
        .register_verified_result_context(
            &snapshot,
            ResultContextSeed {
                revision: 1,
                connection_id: request.connection_id,
                connection_generation: snapshot.generation.clone(),
                source: ResultSource {
                    database: table.database.clone(),
                    schema: table.schema.clone(),
                    table: table.table.clone(),
                    kind: table
                        .object_kind
                        .clone()
                        .unwrap_or_else(|| "unknown".into()),
                },
                columns,
                metadata_fingerprint: fingerprint,
                reason_codes,
            },
            rows,
            object_identity,
            policy,
        )
        .await?;
    protect_display_integers(&mut result);
    Ok(ReadPage {
        page: TablePage {
            result,
            context: published.context,
            row_handles: published.row_handles,
            table,
            total_rows,
            typed_rows,
        },
        has_more,
        order,
    })
}

pub async fn validate_context(
    manager: &ConnectionPoolManager,
    request: ContextValidationRequest,
) -> Result<ResultContext, ContextError> {
    check_reference(
        request.protocol_version,
        &request.result_id,
        request.revision,
    )?;
    if request.row_handles.len() > MAX_PAGE_SIZE as usize
        || request
            .row_handles
            .iter()
            .any(|handle| Uuid::parse_str(handle).is_err())
    {
        return Err(error("INVALID_ROW_HANDLE", "行引用无效或超过当前页上限"));
    }
    let now = chrono::Utc::now().timestamp_millis();
    let context = manager
        .result_contexts
        .get_context(&request.result_id, request.revision, now)?;
    let snapshot = manager
        .active_connection_snapshot(&context.connection_id)
        .await?;
    if snapshot.generation != context.connection_generation {
        return Err(error("CONNECTION_CHANGED", "连接已换代，请重新加载结果"));
    }
    let table = ResolvedTable {
        database: context.source.database.clone(),
        schema: context.source.schema.clone(),
        table: context.source.table.clone(),
        columns: vec![],
        object_kind: Some(context.source.kind.clone()),
    };
    let current = snapshot
        .driver
        .resolve_table(
            &table.database,
            &source_parts(snapshot.driver.driver_type(), &table),
        )
        .await
        .map_err(database_error)?;
    let expected =
        manager
            .result_contexts
            .metadata_fingerprint(&request.result_id, request.revision, now)?;
    if metadata_fingerprint(&current)? != expected {
        manager
            .result_contexts
            .release(&request.result_id, request.revision, now)?;
        return Err(error("METADATA_CHANGED", "结果的表结构已变化，请重新加载"));
    }
    // 校验后端保留的引用，不把真实键、原始值或可变来源回传前端。
    manager
        .validate_result_handles(&context, &request.row_handles)
        .await?;
    Ok(context)
}

pub fn release_context(
    manager: &ConnectionPoolManager,
    request: ContextReference,
) -> Result<(), ContextError> {
    check_reference(
        request.protocol_version,
        &request.result_id,
        request.revision,
    )?;
    manager.result_contexts.release(
        &request.result_id,
        request.revision,
        chrono::Utc::now().timestamp_millis(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn completed_page_and_failed_page_keep_actual_outcome_after_late_cancel() {
        for succeeds in [true, false] {
            let manager = ConnectionPoolManager::new(None);
            let config = serde_json::from_value(serde_json::json!({"id":"读取完成竞争","name":"读取完成竞争","driverType":"sqlite","host":"","port":0,"user":"","password":"","database":":memory:"})).unwrap();
            manager.create_connection(config).await.unwrap();
            let driver = manager.get_connection("读取完成竞争").await.unwrap();
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
            let mut request: TablePageRequest = serde_json::from_value(serde_json::json!({"protocolVersion":1,"connectionId":"读取完成竞争","database":"main","tableParts":["items"],"page":1,"pageSize":1,"sort":null})).unwrap();
            if !succeeds {
                request.sort = Some(TableSort {
                    column: "不存在的排序列".into(),
                    direction: SortDirection::Asc,
                });
            }
            let task_id = Uuid::new_v4().to_string();
            let manager_ref = &manager;
            let outcome = read_dataset_page_with_completion_hook(
                &manager,
                request,
                Some(task_id.clone()),
                None,
                move |task_id| async move {
                    // 精确暂停于上下文已经发布/失败已经确定、TaskGuard 尚未 finish 的边界，无计时假设。
                    assert_eq!(
                        manager_ref
                            .result_contexts
                            .stats(chrono::Utc::now().timestamp_millis())
                            .unwrap()
                            .contexts,
                        usize::from(succeeds)
                    );
                    let _ = manager_ref.tasks.request_cancel(&task_id).await;
                    assert!(
                        manager_ref
                            .tasks
                            .snapshot(&task_id)
                            .unwrap()
                            .cancel_requested
                    );
                },
            )
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
                let page = outcome.ok().unwrap().page;
                assert_eq!(page.result.rows.len(), 1);
                manager
                    .result_contexts
                    .get_context(
                        &page.context.result_id,
                        page.context.revision,
                        chrono::Utc::now().timestamp_millis(),
                    )
                    .unwrap();
            } else {
                assert_eq!(outcome.err().unwrap().code, "INVALID_SORT");
            }
            assert_eq!(manager.tasks.active_count(), 0);
        }
    }

    #[test]
    fn interrupted_page_maps_to_explicit_cancellation_outcome() {
        assert_eq!(
            database_error(crate::models::DatabaseError::QueryCancelled).code,
            "TABLE_TASK_CANCELLED"
        );
    }

    #[test]
    fn mssql_numeric_scale_38_is_rejected_before_native_row_decode() {
        let mut table = ResolvedTable {
            database: "db".into(),
            schema: Some("dbo".into()),
            table: "t".into(),
            object_kind: Some("table".into()),
            columns: vec![ColumnInfo {
                name: "n".into(),
                data_type: "decimal(38,38)".into(),
                nullable: true,
                is_primary_key: false,
                default_value: None,
                extra: None,
                comment: None,
            }],
        };
        assert_eq!(
            check_typed_read_support("mssql", &table).unwrap_err().code,
            "UNSUPPORTED_VALUE_TYPE"
        );
        table.columns[0].data_type = "numeric(38,17)".into();
        check_typed_read_support("mssql", &table).unwrap();
        table.columns[0].data_type = "decimal(38,38)".into();
        check_typed_read_support("postgres", &table).unwrap();
    }

    #[test]
    fn protocol_does_not_accept_sql_or_client_identity() {
        let request = serde_json::json!({"protocolVersion":1,"connectionId":"a","database":"db","tableParts":["t"],"page":1,"pageSize":20});
        assert!(serde_json::from_value::<TablePageRequest>(request.clone()).is_ok());
        for field in [
            "sql",
            "primaryKeys",
            "columns",
            "connectionGeneration",
            "source",
        ] {
            let mut invalid = request.clone();
            invalid[field] = serde_json::json!("injected");
            assert!(
                serde_json::from_value::<TablePageRequest>(invalid).is_err(),
                "{field}"
            );
        }
    }

    #[test]
    fn integer_identity_is_exact_and_rejects_lossy_values() {
        let column = ColumnInfo {
            name: "id".into(),
            data_type: "bigint unsigned".into(),
            nullable: false,
            is_primary_key: true,
            default_value: None,
            extra: None,
            comment: None,
        };
        assert_eq!(
            key_value("mysql", &column, &serde_json::json!("18446744073709551615")),
            Some(RowIdentityValue::UnsignedInteger(
                "18446744073709551615".into()
            ))
        );
        assert!(key_value("mysql", &column, &serde_json::json!("1 OR 1=1")).is_none());
        assert!(key_value("mysql", &column, &serde_json::json!("01")).is_none());
        let signed = ColumnInfo {
            data_type: "BIGINT".into(),
            ..column
        };
        assert_eq!(
            key_value("sqlite", &signed, &serde_json::json!(9007199254740993i64)),
            Some(RowIdentityValue::Integer("9007199254740993".into()))
        );
        assert!(key_value("sqlite", &signed, &serde_json::json!("9007199254740993")).is_none());
        assert!(key_value("postgres", &signed, &serde_json::json!(1.0)).is_none());
        let text = ColumnInfo {
            data_type: "TEXT".into(),
            ..signed
        };
        assert!(key_value("sqlite", &text, &serde_json::json!("不确定转换")).is_none());
    }
}
