use crate::database::result_budget::QueryBudget;
use crate::database::DatabaseDriver;
use crate::models::*;
use crate::session::{SessionCancellation, SessionCapabilities, SessionLease, TargetSnapshot};
use crate::typed_value::{validate_values, TypedQueryResult, TypedValue};
use async_trait::async_trait;
use rusqlite::types::ValueRef;
use rusqlite::{Connection, InterruptHandle, OpenFlags};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex, Weak};
use tokio::sync::{oneshot, Mutex as AsyncMutex, OwnedMutexGuard};

/// 只在专属工作线程内使用，同步 API 不会占用 Tokio 工作线程。
struct SqliteConnection {
    conn: Mutex<Connection>,
    schema_epoch: Arc<AtomicU64>,
}

/// 同连接执行过的目录/连接状态修改即使随后回滚或重置 schema_version，也淘汰旧身份。
/// 外部连接恶意重置 schema_version 不具有此保证；SQLite 没有不可伪造的对象 UUID。
unsafe extern "C" fn sqlite_identity_authorizer(
    context: *mut std::ffi::c_void,
    action: std::ffi::c_int,
    first: *const std::ffi::c_char,
    second: *const std::ffi::c_char,
    _database: *const std::ffi::c_char,
    _trigger: *const std::ffi::c_char,
) -> std::ffi::c_int {
    use rusqlite::ffi;
    let argument = if first.is_null() {
        &b""[..]
    } else {
        std::ffi::CStr::from_ptr(first).to_bytes()
    };
    let metadata_pragma = [
        "table_info",
        "table_xinfo",
        "table_list",
        "index_info",
        "index_xinfo",
        "index_list",
        "foreign_key_list",
    ]
    .iter()
    .any(|name| argument.eq_ignore_ascii_case(name.as_bytes()));
    let changes_schema = matches!(
        action,
        ffi::SQLITE_CREATE_INDEX
            | ffi::SQLITE_CREATE_TABLE
            | ffi::SQLITE_CREATE_TEMP_INDEX
            | ffi::SQLITE_CREATE_TEMP_TABLE
            | ffi::SQLITE_CREATE_TEMP_TRIGGER
            | ffi::SQLITE_CREATE_TEMP_VIEW
            | ffi::SQLITE_CREATE_TRIGGER
            | ffi::SQLITE_CREATE_VIEW
            | ffi::SQLITE_DROP_INDEX
            | ffi::SQLITE_DROP_TABLE
            | ffi::SQLITE_DROP_TEMP_INDEX
            | ffi::SQLITE_DROP_TEMP_TABLE
            | ffi::SQLITE_DROP_TEMP_TRIGGER
            | ffi::SQLITE_DROP_TEMP_VIEW
            | ffi::SQLITE_DROP_TRIGGER
            | ffi::SQLITE_DROP_VIEW
            | ffi::SQLITE_ATTACH
            | ffi::SQLITE_DETACH
            | ffi::SQLITE_ALTER_TABLE
            | ffi::SQLITE_REINDEX
            | ffi::SQLITE_ANALYZE
            | ffi::SQLITE_CREATE_VTABLE
            | ffi::SQLITE_DROP_VTABLE
    ) || (action == ffi::SQLITE_PRAGMA
        && !metadata_pragma
        && (!second.is_null()
            || [
                "optimize",
                "incremental_vacuum",
                "wal_checkpoint",
                "shrink_memory",
            ]
            .iter()
            .any(|name| argument.eq_ignore_ascii_case(name.as_bytes()))))
        || (matches!(
            action,
            ffi::SQLITE_INSERT | ffi::SQLITE_UPDATE | ffi::SQLITE_DELETE
        ) && [
            "sqlite_master",
            "sqlite_schema",
            "sqlite_temp_master",
            "sqlite_temp_schema",
        ]
        .iter()
        .any(|name| argument.eq_ignore_ascii_case(name.as_bytes())));
    if changes_schema {
        // 饱和后 inspect_target 拒绝授予写能力，避免计数回绕重新匹配旧身份。
        let epoch = &*(context.cast::<AtomicU64>());
        let _ = epoch.fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
            value.checked_add(1)
        });
    }
    ffi::SQLITE_OK
}

impl SqliteConnection {
    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>, DatabaseError> {
        self.conn
            .lock()
            .map_err(|_| DatabaseError::Unknown("SQLite 连接锁中毒".into()))
    }
}

fn open_conn(path: &str) -> Result<Connection, DatabaseError> {
    if path == ":memory:" {
        return Connection::open_in_memory()
            .map_err(|e| DatabaseError::ConnectionError(e.to_string()));
    }
    // 文件不存在则创建（SQLite 工具惯例）
    Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
    )
    .map_err(|e| DatabaseError::ConnectionError(format!("打开 {} 失败: {}", path, e)))
}

pub async fn test_connection(config: &ConnectionConfig) -> Result<(), DatabaseError> {
    let path = config
        .database
        .as_deref()
        .filter(|p| !p.is_empty())
        .ok_or_else(|| DatabaseError::ConnectionError("SQLite 连接需要指定数据库文件路径".into()))?
        .to_owned();
    tokio::task::spawn_blocking(move || {
        let conn = open_conn(&path)?;
        conn.query_row("SELECT 1", [], |_| Ok(()))
            .map_err(|e| DatabaseError::ConnectionError(e.to_string()))?;
        Ok(())
    })
    .await
    .map_err(|e| DatabaseError::ConnectionError(e.to_string()))?
}

/// rusqlite 值转 JSON
fn sqlite_raw_length(value: ValueRef<'_>) -> usize {
    match value {
        ValueRef::Null => 0,
        ValueRef::Integer(_) | ValueRef::Real(_) => 8,
        ValueRef::Text(bytes) | ValueRef::Blob(bytes) => bytes.len(),
    }
}

/// 原始载荷通过预算检查后才创建显示值。
fn sqlite_value_to_json(v: ValueRef<'_>) -> Option<serde_json::Value> {
    use serde_json::Value as J;
    match v {
        ValueRef::Null => Some(J::Null),
        ValueRef::Integer(i) => Some(J::from(i)),
        ValueRef::Real(f) => Some(J::from(f)),
        ValueRef::Text(t) => Some(J::String(String::from_utf8_lossy(t).into_owned())),
        ValueRef::Blob(b) => Some(J::String(format!(
            "\\x{}",
            b.iter().map(|x| format!("{:02x}", x)).collect::<String>()
        ))),
    }
}

/// SQLite 只提供五种存储类；不能从声明类型推断布尔、十进制或日期语义。
fn sqlite_value_typed(value: ValueRef<'_>) -> Option<TypedValue> {
    let value = match value {
        ValueRef::Null => TypedValue::Null,
        ValueRef::Integer(value) => TypedValue::Integer(value.to_string()),
        ValueRef::Real(value) => TypedValue::Float(value),
        ValueRef::Text(value) => TypedValue::Text(std::str::from_utf8(value).ok()?.into()),
        ValueRef::Blob(value) => TypedValue::binary(value),
    };
    value.validate().ok()?;
    Some(value)
}

fn sqlite_params(params: &[TypedValue]) -> Result<Vec<rusqlite::types::Value>, DatabaseError> {
    use rusqlite::types::Value;
    validate_values(params)?;
    params
        .iter()
        .map(|value| {
            Ok(match value {
                TypedValue::Null => Value::Null,
                TypedValue::Text(value) => Value::Text(value.clone()),
                // SQLite 没有独立 BOOLEAN 存储类，读取时明确返回整数 0/1。
                TypedValue::Boolean(value) => Value::Integer(i64::from(*value)),
                TypedValue::Integer(value) => Value::Integer(
                    value
                        .parse()
                        .map_err(|_| DatabaseError::TypeError("SQLite int64 参数无效".into()))?,
                ),
                TypedValue::UnsignedInteger(value) => {
                    Value::Integer(value.parse::<i64>().map_err(|_| {
                        DatabaseError::TypeError("SQLite INTEGER 无法无损保存该 uint64".into())
                    })?)
                }
                TypedValue::Float(value) => Value::Real(*value),
                TypedValue::Binary(_) => Value::Blob(value.binary_bytes()?),
                TypedValue::Decimal(_)
                | TypedValue::Date(_)
                | TypedValue::Time(_)
                | TypedValue::Timestamp(_) => {
                    return Err(DatabaseError::TypeError(
                        "SQLite 没有原生十进制或时间存储类，拒绝隐式转换".into(),
                    ));
                }
            })
        })
        .collect()
}

/// 使用 SQLite 自己的解析器检查尾部；不能依赖未启用的 rusqlite extra_check。
fn sqlite_single_statement<'a>(
    conn: &'a Connection,
    sql: &str,
) -> Result<rusqlite::Statement<'a>, DatabaseError> {
    let mut batch = rusqlite::Batch::new(conn, sql);
    let statement = batch
        .next()
        .map_err(map_err)?
        .ok_or_else(|| DatabaseError::QueryError("必须提供一条 SQL 语句".into()))?;
    if batch.next().map_err(map_err)?.is_some() {
        return Err(DatabaseError::QueryError(
            "类型化执行只接受单条 SQL 语句".into(),
        ));
    }
    Ok(statement)
}

/// 虚拟表在 sqlite_schema 中也标为 table，必须结合目录保存的建表语句区分。
/// 无法明确识别的语句保守返回 unknown，避免授予可信行身份。
fn sqlite_object_kind(object_type: &str, create_sql: Option<&str>) -> &'static str {
    if object_type == "view" {
        return "view";
    }
    if object_type != "table" {
        return "unknown";
    }
    let mut words = create_sql.unwrap_or_default().split_whitespace();
    match (words.next(), words.next()) {
        (Some(create), Some(table))
            if create.eq_ignore_ascii_case("CREATE") && table.eq_ignore_ascii_case("TABLE") =>
        {
            "table"
        }
        // CREATE VIRTUAL TABLE 以及未识别的语法均不提供可信表身份。
        _ => "unknown",
    }
}

fn is_interrupt(e: &rusqlite::Error) -> bool {
    e.sqlite_error_code() == Some(rusqlite::ErrorCode::OperationInterrupted)
}

fn map_err(e: rusqlite::Error) -> DatabaseError {
    // sqlite3_interrupt 触发的错误码为 SQLITE_INTERRUPT
    if is_interrupt(&e) {
        DatabaseError::QueryCancelled
    } else {
        DatabaseError::QueryError(e.to_string())
    }
}

impl SqliteConnection {
    fn query_typed(
        &self,
        sql: &str,
        params: &[TypedValue],
        _database: Option<&str>,
    ) -> Result<TypedQueryResult, DatabaseError> {
        let start = std::time::Instant::now();
        let params = sqlite_params(params)?;
        let conn = self.lock()?;
        let mut stmt = sqlite_single_statement(&conn, sql)?;
        let columns: Vec<ColumnMetadata> = stmt
            .columns()
            .iter()
            .map(|column| ColumnMetadata {
                name: column.name().into(),
                data_type: column.decl_type().unwrap_or_default().into(),
                unified_type: "Unknown".into(),
                nullable: true,
                length: None,
                precision: None,
                scale: None,
            })
            .collect();
        let mut rows = Vec::new();
        let mut typed_rows = Vec::new();
        let mut truncated = false;
        let mut budget = QueryBudget::new(&columns)?;
        let mut raw = stmt
            .query(rusqlite::params_from_iter(params.iter()))
            .map_err(map_err)?;
        while let Some(row) = raw.next().map_err(map_err)? {
            let lengths = (0..columns.len())
                .map(|index| row.get_ref(index).map(sqlite_raw_length).map_err(map_err))
                .collect::<Result<Vec<_>, _>>()?;
            if budget.row_limit_reached() || !budget.allows_raw_row(lengths) {
                truncated = true;
                break;
            }
            let mut values = Vec::with_capacity(columns.len());
            let mut typed = Vec::with_capacity(columns.len());
            for index in 0..columns.len() {
                let value = row.get_ref(index).map_err(map_err)?;
                let decoded = sqlite_value_typed(value);
                values.push(
                    decoded
                        .as_ref()
                        .map(TypedValue::display_json)
                        .or_else(|| sqlite_value_to_json(value)),
                );
                typed.push(decoded);
            }
            let row = RowData { values };
            if !budget.try_add_row(&row, Some(&typed))? {
                truncated = true;
                break;
            }
            rows.push(row);
            typed_rows.push(typed);
        }
        let total_count = rows.len();
        Ok(TypedQueryResult {
            result: QueryResult {
                columns,
                rows,
                total_count: Some(total_count),
                execution_time_ms: start.elapsed().as_millis() as u64,
                rows_affected: None,
                last_insert_id: None,
                truncated,
            },
            typed_rows,
        })
    }

    fn execute_bound(
        &self,
        sql: &str,
        params: &[TypedValue],
        _database: Option<&str>,
    ) -> Result<u64, DatabaseError> {
        let params = sqlite_params(params)?;
        let conn = self.lock()?;
        let mut stmt = sqlite_single_statement(&conn, sql)?;
        stmt.execute(rusqlite::params_from_iter(params.iter()))
            .map(|count| count as u64)
            .map_err(map_err)
    }

    fn query(
        &self,
        sql: &str,
        _database: Option<&str>,
        _query_id: Option<u64>,
    ) -> Result<QueryResult, DatabaseError> {
        let start = std::time::Instant::now();
        let conn = self.lock()?;
        let mut stmt = conn.prepare(sql).map_err(map_err)?;

        let col_meta = stmt.columns();
        let columns: Vec<ColumnMetadata> = col_meta
            .iter()
            .map(|c| ColumnMetadata {
                name: c.name().to_string(),
                data_type: c.decl_type().map(|t| t.to_string()).unwrap_or_default(),
                unified_type: "Unknown".to_string(),
                nullable: true,
                length: None,
                precision: None,
                scale: None,
            })
            .collect();
        let col_count = columns.len();

        let mut rows: Vec<RowData> = Vec::new();
        let mut truncated = false;
        let mut budget = QueryBudget::new(&columns)?;
        let mut raw = stmt.query([]).map_err(map_err)?;
        while let Some(r) = raw.next().map_err(map_err)? {
            let lengths = (0..col_count)
                .map(|index| r.get_ref(index).map(sqlite_raw_length).map_err(map_err))
                .collect::<Result<Vec<_>, _>>()?;
            if budget.row_limit_reached() || !budget.allows_raw_row(lengths) {
                truncated = true;
                break;
            }
            let values = (0..col_count)
                .map(|i| r.get_ref(i).ok().map(sqlite_value_to_json).flatten())
                .collect();
            let row = RowData { values };
            if !budget.try_add_row(&row, None)? {
                truncated = true;
                break;
            }
            rows.push(row);
        }

        let total_count = rows.len();
        Ok(QueryResult {
            columns,
            rows,
            total_count: Some(total_count),
            execution_time_ms: start.elapsed().as_millis() as u64,
            rows_affected: None,
            last_insert_id: None,
            truncated,
        })
    }

    fn execute(
        &self,
        sql: &str,
        _database: Option<&str>,
        _query_id: Option<u64>,
    ) -> Result<QueryResult, DatabaseError> {
        let start = std::time::Instant::now();
        let conn = self.lock()?;
        // 单语句用 execute 拿影响行数；含多条语句时退化为 execute_batch（不返回行数）
        let affected = match conn.execute(sql, []) {
            Ok(n) => Some(n as u64),
            Err(e) => {
                if is_interrupt(&e) {
                    return Err(DatabaseError::QueryCancelled);
                }
                // 多语句场景
                conn.execute_batch(sql).map_err(|e2| {
                    if is_interrupt(&e2) {
                        return DatabaseError::QueryCancelled;
                    }
                    DatabaseError::ExecuteError(e.to_string())
                })?;
                None
            }
        };

        Ok(QueryResult {
            columns: vec![],
            rows: vec![],
            total_count: Some(0),
            execution_time_ms: start.elapsed().as_millis() as u64,
            rows_affected: affected,
            last_insert_id: None,
            truncated: false,
        })
    }

    fn get_databases(&self) -> Result<Vec<String>, DatabaseError> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare("PRAGMA database_list").map_err(map_err)?;
        let names = stmt
            .query_map([], |r| r.get::<_, String>(1))
            .map_err(map_err)?
            .filter_map(|r| r.ok())
            // temp 库无实际意义，隐藏
            .filter(|n| n != "temp")
            .collect();
        Ok(names)
    }

    fn catalog_objects(&self, object_type: &str) -> Result<Vec<TableInfo>, DatabaseError> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare("PRAGMA database_list").map_err(map_err)?;
        let attached = stmt
            .query_map([], |r| r.get::<_, String>(1))
            .map_err(map_err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(map_err)?;
        let mut schemas = vec!["temp".to_string(), "main".to_string()];
        schemas.extend(attached.into_iter().filter(|s| s != "temp" && s != "main"));
        let mut objects = Vec::new();
        for schema in schemas {
            let quoted_schema = schema.replace('"', "\"\"");
            let mut stmt = conn.prepare(&format!(
                "SELECT name FROM \"{}\".sqlite_schema WHERE type = ?1 AND name NOT LIKE 'sqlite_%' ORDER BY name", quoted_schema
            )).map_err(map_err)?;
            let names = stmt
                .query_map([object_type], |r| r.get::<_, String>(0))
                .map_err(map_err)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(map_err)?;
            for name in names {
                let row_count = if object_type == "table" {
                    conn.query_row(
                        &format!(
                            "SELECT COUNT(*) FROM \"{}\".\"{}\"",
                            quoted_schema,
                            name.replace('"', "\"\"")
                        ),
                        [],
                        |r| r.get::<_, i64>(0),
                    )
                    .ok()
                } else {
                    None
                };
                objects.push(TableInfo {
                    name,
                    schema: Some(schema.clone()),
                    row_count,
                    engine: (object_type == "table").then(|| "SQLite".to_string()),
                    comment: None,
                });
            }
        }
        Ok(objects)
    }

    fn get_tables(&self, _database: Option<&str>) -> Result<Vec<TableInfo>, DatabaseError> {
        self.catalog_objects("table")
    }

    fn get_views(&self, _database: Option<&str>) -> Result<Vec<TableInfo>, DatabaseError> {
        self.catalog_objects("view")
    }

    fn get_view_definition_for(&self, target: &ResolvedTable) -> Result<String, DatabaseError> {
        let conn = self.lock()?;
        let schema = target
            .schema
            .as_deref()
            .ok_or_else(|| DatabaseError::QueryError("SQLite 视图目标缺少 schema".into()))?;
        conn.query_row(
            &format!(
                "SELECT sql FROM \"{}\".sqlite_schema WHERE type = 'view' AND name = ?1",
                schema.replace('"', "\"\"")
            ),
            [&target.table],
            |r| r.get(0),
        )
        .map_err(map_err)
    }

    fn get_triggers(&self, _database: Option<&str>) -> Result<Vec<TableInfo>, DatabaseError> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(
                "SELECT name, tbl_name FROM sqlite_master WHERE type = 'trigger' ORDER BY name",
            )
            .map_err(map_err)?;
        let triggers = stmt
            .query_map([], |r| {
                let name: String = r.get(0)?;
                let table: String = r.get(1)?;
                Ok(TableInfo {
                    name,
                    schema: Some("main".to_string()),
                    row_count: None,
                    engine: None,
                    comment: Some(format!("ON {}", table)),
                })
            })
            .map_err(map_err)?
            .filter_map(|r| r.ok())
            .collect();
        Ok(triggers)
    }

    fn get_trigger_definition(
        &self,
        _database: Option<&str>,
        trigger: &str,
    ) -> Result<String, DatabaseError> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare("SELECT sql FROM sqlite_master WHERE type = 'trigger' AND name = ?1")
            .map_err(map_err)?;
        let mut rows = stmt.query(rusqlite::params![trigger]).map_err(map_err)?;
        match rows.next().map_err(map_err)? {
            Some(row) => row.get::<_, String>(0).map_err(map_err),
            None => Err(DatabaseError::Unknown(format!("触发器 {} 不存在", trigger))),
        }
    }

    fn get_procedures(&self, _database: Option<&str>) -> Result<Vec<TableInfo>, DatabaseError> {
        // SQLite 不支持存储过程
        Ok(Vec::new())
    }

    fn get_procedure_definition(
        &self,
        _database: Option<&str>,
        procedure: &str,
    ) -> Result<String, DatabaseError> {
        Err(DatabaseError::Unknown(format!(
            "SQLite 不支持存储过程（{}）",
            procedure
        )))
    }

    fn get_functions(&self, _database: Option<&str>) -> Result<Vec<TableInfo>, DatabaseError> {
        // SQLite 不支持自定义存储函数（内置函数不在此处列出）
        Ok(Vec::new())
    }

    fn get_function_definition(
        &self,
        _database: Option<&str>,
        function: &str,
    ) -> Result<String, DatabaseError> {
        Err(DatabaseError::Unknown(format!(
            "SQLite 不支持存储函数（{}）",
            function
        )))
    }

    fn get_users(&self, _database: Option<&str>) -> Result<Vec<DbUserInfo>, DatabaseError> {
        // SQLite 无账号体系
        Ok(Vec::new())
    }

    fn get_current_user(&self, _database: Option<&str>) -> Result<CurrentUserInfo, DatabaseError> {
        Ok(CurrentUserInfo {
            user: "local".to_string(),
            grants: vec!["对数据库文件拥有全部权限（SQLite 无账号体系）".to_string()],
        })
    }

    fn resolve_table(
        &self,
        database: &str,
        table_parts: &[String],
    ) -> Result<ResolvedTable, DatabaseError> {
        let conn = self.lock()?;
        Self::resolve_table_locked(&conn, database, table_parts)
    }

    fn resolve_table_locked(
        conn: &Connection,
        database: &str,
        table_parts: &[String],
    ) -> Result<ResolvedTable, DatabaseError> {
        let (requested_schema, requested_table) = match table_parts {
            [table] => (None, table.as_str()),
            [schema, table] => (Some(schema.as_str()), table.as_str()),
            _ => {
                return Err(DatabaseError::QueryError(
                    "SQLite 表名需要一段或两段标识符".into(),
                ))
            }
        };
        if table_parts.iter().any(String::is_empty) {
            return Err(DatabaseError::QueryError("表名各段不能为空".into()));
        }
        let mut stmt = conn.prepare("PRAGMA database_list").map_err(map_err)?;
        let attached = stmt
            .query_map([], |r| r.get::<_, String>(1))
            .map_err(map_err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(map_err)?;
        // SQLite 未限定表名时依次查找 temp、main 和按 ATTACH 顺序加入的库。
        let mut schemas = vec!["temp".to_string(), "main".to_string()];
        schemas.extend(
            attached
                .into_iter()
                .filter(|name| name != "temp" && name != "main"),
        );
        if let Some(requested) = requested_schema {
            schemas.retain(|name| name.eq_ignore_ascii_case(requested));
        }
        for schema in schemas {
            let sql = format!(
                "SELECT name, type, sql FROM \"{}\".sqlite_schema WHERE type IN ('table', 'view') AND name = ?1 COLLATE NOCASE",
                schema.replace('"', "\"\""),
            );
            let mut identity_stmt = conn.prepare(&sql).map_err(map_err)?;
            let mut rows = identity_stmt.query([requested_table]).map_err(map_err)?;
            if let Some(row) = rows.next().map_err(map_err)? {
                let table: String = row.get(0).map_err(map_err)?;
                let object_type: String = row.get(1).map_err(map_err)?;
                let create_sql: Option<String> = row.get(2).map_err(map_err)?;
                let object_kind = sqlite_object_kind(&object_type, create_sql.as_deref());
                let columns = Self::columns_for_schema(conn, Some(&schema), &table)?;
                if columns.is_empty() {
                    return Err(DatabaseError::QueryError("目标表列信息不可用".into()));
                }
                return Ok(ResolvedTable {
                    database: database.to_string(),
                    schema: Some(schema),
                    table,
                    columns,
                    object_kind: Some(object_kind.into()),
                });
            }
        }
        Err(DatabaseError::QueryError(
            "未找到目标表或指定的 SQLite schema".into(),
        ))
    }

    fn inspect_target(
        &self,
        target: &ResolvedTable,
        lock: bool,
    ) -> Result<TargetSnapshot, DatabaseError> {
        let conn = self.lock()?;
        if conn.is_autocommit() {
            return Err(DatabaseError::QueryError(
                "OBJECT_IDENTITY_UNPROTECTED: SQLite 对象检查必须位于租约事务内".into(),
            ));
        }
        if lock {
            // BEGIN 本身不是写保护；必须已在 main 取得写事务，阻挡其他连接插入 DDL。
            let state = unsafe {
                rusqlite::ffi::sqlite3_txn_state(conn.handle(), b"main\0".as_ptr().cast())
            };
            if state != rusqlite::ffi::SQLITE_TXN_WRITE {
                return Err(DatabaseError::QueryError(
                    "OBJECT_IDENTITY_UNPROTECTED: SQLite 写入检查需要 BEGIN IMMEDIATE".into(),
                ));
            }
        }
        let schema = target.schema.as_deref().ok_or_else(|| {
            DatabaseError::QueryError("OBJECT_IDENTITY_UNSUPPORTED: SQLite 目标缺少 schema".into())
        })?;
        let table = Self::resolve_table_locked(
            &conn,
            &target.database,
            &[schema.to_owned(), target.table.clone()],
        )?;
        let schema = table.schema.as_deref().ok_or_else(|| {
            DatabaseError::QueryError("OBJECT_IDENTITY_UNSUPPORTED: SQLite 目标缺少 schema".into())
        })?;
        let mut write_reasons = Vec::new();
        if schema != "main" {
            write_reasons.push(
                "SQLITE_MAIN_TABLE_REQUIRED: 首版仅支持 main 基础表写入，TEMP/ATTACH 对象只读"
                    .into(),
            );
        }
        if table.object_kind.as_deref() != Some("table")
            || table.table.to_ascii_lowercase().starts_with("sqlite_")
        {
            write_reasons
                .push("SQLITE_BASE_TABLE_REQUIRED: 视图、虚拟表或内部对象不支持写入".into());
        }
        let quoted_schema = schema.replace('"', "\"\"");
        let mut list = conn
            .prepare(&format!("PRAGMA \"{}\".table_list", quoted_schema))
            .map_err(map_err)?;
        let table_list = list
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                ))
            })
            .map_err(map_err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(map_err)?;
        let table_flags = table_list.into_iter().find(|entry| entry.0 == table.table);
        if !matches!(&table_flags, Some((_, kind, count, _, _)) if kind == "table" && *count == table.columns.len() as i64)
        {
            write_reasons.push(
                "SQLITE_OBJECT_UNVERIFIED: 未能完整确认基础表，影子表与隐藏列不支持写入".into(),
            );
        }
        // 检查全部触发器，覆盖外键级联可能触及的其他主库表以及 TEMP 触发器。
        let triggers: i64 = conn.query_row(
            "SELECT (SELECT count(*) FROM main.sqlite_schema WHERE type='trigger') + (SELECT count(*) FROM temp.sqlite_schema WHERE type='trigger')",
            [], |row| row.get(0),
        ).map_err(map_err)?;
        if triggers != 0 {
            write_reasons
                .push("SQLITE_TRIGGER_UNSUPPORTED: 当前连接存在未经验证的 main/TEMP 触发器".into());
        }
        let mut pragmas = Vec::new();
        for (name, required) in [
            ("foreign_keys", Some(1)),
            ("ignore_check_constraints", Some(0)),
            ("writable_schema", Some(0)),
            ("read_uncommitted", Some(0)),
            ("query_only", Some(0)),
            ("defer_foreign_keys", None),
            ("recursive_triggers", None),
            ("trusted_schema", None),
            ("legacy_alter_table", None),
            ("reverse_unordered_selects", None),
        ] {
            let value: i64 = conn
                .query_row(&format!("PRAGMA {}", name), [], |row| row.get(0))
                .map_err(map_err)?;
            if required.is_some_and(|required| value != required) {
                write_reasons.push(format!(
                    "SQLITE_UNSAFE_PRAGMA: {}={} 不满足安全写入条件",
                    name, value
                ));
            }
            pragmas.push((name, value));
        }
        let schema_version: i64 = conn
            .query_row(
                &format!("PRAGMA \"{}\".schema_version", quoted_schema),
                [],
                |row| row.get(0),
            )
            .map_err(map_err)?;
        // 保留完整目录，索引、约束依赖表以及同名同结构重建均参与身份核对。
        let mut catalog = conn.prepare(&format!(
            "SELECT type, name, tbl_name, rootpage, sql FROM \"{}\".sqlite_schema ORDER BY type, name", quoted_schema,
        )).map_err(map_err)?;
        let catalog = catalog
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, Option<String>>(4)?,
                ))
            })
            .map_err(map_err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(map_err)?;
        let mut databases = conn.prepare("PRAGMA database_list").map_err(map_err)?;
        let databases = databases
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .map_err(map_err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(map_err)?;
        let epoch = self.schema_epoch.load(Ordering::Acquire);
        if epoch == u64::MAX {
            write_reasons.push("SQLITE_IDENTITY_EPOCH_EXHAUSTED: 连接身份计数已耗尽".into());
        }
        let identity = serde_json::to_string(&serde_json::json!({
            "backend": "sqlite-main-v1", "schema": schema, "table": table.table,
            "schemaVersion": schema_version, "catalog": catalog, "tableFlags": table_flags,
            "databases": databases, "pragmas": pragmas, "triggers": triggers, "epoch": epoch,
        }))
        .map_err(|error| DatabaseError::Unknown(error.to_string()))?;
        Ok(TargetSnapshot {
            table,
            identity,
            write_reasons,
        })
    }

    fn get_indexes_for(&self, target: &ResolvedTable) -> Result<Vec<IndexInfo>, DatabaseError> {
        let conn = self.lock()?;
        let schema = target
            .schema
            .as_deref()
            .ok_or_else(|| DatabaseError::QueryError("SQLite 元数据目标缺少 schema".into()))?;
        let schema = schema.replace('"', "\"\"");
        let mut stmt = conn
            .prepare(&format!(
                "PRAGMA \"{}\".index_list(\"{}\")",
                schema,
                target.table.replace('"', "\"\"")
            ))
            .map_err(map_err)?;
        let indexes = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)? != 0,
                    r.get::<_, String>(3)?,
                ))
            })
            .map_err(map_err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(map_err)?;
        let mut result = Vec::new();
        for (name, unique, origin) in indexes {
            let mut stmt = conn
                .prepare(&format!(
                    "PRAGMA \"{}\".index_xinfo(\"{}\")",
                    schema,
                    name.replace('"', "\"\"")
                ))
                .map_err(map_err)?;
            let mut entries = stmt
                .query_map([], |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, Option<String>>(2)?,
                        r.get::<_, i64>(5)?,
                        r.get::<_, i64>(3)?,
                    ))
                })
                .map_err(map_err)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(map_err)?;
            entries.sort_by_key(|entry| entry.0);
            let mut columns = Vec::new();
            let mut key_parts = Vec::new();
            for (_, cid, column, key, descending) in entries {
                if key == 0 {
                    continue;
                }
                let column = if cid >= 0 { column } else { None };
                if let Some(column) = &column {
                    columns.push(column.clone());
                }
                key_parts.push(IndexKeyPart {
                    column,
                    expression: None,
                    descending: descending != 0,
                });
            }
            let definition: Option<String> = conn
                .query_row(
                    &format!(
                        "SELECT (SELECT sql FROM \"{}\".sqlite_schema WHERE type = 'index' AND name = ?1)",
                        schema
                    ),
                    [&name],
                    |row| row.get(0),
                )
                .map_err(map_err)?;
            result.push(IndexInfo {
                name,
                columns,
                is_unique: unique,
                is_primary: origin == "pk",
                key_parts,
                included_columns: Vec::new(),
                definition,
                predicate: None,
            });
        }
        Ok(result)
    }

    fn get_foreign_keys_for(
        &self,
        target: &ResolvedTable,
    ) -> Result<Vec<ForeignKeyInfo>, DatabaseError> {
        let conn = self.lock()?;
        let schema = target
            .schema
            .as_deref()
            .ok_or_else(|| DatabaseError::QueryError("SQLite 元数据目标缺少 schema".into()))?;
        let quoted_schema = schema.replace('"', "\"\"");
        let mut stmt = conn
            .prepare(&format!(
                "PRAGMA \"{}\".foreign_key_list(\"{}\")",
                quoted_schema,
                target.table.replace('"', "\"\"")
            ))
            .map_err(map_err)?;
        let mut rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, String>(6)?,
                ))
            })
            .map_err(map_err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(map_err)?;
        rows.sort_by_key(|r| (r.0, r.1));
        let mut groups = std::collections::BTreeMap::new();
        for row in rows {
            groups.entry(row.0).or_insert_with(Vec::new).push(row);
        }
        let mut result = Vec::new();
        for (id, group) in groups {
            let (_, _, ref_table, _, _, on_update, on_delete) = &group[0];
            let mut fk = ForeignKeyInfo {
                name: format!("fk_{}", id),
                columns: Vec::new(),
                ref_table: ref_table.clone(),
                ref_database: Some(target.database.clone()),
                ref_schema: Some(schema.to_owned()),
                ref_columns: Vec::new(),
                on_update: Some(on_update.clone()),
                on_delete: Some(on_delete.clone()),
            };
            let implicit = group.iter().any(|row| row.4.is_none());
            let mut implicit_pk = Vec::new();
            if implicit {
                if group.iter().any(|row| row.4.is_some()) {
                    return Err(DatabaseError::QueryError(
                        "SQLite 同一外键混合显式与隐式引用列".into(),
                    ));
                }
                let mut pk_stmt = conn
                    .prepare(&format!(
                        "PRAGMA \"{}\".table_xinfo(\"{}\")",
                        quoted_schema,
                        ref_table.replace('"', "\"\"")
                    ))
                    .map_err(map_err)?;
                let mut pk = pk_stmt
                    .query_map([], |r| Ok((r.get::<_, i64>(5)?, r.get::<_, String>(1)?)))
                    .map_err(map_err)?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(map_err)?;
                pk.retain(|(ordinal, _)| *ordinal > 0);
                pk.sort_by_key(|(ordinal, _)| *ordinal);
                implicit_pk = pk.into_iter().map(|(_, column)| column).collect();
                if implicit_pk.len() != group.len() {
                    return Err(DatabaseError::QueryError(
                        "METADATA_FOREIGN_KEY_UNSUPPORTED: SQLite 外键列数与隐式父主键不一致"
                            .into(),
                    ));
                }
            }
            for (_, seq, _, from, to, _, _) in group {
                let referenced_column = match to {
                    Some(column) => column,
                    None => implicit_pk.get(usize::try_from(seq).map_err(|_| DatabaseError::QueryError("SQLite 外键序号无效".into()))?).cloned().ok_or_else(|| DatabaseError::QueryError("METADATA_FOREIGN_KEY_UNSUPPORTED: SQLite 隐式引用列无法与父表主键配对".into()))?,
                };
                fk.columns.push(from);
                fk.ref_columns.push(referenced_column);
            }
            result.push(fk);
        }
        Ok(result)
    }

    fn stream_export(
        &self,
        sql: &str,
        sink: Arc<dyn crate::export::ExportSink>,
    ) -> Result<u64, DatabaseError> {
        sink.check_cancelled()?;
        let conn = self.lock()?;
        let mut statement = sqlite_single_statement(&conn, sql)?;
        if !statement.readonly() {
            return Err(DatabaseError::TypeError(
                "EXPORT_QUERY_REQUIRED: 只允许读取语句导出".into(),
            ));
        }
        let columns: Vec<_> = statement
            .column_names()
            .iter()
            .map(|name| crate::export::ExportColumn {
                name: (*name).to_string(),
            })
            .collect();
        sink.columns(&columns)?;
        let mut source = statement.query([]).map_err(map_err)?;
        let mut count = 0;
        while let Some(row) = source.next().map_err(map_err)? {
            sink.check_cancelled()?;
            let lengths = (0..columns.len())
                .map(|index| row.get_ref(index).map(sqlite_raw_length).map_err(map_err))
                .collect::<Result<Vec<_>, _>>()?;
            crate::export::check_raw_lengths(lengths)?;
            let values = (0..columns.len())
                .map(|index| {
                    sqlite_value_typed(row.get_ref(index).map_err(map_err)?).ok_or_else(|| {
                        DatabaseError::TypeError(
                            "EXPORT_TYPE_UNSUPPORTED: SQLite 值无法无损编码，未使用显示值替代"
                                .into(),
                        )
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            sink.row(&values)?;
            count += 1;
        }
        sink.check_cancelled()?;
        Ok(count)
    }

    fn dump_table_for(
        &self,
        target: &ResolvedTable,
        include_drop: bool,
    ) -> Result<(String, u64), DatabaseError> {
        let conn = self.lock()?;
        if target.schema.as_deref() != Some("main") {
            return Err(DatabaseError::TypeError(
                "EXPORT_SCHEMA_UNSUPPORTED: SQL 转储尚不能重建 SQLite temp/ATTACH 目标，请使用 CSV"
                    .into(),
            ));
        }
        let mut out = String::from("-- SQLink 表定义\n\n");
        let quoted = format!("\"main\".\"{}\"", self.escape_identifier(&target.table));
        if include_drop {
            out.push_str(&format!("DROP TABLE IF EXISTS {quoted};\n\n"));
        }
        let create_sql: String = conn
            .query_row(
                "SELECT sql FROM main.sqlite_schema WHERE type = 'table' AND name = ?1",
                [&target.table],
                |row| row.get(0),
            )
            .map_err(map_err)?;
        let span = crate::database::dump::create_table_target_span(&create_sql).ok_or_else(|| {
            DatabaseError::TypeError("EXPORT_SCHEMA_UNSUPPORTED: 未识别 SQLite CREATE TABLE 目标，不能安全限定 main；请使用 CSV".into())
        })?;
        let original_target = &create_sql[span.clone()];
        let parts = crate::database::dump::decode_identifier_parts(original_target);
        if parts.as_deref() != Some(std::slice::from_ref(&target.table)) {
            return Err(DatabaseError::TypeError(
                "EXPORT_SCHEMA_UNSUPPORTED: SQLite 原始定义的目标与已解析对象不一致，请使用 CSV"
                    .into(),
            ));
        }
        // SQLite 目录存放未限定的真实目标；加上 main，仍保留原来的方括号/引号形式。
        let create_target = format!("\"main\".{original_target}");
        let final_length = create_sql
            .len()
            .saturating_sub(span.len())
            .saturating_add(create_target.len());
        if final_length > crate::export::MAX_SQL_STATEMENT_BYTES {
            return Err(DatabaseError::TypeError(
                "EXPORT_SCHEMA_LIMIT: 限定 main 后的单表定义超过 8 MiB".into(),
            ));
        }
        // 仅替换扫描器确认的目标名称 span；默认值、CHECK、注释及列定义保持原文。
        out.push_str(&create_sql[..span.start]);
        out.push_str(&create_target);
        out.push_str(&create_sql[span.end..]);
        if !create_sql.trim_end().ends_with(';') {
            out.push(';');
        }
        out.push_str("\n\n");
        Ok((out, 0))
    }
    fn escape_identifier(&self, identifier: &str) -> String {
        identifier.replace('"', "\"\"")
    }
}

impl SqliteConnection {
    fn columns_for_schema(
        conn: &Connection,
        schema: Option<&str>,
        table: &str,
    ) -> Result<Vec<ColumnInfo>, DatabaseError> {
        let schema_prefix = schema
            .map(|name| format!("\"{}\".", name.replace('"', "\"\"")))
            .unwrap_or_default();
        let mut stmt = conn
            .prepare(&format!(
                "PRAGMA {}table_xinfo(\"{}\")",
                schema_prefix,
                table.replace('"', "\"\"")
            ))
            .map_err(map_err)?;
        let cols = stmt
            .query_map([], |r| {
                let hidden: i64 = r.get(6)?;
                if !matches!(hidden, 0 | 2 | 3) {
                    return Ok(None);
                }
                Ok(Some(ColumnInfo {
                    name: r.get(1)?,
                    data_type: r.get::<_, String>(2)?,
                    nullable: r.get::<_, i64>(3)? == 0,
                    is_primary_key: r.get::<_, i64>(5)? > 0,
                    default_value: r.get(4)?,
                    extra: match hidden {
                        2 => Some("generated virtual".into()),
                        3 => Some("generated stored".into()),
                        _ => None,
                    },
                    comment: None,
                }))
            })
            .map_err(map_err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(map_err)?
            .into_iter()
            .flatten()
            .collect();
        Ok(cols)
    }
}

/// 取消检查和工作线程开始/结束、租约切换共用短锁；锁内绝不执行 SQL。
#[derive(Default)]
struct ActiveSession {
    id: Option<String>,
    running: bool,
    cancelled: bool,
}

struct WorkerState {
    active: Mutex<ActiveSession>,
    interrupt: InterruptHandle,
    connected: AtomicBool,
    isolated: AtomicBool,
    workers: AtomicUsize,
    leases: AtomicUsize,
    operations: AtomicUsize,
}

impl WorkerState {
    fn usable(&self) -> Result<(), DatabaseError> {
        if !self.connected.load(Ordering::Acquire) {
            return Err(DatabaseError::ConnectionError("SQLite 连接已断开".into()));
        }
        if self.isolated.load(Ordering::Acquire) {
            return Err(DatabaseError::ConnectionError(
                "SQLite 会话清理失败，连接已隔离".into(),
            ));
        }
        Ok(())
    }

    fn cancel(&self, id: &str) -> Result<(), DatabaseError> {
        let mut active = self.active.lock().unwrap_or_else(|e| e.into_inner());
        if active.id.as_deref() != Some(id) {
            return Err(DatabaseError::Unknown(
                "SQLite 租约已归还，取消句柄已失效".into(),
            ));
        }
        active.cancelled = true;
        if active.running {
            self.interrupt.interrupt();
        }
        Ok(())
    }
}

type WorkerOperation = Box<dyn FnOnce(&mut SqliteConnection, &Arc<WorkerState>) + Send>;

struct LeaseGuard {
    _guard: OwnedMutexGuard<()>,
    state: Arc<WorkerState>,
}

impl Drop for LeaseGuard {
    fn drop(&mut self) {
        // 关闭线程丢弃队列、发送失败等路径也必须回收租约计数和 gate。
        self.state.leases.fetch_sub(1, Ordering::AcqRel);
    }
}

unsafe extern "C" fn sqlite_progress_callback(
    context: *mut std::ffi::c_void,
) -> std::os::raw::c_int {
    // 指针来自工作线程持有的 Arc，连接关闭前始终有效；回调只由该连接线程调用。
    let state = unsafe { &*(context.cast::<WorkerState>()) };
    let active = state.active.lock().unwrap_or_else(|e| e.into_inner());
    i32::from(active.running && (active.cancelled || !state.connected.load(Ordering::Acquire)))
}

enum WorkerCommand {
    Run(WorkerOperation),
    Abort(String),
    Rollback {
        id: String,
        reply: oneshot::Sender<Result<(), DatabaseError>>,
    },
    Shutdown(oneshot::Sender<Result<(), DatabaseError>>),
    Release {
        id: String,
        // 工作线程确认回滚后才归还 gate，不能在租约 Drop 时直接放行。
        guard: LeaseGuard,
        reply: Option<oneshot::Sender<Result<(), DatabaseError>>>,
    },
}

struct SqliteCore {
    sender: mpsc::Sender<WorkerCommand>,
    gate: Arc<AsyncMutex<()>>,
    state: Arc<WorkerState>,
    queries: Mutex<HashMap<u64, (String, Weak<SqliteCancellation>)>>,
}

/// SQLite 始终使用同一个物理连接；异步 gate 覆盖整个租约和事务。
pub struct SqliteDriver {
    core: Arc<SqliteCore>,
}

/// 用于验证后台工作和租约已完成清理，不包含连接和查询内容。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SqliteSessionStats {
    pub workers: usize,
    pub leases: usize,
    pub operations: usize,
    pub registered_queries: usize,
}

fn cleanup_connection(
    connection: &SqliteConnection,
    state: &WorkerState,
) -> Result<(), DatabaseError> {
    let result = (|| {
        let conn = connection.lock()?;
        if !conn.is_autocommit() {
            conn.execute_batch("ROLLBACK").map_err(map_err)?;
        }
        if !conn.is_autocommit() {
            return Err(DatabaseError::ConnectionError("SQLite 事务仍未结束".into()));
        }
        Ok(())
    })();
    if result.is_err() {
        state.isolated.store(true, Ordering::Release);
    }
    result
}

impl SqliteDriver {
    pub async fn connect(config: &ConnectionConfig) -> Result<Self, DatabaseError> {
        let path = config
            .database
            .as_deref()
            .filter(|p| !p.is_empty())
            .ok_or_else(|| {
                DatabaseError::ConnectionError("SQLite 连接需要指定数据库文件路径".into())
            })?
            .to_owned();
        // 打开文件和验证同样不占用异步运行时。
        tokio::task::spawn_blocking(move || {
            let connection = open_conn(&path)?;
            connection
                .query_row("SELECT 1", [], |_| Ok(()))
                .map_err(map_err)?;
            Self::from_connection(connection)
        })
        .await
        .map_err(|e| DatabaseError::ConnectionError(e.to_string()))?
    }

    fn from_connection(connection: Connection) -> Result<Self, DatabaseError> {
        let state = Arc::new(WorkerState {
            active: Mutex::new(ActiveSession::default()),
            interrupt: connection.get_interrupt_handle(),
            connected: AtomicBool::new(true),
            isolated: AtomicBool::new(false),
            workers: AtomicUsize::new(1),
            leases: AtomicUsize::new(0),
            operations: AtomicUsize::new(0),
        });
        let (sender, receiver) = mpsc::channel();
        let worker_state = state.clone();
        std::thread::Builder::new()
            .name("sqlite-session".into())
            .spawn(move || {
                struct WorkerExit(Arc<WorkerState>);
                impl Drop for WorkerExit {
                    fn drop(&mut self) {
                        self.0.workers.fetch_sub(1, Ordering::AcqRel);
                    }
                }
                let _exit = WorkerExit(worker_state.clone());
                let schema_epoch = Arc::new(AtomicU64::new(0));
                // Arc 的地址在连接整个生命周期内稳定；连接先于 epoch 字段销毁。
                let authorizer_status = unsafe {
                    rusqlite::ffi::sqlite3_set_authorizer(
                        connection.handle(),
                        Some(sqlite_identity_authorizer),
                        Arc::as_ptr(&schema_epoch).cast_mut().cast(),
                    )
                };
                if authorizer_status != rusqlite::ffi::SQLITE_OK {
                    worker_state.isolated.store(true, Ordering::Release);
                    return;
                }
                // interrupt 在 SQL 尚未开始时不会生效；进度回调补足开始边界的取消竞态。
                // 回调指针依赖 worker_state 的 Arc，生命周期长于 connection；全部操作位于此线程。
                unsafe {
                    rusqlite::ffi::sqlite3_progress_handler(
                        connection.handle(),
                        1_000,
                        Some(sqlite_progress_callback),
                        Arc::as_ptr(&worker_state).cast_mut().cast(),
                    );
                }
                let mut connection = SqliteConnection {
                    conn: Mutex::new(connection),
                    schema_epoch,
                };
                while let Ok(command) = receiver.recv() {
                    match command {
                        WorkerCommand::Run(operation) => operation(&mut connection, &worker_state),
                        WorkerCommand::Shutdown(reply) => {
                            let result = cleanup_connection(&connection, &worker_state);
                            let _ = reply.send(result);
                            break;
                        }
                        WorkerCommand::Abort(id) => {
                            let matches = worker_state
                                .active
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .id
                                .as_deref()
                                == Some(&id);
                            if matches {
                                let _ = cleanup_connection(&connection, &worker_state);
                            }
                        }
                        WorkerCommand::Rollback { id, reply } => {
                            let matches = worker_state
                                .active
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .id
                                .as_deref()
                                == Some(&id);
                            // 清理命令绕过用户取消门禁，但不能替其他租约清理事务。
                            // running 已由前一个 SQL 操作解除，进度回调不会中断回滚。
                            let result = if matches {
                                cleanup_connection(&connection, &worker_state)
                            } else {
                                Err(DatabaseError::Unknown(
                                    "SQLite 租约已归还，无法确认回滚".into(),
                                ))
                            };
                            let _ = reply.send(result);
                        }
                        WorkerCommand::Release { id, guard, reply } => {
                            let result = cleanup_connection(&connection, &worker_state);
                            {
                                let mut active = worker_state
                                    .active
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner());
                                if active.id.as_deref() == Some(&id) {
                                    *active = ActiveSession::default();
                                }
                            }
                            drop(guard);
                            if let Some(reply) = reply {
                                let _ = reply.send(result);
                            }
                        }
                    }
                }
                let _ = cleanup_connection(&connection, &worker_state);
            })
            .map_err(|e| {
                DatabaseError::ConnectionError(format!("启动 SQLite 工作线程失败: {}", e))
            })?;
        Ok(Self {
            core: Arc::new(SqliteCore {
                sender,
                gate: Arc::new(AsyncMutex::new(())),
                state,
                queries: Mutex::new(HashMap::new()),
            }),
        })
    }

    pub fn session_stats(&self) -> SqliteSessionStats {
        SqliteSessionStats {
            workers: self.core.state.workers.load(Ordering::Acquire),
            leases: self.core.state.leases.load(Ordering::Acquire),
            operations: self.core.state.operations.load(Ordering::Acquire),
            registered_queries: self
                .core
                .queries
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .len(),
        }
    }

    async fn lease(&self) -> Result<SqliteLease, DatabaseError> {
        self.core.state.usable()?;
        let guard = self.core.gate.clone().lock_owned().await;
        self.core.state.usable()?;
        let id = uuid::Uuid::new_v4().to_string();
        {
            let mut active = self
                .core
                .state
                .active
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            // 与 disconnect 的状态变更互斥，租约不会让断开的连接复活。
            self.core.state.usable()?;
            *active = ActiveSession {
                id: Some(id.clone()),
                running: false,
                cancelled: false,
            };
        }
        self.core.state.leases.fetch_add(1, Ordering::AcqRel);
        Ok(SqliteLease {
            cancellation: Arc::new(SqliteCancellation {
                id: id.clone(),
                state: self.core.state.clone(),
            }),
            id,
            core: self.core.clone(),
            guard: Some(LeaseGuard {
                _guard: guard,
                state: self.core.state.clone(),
            }),
        })
    }

    async fn run<T, F>(&self, operation: F) -> Result<T, DatabaseError>
    where
        T: Send + 'static,
        F: FnOnce(&mut SqliteConnection) -> Result<T, DatabaseError> + Send + 'static,
    {
        let mut lease = self.lease().await?;
        let result = lease.run(operation).await;
        let cleanup = Box::new(lease).release().await;
        match result {
            Ok(value) => {
                cleanup?;
                Ok(value)
            }
            Err(error) => Err(error),
        }
    }

    fn register_query(
        &self,
        query_id: Option<u64>,
        lease: &SqliteLease,
    ) -> Result<QueryRegistration, DatabaseError> {
        if let Some(query_id) = query_id {
            let mut queries = self.core.queries.lock().unwrap_or_else(|e| e.into_inner());
            if queries.contains_key(&query_id) {
                return Err(DatabaseError::QueryError("SQLite query_id 正在使用".into()));
            }
            queries.insert(
                query_id,
                (lease.id.clone(), Arc::downgrade(&lease.cancellation)),
            );
        }
        Ok(QueryRegistration {
            core: self.core.clone(),
            query_id,
            id: lease.id.clone(),
        })
    }
}

struct QueryRegistration {
    core: Arc<SqliteCore>,
    query_id: Option<u64>,
    id: String,
}
impl Drop for QueryRegistration {
    fn drop(&mut self) {
        if let Some(query_id) = self.query_id {
            let mut queries = self.core.queries.lock().unwrap_or_else(|e| e.into_inner());
            if queries.get(&query_id).map(|entry| entry.0.as_str()) == Some(&self.id) {
                queries.remove(&query_id);
            }
        }
    }
}

struct SqliteCancellation {
    id: String,
    state: Arc<WorkerState>,
}
#[async_trait]
impl SessionCancellation for SqliteCancellation {
    async fn cancel(&self) -> Result<(), DatabaseError> {
        self.state.cancel(&self.id)
    }
}

/// Future 被丢弃时，立即中断对应代次，并让工作线程执行事务清理。
struct PendingOperation {
    core: Arc<SqliteCore>,
    id: String,
    armed: bool,
}
impl Drop for PendingOperation {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.core.state.cancel(&self.id);
            let _ = self.core.sender.send(WorkerCommand::Abort(self.id.clone()));
        }
    }
}

struct SqliteLease {
    id: String,
    core: Arc<SqliteCore>,
    cancellation: Arc<SqliteCancellation>,
    guard: Option<LeaseGuard>,
}

impl SqliteLease {
    async fn run<T, F>(&mut self, operation: F) -> Result<T, DatabaseError>
    where
        T: Send + 'static,
        F: FnOnce(&mut SqliteConnection) -> Result<T, DatabaseError> + Send + 'static,
    {
        self.core.state.usable()?;
        let id = self.id.clone();
        let (sender, receiver) = oneshot::channel();
        let mut pending = PendingOperation {
            core: self.core.clone(),
            id: id.clone(),
            armed: true,
        };
        self.core
            .sender
            .send(WorkerCommand::Run(Box::new(move |connection, state| {
                let started = {
                    let mut active = state.active.lock().unwrap_or_else(|e| e.into_inner());
                    if active.id.as_deref() != Some(&id)
                        || active.cancelled
                        || state.usable().is_err()
                    {
                        false
                    } else {
                        active.running = true;
                        true
                    }
                };
                if !started {
                    let _ = sender.send(Err(DatabaseError::QueryCancelled));
                    return;
                }
                state.operations.fetch_add(1, Ordering::AcqRel);
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    operation(connection)
                }))
                .unwrap_or_else(|_| {
                    Err(DatabaseError::Unknown("SQLite 工作操作发生 panic".into()))
                });
                state.operations.fetch_sub(1, Ordering::AcqRel);
                let cancelled = {
                    let mut active = state.active.lock().unwrap_or_else(|e| e.into_inner());
                    active.running = false;
                    active.cancelled || active.id.as_deref() != Some(&id)
                };
                if result.is_err() || cancelled || sender.is_closed() {
                    let _ = cleanup_connection(connection, state);
                }
                // 取消只是请求，不能把已经自动提交并确认成功的写入改报为取消。
                // 真正的 SQLite 中断仍由 map_err 返回 QueryCancelled；批任务另行确认回滚。
                let _ = sender.send(result);
            })))
            .map_err(|_| DatabaseError::ConnectionError("SQLite 工作线程已退出".into()))?;
        let result = receiver
            .await
            .map_err(|_| DatabaseError::ConnectionError("SQLite 工作线程未返回结果".into()))?;
        pending.armed = false;
        result
    }

    fn release_to_worker(&mut self, reply: Option<oneshot::Sender<Result<(), DatabaseError>>>) {
        if let Some(guard) = self.guard.take() {
            {
                let mut active = self
                    .core
                    .state
                    .active
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                if active.id.as_deref() == Some(&self.id) {
                    if active.running {
                        self.core.state.interrupt.interrupt();
                    }
                    // 清理期间旧取消句柄已失效；gate 仍由 Release 命令持有。
                    active.id = None;
                    active.cancelled = true;
                    // running 由工作线程在 SQL 返回后解除；若此处提前清除，
                    // SQLite 开始前丢失的 interrupt 将无法再由进度回调补偿。
                }
            }
            if self
                .core
                .sender
                .send(WorkerCommand::Release {
                    id: self.id.clone(),
                    guard,
                    reply,
                })
                .is_err()
            {
                self.core.state.isolated.store(true, Ordering::Release);
            }
        }
    }
}

impl Drop for SqliteLease {
    fn drop(&mut self) {
        self.release_to_worker(None);
    }
}

#[async_trait]
impl SessionLease for SqliteLease {
    fn session_id(&self) -> &str {
        &self.id
    }
    fn capabilities(&self) -> SessionCapabilities {
        SessionCapabilities::transaction_only()
    }
    fn cancel_handle(&self) -> Option<Arc<dyn SessionCancellation>> {
        Some(self.cancellation.clone())
    }
    async fn stream_export(
        &mut self,
        sql: &str,
        sink: Arc<dyn crate::export::ExportSink>,
    ) -> Result<u64, DatabaseError> {
        let sql = sql.to_owned();
        self.run(move |connection| connection.stream_export(&sql, sink))
            .await
    }
    async fn query(&mut self, sql: &str) -> Result<QueryResult, DatabaseError> {
        let sql = sql.to_owned();
        self.run(move |connection| connection.query(&sql, None, None))
            .await
    }
    async fn execute(&mut self, sql: &str) -> Result<QueryResult, DatabaseError> {
        let sql = sql.to_owned();
        self.run(move |connection| connection.execute(&sql, None, None))
            .await
    }
    async fn query_typed(
        &mut self,
        sql: &str,
        params: &[TypedValue],
    ) -> Result<TypedQueryResult, DatabaseError> {
        let (sql, params) = (sql.to_owned(), params.to_vec());
        self.run(move |connection| connection.query_typed(&sql, &params, None))
            .await
    }
    async fn execute_bound(
        &mut self,
        sql: &str,
        params: &[TypedValue],
    ) -> Result<u64, DatabaseError> {
        let (sql, params) = (sql.to_owned(), params.to_vec());
        self.run(move |connection| connection.execute_bound(&sql, &params, None))
            .await
    }
    async fn begin(&mut self) -> Result<(), DatabaseError> {
        self.run(|connection| connection.lock()?.execute_batch("BEGIN").map_err(map_err))
            .await
    }
    async fn begin_write(&mut self) -> Result<(), DatabaseError> {
        self.run(|connection| {
            connection
                .lock()?
                .execute_batch("BEGIN IMMEDIATE")
                .map_err(map_err)
        })
        .await
    }
    async fn inspect_target(
        &mut self,
        target: &ResolvedTable,
        lock: bool,
    ) -> Result<TargetSnapshot, DatabaseError> {
        let target = target.clone();
        self.run(move |connection| connection.inspect_target(&target, lock))
            .await
    }
    async fn commit(&mut self) -> Result<(), DatabaseError> {
        self.run(|connection| connection.lock()?.execute_batch("COMMIT").map_err(map_err))
            .await
    }
    async fn rollback(&mut self) -> Result<(), DatabaseError> {
        let (sender, receiver) = oneshot::channel();
        self.core
            .sender
            .send(WorkerCommand::Rollback {
                id: self.id.clone(),
                reply: sender,
            })
            .map_err(|_| DatabaseError::ConnectionError("SQLite 回滚工作线程已退出".into()))?;
        // 不清除 cancelled：确认回滚不能重新授予本租约执行 SQL 的权限。
        receiver
            .await
            .map_err(|_| DatabaseError::ConnectionError("SQLite 回滚工作线程未返回结果".into()))?
    }
    async fn release(mut self: Box<Self>) -> Result<(), DatabaseError> {
        let (sender, receiver) = oneshot::channel();
        self.release_to_worker(Some(sender));
        receiver
            .await
            .map_err(|_| DatabaseError::ConnectionError("SQLite 清理工作线程已退出".into()))?
    }
}

#[async_trait]
impl DatabaseDriver for SqliteDriver {
    fn driver_type(&self) -> &'static str {
        "sqlite"
    }
    fn is_connected(&self) -> bool {
        self.core.state.usable().is_ok()
    }
    async fn validate_export_path(&self, path: &str) -> Result<(), DatabaseError> {
        let path = path.to_owned();
        self.run(move |connection| {
            let conn = connection.lock()?;
            let mut statement = conn.prepare("PRAGMA database_list").map_err(map_err)?;
            let files = statement
                .query_map([], |row| row.get::<_, String>(2))
                .map_err(map_err)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(map_err)?;
            crate::export::reject_database_paths(&path, &files)
        })
        .await
    }
    fn session_capabilities(&self) -> SessionCapabilities {
        SessionCapabilities::transaction_only()
    }
    async fn lease_session(
        &self,
        _database: Option<&str>,
    ) -> Result<Box<dyn SessionLease>, DatabaseError> {
        Ok(Box::new(self.lease().await?))
    }
    async fn cancel_query(&self, query_id: u64) -> Result<(), DatabaseError> {
        let cancellation = self
            .core
            .queries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&query_id)
            .and_then(|entry| entry.1.upgrade());
        match cancellation {
            Some(handle) => handle.cancel().await,
            None => Err(DatabaseError::Unknown("SQLite 查询不存在或已结束".into())),
        }
    }
    async fn execute_batch(
        &self,
        sqls: Vec<String>,
        _database: Option<&str>,
    ) -> Result<u64, DatabaseError> {
        let mut lease = self.lease().await?;
        lease.begin().await?;
        let mut total = 0;
        for sql in sqls {
            match lease.execute_bound(&sql, &[]).await {
                Ok(count) => total += count,
                Err(error) => {
                    let _ = lease.rollback().await;
                    let _ = Box::new(lease).release().await;
                    return Err(error);
                }
            }
        }
        lease.commit().await?;
        Box::new(lease).release().await?;
        Ok(total)
    }
    async fn disconnect(&self) -> Result<(), DatabaseError> {
        {
            let mut active = self
                .core
                .state
                .active
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            self.core.state.connected.store(false, Ordering::Release);
            self.core.state.isolated.store(true, Ordering::Release);
            active.cancelled = true;
            if active.running {
                self.core.state.interrupt.interrupt();
            }
        }
        let (sender, receiver) = oneshot::channel();
        if self
            .core
            .sender
            .send(WorkerCommand::Shutdown(sender))
            .is_err()
        {
            return Ok(());
        }
        receiver
            .await
            .map_err(|_| DatabaseError::ConnectionError("SQLite 断开清理工作线程已退出".into()))?
    }
    fn escape_identifier(&self, identifier: &str) -> String {
        identifier.replace('"', "\"\"")
    }
    async fn query_typed(
        &self,
        sql: &str,
        params: &[TypedValue],
        _database: Option<&str>,
    ) -> Result<TypedQueryResult, DatabaseError> {
        let sql = sql.to_owned();
        let params = params.to_vec();
        self.run(move |connection| connection.query_typed(&sql, &params, None))
            .await
    }
    async fn execute_bound(
        &self,
        sql: &str,
        params: &[TypedValue],
        _database: Option<&str>,
    ) -> Result<u64, DatabaseError> {
        let sql = sql.to_owned();
        let params = params.to_vec();
        self.run(move |connection| connection.execute_bound(&sql, &params, None))
            .await
    }
    async fn query(
        &self,
        sql: &str,
        _database: Option<&str>,
        query_id: Option<u64>,
    ) -> Result<QueryResult, DatabaseError> {
        let mut lease = self.lease().await?;
        let registration = self.register_query(query_id, &lease)?;
        let result = lease.query(sql).await;
        drop(registration);
        let cleanup = Box::new(lease).release().await;
        match result {
            Ok(value) => {
                cleanup?;
                Ok(value)
            }
            Err(error) => Err(error),
        }
    }
    async fn execute(
        &self,
        sql: &str,
        _database: Option<&str>,
        query_id: Option<u64>,
    ) -> Result<QueryResult, DatabaseError> {
        let mut lease = self.lease().await?;
        let registration = self.register_query(query_id, &lease)?;
        let result = lease.execute(sql).await;
        drop(registration);
        let cleanup = Box::new(lease).release().await;
        match result {
            Ok(value) => {
                cleanup?;
                Ok(value)
            }
            Err(error) => Err(error),
        }
    }
    async fn get_databases(&self) -> Result<Vec<String>, DatabaseError> {
        self.run(move |connection| connection.get_databases()).await
    }
    async fn get_tables(&self, _database: Option<&str>) -> Result<Vec<TableInfo>, DatabaseError> {
        self.run(move |connection| connection.get_tables(None))
            .await
    }
    async fn get_views(&self, _database: Option<&str>) -> Result<Vec<TableInfo>, DatabaseError> {
        self.run(move |connection| connection.get_views(None)).await
    }
    async fn get_view_definition(
        &self,
        database: Option<&str>,
        view: &str,
    ) -> Result<String, DatabaseError> {
        let target = crate::metadata::resolve_legacy(self, database, view).await?;
        self.get_view_definition_for(&target).await
    }
    async fn get_view_definition_for(
        &self,
        target: &ResolvedTable,
    ) -> Result<String, DatabaseError> {
        let target = target.clone();
        self.run(move |connection| connection.get_view_definition_for(&target))
            .await
    }
    async fn get_triggers(&self, _database: Option<&str>) -> Result<Vec<TableInfo>, DatabaseError> {
        self.run(move |connection| connection.get_triggers(None))
            .await
    }
    async fn get_trigger_definition(
        &self,
        _database: Option<&str>,
        trigger: &str,
    ) -> Result<String, DatabaseError> {
        let trigger = trigger.to_owned();
        self.run(move |connection| connection.get_trigger_definition(None, &trigger))
            .await
    }
    async fn get_procedures(
        &self,
        _database: Option<&str>,
    ) -> Result<Vec<TableInfo>, DatabaseError> {
        self.run(move |connection| connection.get_procedures(None))
            .await
    }
    async fn get_procedure_definition(
        &self,
        _database: Option<&str>,
        procedure: &str,
    ) -> Result<String, DatabaseError> {
        let procedure = procedure.to_owned();
        self.run(move |connection| connection.get_procedure_definition(None, &procedure))
            .await
    }
    async fn get_functions(
        &self,
        _database: Option<&str>,
    ) -> Result<Vec<TableInfo>, DatabaseError> {
        self.run(move |connection| connection.get_functions(None))
            .await
    }
    async fn get_function_definition(
        &self,
        _database: Option<&str>,
        function: &str,
    ) -> Result<String, DatabaseError> {
        let function = function.to_owned();
        self.run(move |connection| connection.get_function_definition(None, &function))
            .await
    }
    async fn get_users(&self, _database: Option<&str>) -> Result<Vec<DbUserInfo>, DatabaseError> {
        self.run(move |connection| connection.get_users(None)).await
    }
    async fn get_current_user(
        &self,
        _database: Option<&str>,
    ) -> Result<CurrentUserInfo, DatabaseError> {
        self.run(move |connection| connection.get_current_user(None))
            .await
    }
    async fn resolve_table(
        &self,
        database: &str,
        table_parts: &[String],
    ) -> Result<ResolvedTable, DatabaseError> {
        let database = database.to_owned();
        let table_parts = table_parts.to_vec();
        self.run(move |connection| connection.resolve_table(&database, &table_parts))
            .await
    }
    async fn get_columns(
        &self,
        database: Option<&str>,
        table: &str,
    ) -> Result<Vec<ColumnInfo>, DatabaseError> {
        Ok(crate::metadata::resolve_legacy(self, database, table)
            .await?
            .columns)
    }
    async fn get_indexes(
        &self,
        database: Option<&str>,
        table: &str,
    ) -> Result<Vec<IndexInfo>, DatabaseError> {
        let target = crate::metadata::resolve_legacy(self, database, table).await?;
        self.get_indexes_for(&target).await
    }
    async fn get_indexes_for(
        &self,
        target: &ResolvedTable,
    ) -> Result<Vec<IndexInfo>, DatabaseError> {
        let target = target.clone();
        self.run(move |connection| connection.get_indexes_for(&target))
            .await
    }
    async fn get_foreign_keys(
        &self,
        database: Option<&str>,
        table: &str,
    ) -> Result<Vec<ForeignKeyInfo>, DatabaseError> {
        let target = crate::metadata::resolve_legacy(self, database, table).await?;
        self.get_foreign_keys_for(&target).await
    }
    async fn get_foreign_keys_for(
        &self,
        target: &ResolvedTable,
    ) -> Result<Vec<ForeignKeyInfo>, DatabaseError> {
        let target = target.clone();
        self.run(move |connection| connection.get_foreign_keys_for(&target))
            .await
    }
    async fn export_table_csv(
        &self,
        database: Option<&str>,
        table: &str,
        path: &str,
    ) -> Result<u64, DatabaseError> {
        crate::export::export_csv(self, database, table, path).await
    }
    async fn dump_table(
        &self,
        database: Option<&str>,
        table: &str,
        include_drop: bool,
        include_data: bool,
    ) -> Result<(String, u64), DatabaseError> {
        let target = crate::metadata::resolve_legacy(self, database, table).await?;
        self.dump_table_for(&target, include_drop, include_data)
            .await
    }
    async fn dump_table_for(
        &self,
        target: &ResolvedTable,
        include_drop: bool,
        include_data: bool,
    ) -> Result<(String, u64), DatabaseError> {
        let owned_target = target.clone();
        let (mut out, _) = self
            .run(move |connection| connection.dump_table_for(&owned_target, include_drop))
            .await?;
        let count = if include_data {
            crate::export::append_dump_data_for(self, target, &mut out).await?
        } else {
            0
        };
        Ok((out, count))
    }
}

#[cfg(test)]
mod session_cleanup_tests {
    use super::*;
    use std::time::Duration;

    async fn stopped(state: &WorkerState) {
        tokio::time::timeout(Duration::from_secs(3), async {
            while state.workers.load(Ordering::Acquire) != 0 {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("工作线程必须随最后一个连接所有者退出");
    }

    const LONG_QUERY: &str = "WITH RECURSIVE n(x) AS (VALUES(0) UNION ALL SELECT x+1 FROM n WHERE x<1000000000) SELECT sum(x) FROM n";

    /// 在 worker 已标记 running、但还未调用任何 SQLite API 时精确暂停。
    fn paused_query(
        mut lease: SqliteLease,
    ) -> (
        tokio::task::JoinHandle<Result<QueryResult, DatabaseError>>,
        oneshot::Receiver<()>,
        mpsc::Sender<()>,
    ) {
        let (started, entered) = oneshot::channel();
        let (resume, proceed) = mpsc::channel();
        let task = tokio::spawn(async move {
            let result = lease
                .run(move |connection| {
                    let _ = started.send(());
                    proceed
                        .recv()
                        .map_err(|e| DatabaseError::Unknown(e.to_string()))?;
                    connection.query(LONG_QUERY, None, None)
                })
                .await;
            Box::new(lease).release().await?;
            result
        });
        (task, entered, resume)
    }

    #[tokio::test]
    async fn dropping_owner_before_sql_starts_keeps_progress_cancellation_armed() {
        let driver = SqliteDriver::from_connection(Connection::open_in_memory().unwrap()).unwrap();
        driver
            .execute_bound("CREATE TABLE samples(value INTEGER)", &[], None)
            .await
            .unwrap();
        let mut lease = driver.lease().await.unwrap();
        lease.begin().await.unwrap();
        lease
            .execute_bound("INSERT INTO samples VALUES (1)", &[])
            .await
            .unwrap();
        let (task, entered, resume) = paused_query(lease);
        tokio::time::timeout(Duration::from_secs(3), entered)
            .await
            .unwrap()
            .unwrap();
        // 此时没有活跃 SQLite 语句，单靠 interrupt 必然无法取消随后开始的 SQL。
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        {
            let active = driver.core.state.active.lock().unwrap();
            assert!(active.id.is_none());
            assert!(active.cancelled);
            assert!(
                active.running,
                "Drop 后仍须保留 worker 的执行状态直到 SQL 返回"
            );
        }
        resume.send(()).unwrap();
        let result = tokio::time::timeout(
            Duration::from_secs(3),
            driver.query("SELECT count(*) FROM samples", None, None),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(result.rows[0].values[0], Some(serde_json::json!(0)));
        assert_eq!(driver.session_stats().leases, 0);
        assert_eq!(driver.session_stats().operations, 0);
    }

    #[tokio::test]
    async fn cancelling_before_sql_starts_is_observed_by_the_progress_callback() {
        let driver = SqliteDriver::from_connection(Connection::open_in_memory().unwrap()).unwrap();
        let lease = driver.lease().await.unwrap();
        let cancellation = lease.cancel_handle().unwrap();
        let (task, entered, resume) = paused_query(lease);
        tokio::time::timeout(Duration::from_secs(3), entered)
            .await
            .unwrap()
            .unwrap();
        cancellation.cancel().await.unwrap();
        resume.send(()).unwrap();
        let result = tokio::time::timeout(Duration::from_secs(3), task)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(result, Err(DatabaseError::QueryCancelled)));
        let result = driver.query("SELECT 1", None, None).await.unwrap();
        assert_eq!(result.rows[0].values[0], Some(serde_json::json!(1)));
        assert_eq!(driver.session_stats().leases, 0);
        assert_eq!(driver.session_stats().operations, 0);
    }

    #[tokio::test]
    async fn autocommit_success_is_not_reclassified_by_late_cancellation() {
        let driver = SqliteDriver::from_connection(Connection::open_in_memory().unwrap()).unwrap();
        driver
            .execute_bound("CREATE TABLE samples(value INTEGER)", &[], None)
            .await
            .unwrap();
        let mut lease = driver.lease().await.unwrap();
        let cancellation = lease.cancel_handle().unwrap();
        let (committed, observed_commit) = oneshot::channel();
        let (resume, proceed) = mpsc::channel();
        let task = tokio::spawn(async move {
            let result = lease
                .run(move |connection| {
                    let result =
                        connection.execute_bound("INSERT INTO samples VALUES (1)", &[], None);
                    // SQL 已经结束并自动提交，精确暂停在 run 处理返回值之前。
                    let _ = committed.send(());
                    proceed
                        .recv()
                        .map_err(|e| DatabaseError::Unknown(e.to_string()))?;
                    result
                })
                .await;
            Box::new(lease).release().await?;
            result
        });
        tokio::time::timeout(Duration::from_secs(3), observed_commit)
            .await
            .unwrap()
            .unwrap();
        cancellation.cancel().await.unwrap();
        resume.send(()).unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(3), task)
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
            1
        );
        let result = driver
            .query("SELECT count(*) FROM samples", None, None)
            .await
            .unwrap();
        assert_eq!(result.rows[0].values[0], Some(serde_json::json!(1)));
        assert_eq!(driver.session_stats().leases, 0);
        assert_eq!(driver.session_stats().operations, 0);
    }

    #[tokio::test]
    async fn worker_panic_rolls_back_and_can_reuse_a_proven_clean_connection() {
        let driver = SqliteDriver::from_connection(Connection::open_in_memory().unwrap()).unwrap();
        driver
            .execute_bound("CREATE TABLE samples(value INTEGER)", &[], None)
            .await
            .unwrap();
        let mut lease = driver.lease().await.unwrap();
        lease.begin().await.unwrap();
        lease
            .execute_bound("INSERT INTO samples VALUES (1)", &[])
            .await
            .unwrap();
        let failed: Result<(), DatabaseError> = lease.run(|_| panic!("模拟工作操作 panic")).await;
        assert!(failed.is_err());
        let result = lease.query("SELECT count(*) FROM samples").await.unwrap();
        assert_eq!(result.rows[0].values[0], Some(serde_json::json!(0)));
        Box::new(lease).release().await.unwrap();
        assert_eq!(driver.session_stats().leases, 0);
        let state = driver.core.state.clone();
        drop(driver);
        stopped(&state).await;
    }

    #[tokio::test]
    async fn unprovable_cleanup_isolates_instead_of_reopening_the_memory_database() {
        let driver = SqliteDriver::from_connection(Connection::open_in_memory().unwrap()).unwrap();
        let mut lease = driver.lease().await.unwrap();
        lease.begin().await.unwrap();
        let failed: Result<(), DatabaseError> = lease
            .run(|connection| {
                let _guard = connection.lock()?;
                panic!("模拟持连接锁的操作 panic，清理无法证明连接安全");
            })
            .await;
        assert!(failed.is_err());
        assert!(Box::new(lease).release().await.is_err());
        assert!(!driver.is_connected());
        assert!(driver.lease_session(None).await.is_err());
        assert_eq!(driver.session_stats().leases, 0);
        let state = driver.core.state.clone();
        drop(driver);
        stopped(&state).await;
    }

    #[tokio::test]
    async fn dropping_the_last_lease_and_driver_stops_the_worker() {
        let driver = SqliteDriver::from_connection(Connection::open_in_memory().unwrap()).unwrap();
        let state = driver.core.state.clone();
        let mut lease = driver.lease_session(None).await.unwrap();
        lease.begin().await.unwrap();
        drop(driver);
        drop(lease);
        stopped(&state).await;
        assert_eq!(state.leases.load(Ordering::Acquire), 0);
        assert_eq!(state.operations.load(Ordering::Acquire), 0);
    }
}

#[cfg(test)]
mod resolve_tests {
    use super::*;

    fn driver(sql: &str) -> SqliteDriver {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(sql).unwrap();
        SqliteDriver::from_connection(conn).unwrap()
    }

    async fn resolve(driver: &SqliteDriver, parts: &[&str]) -> ResolvedTable {
        driver
            .resolve_table(
                "/数据库/当前文件.db",
                &parts.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            )
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn resolves_temp_main_and_attached_in_sqlite_order() {
        let driver = driver(
            "ATTACH DATABASE ':memory:' AS first; ATTACH DATABASE ':memory:' AS second;
            CREATE TABLE main.shared(main_id INTEGER PRIMARY KEY, main_value TEXT);
            CREATE TEMP TABLE shared(temp_id INTEGER PRIMARY KEY);
            CREATE TABLE first.shared(first_id INTEGER PRIMARY KEY);
            CREATE TABLE second.shared(second_id INTEGER PRIMARY KEY);",
        );
        let temp = resolve(&driver, &["shared"]).await;
        assert_eq!(temp.schema.as_deref(), Some("temp"));
        assert_eq!(temp.columns[0].name, "temp_id");
        assert!(temp.columns[0].is_primary_key);
        let main = resolve(&driver, &["MAIN", "SHARED"]).await;
        assert_eq!(main.database, "/数据库/当前文件.db");
        assert_eq!(main.schema.as_deref(), Some("main"));
        assert_eq!(main.table, "shared");
        assert_eq!(main.object_kind.as_deref(), Some("table"));
        assert_eq!(main.columns.len(), 2);
        assert_eq!(main.columns[0].name, "main_id");
        assert!(!main.columns[1].is_primary_key);
        assert_eq!(
            resolve(&driver, &["first", "shared"]).await.columns[0].name,
            "first_id"
        );
        assert_eq!(
            resolve(&driver, &["second", "shared"]).await.columns[0].name,
            "second_id"
        );
        driver
            .execute("DROP TABLE temp.shared", None, None)
            .await
            .unwrap();
        assert_eq!(
            resolve(&driver, &["shared"]).await.schema.as_deref(),
            Some("main")
        );
        driver
            .execute("DROP TABLE main.shared", None, None)
            .await
            .unwrap();
        assert_eq!(
            resolve(&driver, &["shared"]).await.schema.as_deref(),
            Some("first")
        );
    }

    #[tokio::test]
    async fn preserves_decoded_dots_and_quotes_in_each_identifier() {
        let driver = driver(
            r#"ATTACH DATABASE ':memory:' AS "库.一""二";
            CREATE TABLE "库.一""二"."表.一""二"("键.一""二" INTEGER PRIMARY KEY, value TEXT DEFAULT '值');
            CREATE TABLE main."main.dot"(id INTEGER PRIMARY KEY);"#,
        );
        let table = resolve(&driver, &["库.一\"二", "表.一\"二"]).await;
        assert_eq!(table.schema.as_deref(), Some("库.一\"二"));
        assert_eq!(table.table, "表.一\"二");
        assert_eq!(table.columns[0].name, "键.一\"二");
        assert!(table.columns[0].is_primary_key);
        assert_eq!(table.columns[1].default_value.as_deref(), Some("'值'"));
        assert_eq!(resolve(&driver, &["main.dot"]).await.table, "main.dot");
        // 已解码的一段标识符不会再次按点号拆分。
        assert!(driver
            .resolve_table("context", &["库.一\"二.表.一\"二".into()])
            .await
            .is_err());
    }

    #[tokio::test]
    async fn resolves_views_and_rejects_missing_or_unsupported_qualifiers() {
        let driver = driver("CREATE TABLE main.present(id INTEGER PRIMARY KEY); CREATE VIEW readonly_view AS SELECT * FROM present;");
        let view = resolve(&driver, &["readonly_view"]).await;
        assert_eq!(view.table, "readonly_view");
        assert_eq!(view.object_kind.as_deref(), Some("view"));
        assert_eq!(view.columns[0].name, "id");
        assert!(!view.columns[0].is_primary_key);
        for parts in [
            vec![],
            vec![""],
            vec!["missing"],
            vec!["missing_schema", "present"],
            vec!["main", "", "present"],
            vec!["main\"; DROP TABLE present;--", "present"],
        ] {
            let parts = parts.iter().map(|s| s.to_string()).collect::<Vec<_>>();
            assert!(driver.resolve_table("context", &parts).await.is_err());
        }
        assert_eq!(resolve(&driver, &["present"]).await.columns[0].name, "id");
    }

    #[tokio::test]
    async fn virtual_tables_do_not_receive_trusted_table_identity() {
        let driver = driver("CREATE VIRTUAL TABLE virtual_rows USING fts5(content);");
        let virtual_table = resolve(&driver, &["virtual_rows"]).await;
        assert_eq!(virtual_table.table, "virtual_rows");
        assert_eq!(virtual_table.columns[0].name, "content");
        assert_eq!(virtual_table.columns.len(), 1);
        assert_eq!(virtual_table.object_kind.as_deref(), Some("unknown"));
    }

    #[tokio::test]
    async fn preserves_generated_columns_and_their_storage_kind() {
        let driver = driver(
            "CREATE TABLE generated_columns(
                id INTEGER PRIMARY KEY,
                ordinary INTEGER,
                derived_virtual INTEGER GENERATED ALWAYS AS (ordinary + 1) VIRTUAL,
                derived_stored INTEGER GENERATED ALWAYS AS (ordinary * 2) STORED
            );",
        );
        let table = resolve(&driver, &["generated_columns"]).await;
        assert_eq!(table.columns.len(), 4);
        assert_eq!(table.columns[0].name, "id");
        assert!(table.columns[0].is_primary_key);
        assert_eq!(table.columns[0].extra, None);
        assert_eq!(table.columns[1].name, "ordinary");
        assert_eq!(table.columns[1].extra, None);
        assert_eq!(table.columns[2].name, "derived_virtual");
        assert_eq!(table.columns[2].extra.as_deref(), Some("generated virtual"));
        assert_eq!(table.columns[3].name, "derived_stored");
        assert_eq!(table.columns[3].extra.as_deref(), Some("generated stored"));
    }

    #[test]
    fn classifies_only_known_sqlite_catalog_objects() {
        assert_eq!(
            sqlite_object_kind("table", Some("CREATE TABLE t(id)")),
            "table"
        );
        assert_eq!(
            sqlite_object_kind("table", Some("create\nvirtual\ttable t USING fts5(id)")),
            "unknown"
        );
        assert_eq!(sqlite_object_kind("table", None), "unknown");
        assert_eq!(sqlite_object_kind("table", Some("unrecognized")), "unknown");
        assert_eq!(
            sqlite_object_kind("view", Some("CREATE VIEW v AS SELECT 1")),
            "view"
        );
        assert_eq!(
            sqlite_object_kind("index", Some("CREATE INDEX i ON t(id)")),
            "unknown"
        );
    }
}
