use crate::database::DatabaseDriver;
use crate::models::*;
use async_trait::async_trait;
use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

/// 单次查询最多返回的行数（与其他驱动一致）
const MAX_ROWS: usize = 10_000;

/// SQLite 驱动：本地文件库，单连接串行执行。
/// rusqlite 是同步 API，SQLite 本地操作通常很快，直接在 tokio 线程上执行；
/// 长查询可通过 interrupt_handle 中断。
pub struct SqliteDriver {
    conn: Mutex<Connection>,
    connected: AtomicBool,
}

impl SqliteDriver {
    pub async fn connect(config: &ConnectionConfig) -> Result<Self, DatabaseError> {
        let path = config
            .database
            .as_deref()
            .filter(|p| !p.is_empty())
            .ok_or_else(|| {
                DatabaseError::ConnectionError("SQLite 连接需要指定数据库文件路径".into())
            })?;

        let conn = open_conn(path)?;
        // 验证可用
        conn.query_row("SELECT 1", [], |_| Ok(()))
            .map_err(|e| DatabaseError::ConnectionError(e.to_string()))?;

        Ok(Self {
            conn: Mutex::new(conn),
            connected: AtomicBool::new(true),
        })
    }

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
        .ok_or_else(|| {
            DatabaseError::ConnectionError("SQLite 连接需要指定数据库文件路径".into())
        })?;
    let conn = open_conn(path)?;
    conn.query_row("SELECT 1", [], |_| Ok(()))
        .map_err(|e| DatabaseError::ConnectionError(e.to_string()))?;
    Ok(())
}

/// rusqlite 值转 JSON
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

#[async_trait]
impl DatabaseDriver for SqliteDriver {
    fn driver_type(&self) -> &'static str {
        "sqlite"
    }

    fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    async fn query(
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
        let mut raw = stmt.query([]).map_err(map_err)?;
        while let Some(r) = raw.next().map_err(map_err)? {
            if rows.len() >= MAX_ROWS {
                truncated = true;
                break;
            }
            let values = (0..col_count)
                .map(|i| r.get_ref(i).ok().map(sqlite_value_to_json).flatten())
                .collect();
            rows.push(RowData { values });
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

    async fn execute(
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

    async fn cancel_query(&self, _query_id: u64) -> Result<(), DatabaseError> {
        let conn = self.lock()?;
        conn.get_interrupt_handle().interrupt();
        Ok(())
    }

    async fn get_databases(&self) -> Result<Vec<String>, DatabaseError> {
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

    async fn get_tables(&self, _database: Option<&str>) -> Result<Vec<TableInfo>, DatabaseError> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(
                "SELECT name FROM sqlite_master \
                 WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
            )
            .map_err(map_err)?;
        let names: Vec<String> = stmt
            .query_map([], |r| r.get(0))
            .map_err(map_err)?
            .filter_map(|r| r.ok())
            .collect();

        let mut tables = Vec::new();
        for name in names {
            // 行数用 COUNT(*)（SQLite 单文件，小库开销可接受；失败则不给值）
            let row_count = conn
                .query_row(
                    &format!("SELECT COUNT(*) FROM \"{}\"", name.replace('"', "\"\"")),
                    [],
                    |r| r.get::<_, i64>(0),
                )
                .ok();
            tables.push(TableInfo {
                name,
                schema: Some("main".to_string()),
                row_count,
                engine: Some("SQLite".to_string()),
                comment: None,
            });
        }
        Ok(tables)
    }

    async fn get_views(&self, _database: Option<&str>) -> Result<Vec<TableInfo>, DatabaseError> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'view' ORDER BY name")
            .map_err(map_err)?;
        let views = stmt
            .query_map([], |r| {
                let name: String = r.get(0)?;
                Ok(TableInfo {
                    name,
                    schema: Some("main".to_string()),
                    row_count: None,
                    engine: None,
                    comment: None,
                })
            })
            .map_err(map_err)?
            .filter_map(|r| r.ok())
            .collect();
        Ok(views)
    }

    async fn get_view_definition(
        &self,
        _database: Option<&str>,
        view: &str,
    ) -> Result<String, DatabaseError> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare("SELECT sql FROM sqlite_master WHERE type = 'view' AND name = ?1")
            .map_err(map_err)?;
        let mut rows = stmt.query(rusqlite::params![view]).map_err(map_err)?;
        match rows.next().map_err(map_err)? {
            Some(row) => row.get::<_, String>(0).map_err(map_err),
            None => Err(DatabaseError::Unknown(format!("视图 {} 不存在", view))),
        }
    }

    async fn get_triggers(&self, _database: Option<&str>) -> Result<Vec<TableInfo>, DatabaseError> {
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

    async fn get_trigger_definition(
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

    async fn get_procedures(
        &self,
        _database: Option<&str>,
    ) -> Result<Vec<TableInfo>, DatabaseError> {
        // SQLite 不支持存储过程
        Ok(Vec::new())
    }

    async fn get_procedure_definition(
        &self,
        _database: Option<&str>,
        procedure: &str,
    ) -> Result<String, DatabaseError> {
        Err(DatabaseError::Unknown(format!(
            "SQLite 不支持存储过程（{}）",
            procedure
        )))
    }

    async fn get_functions(
        &self,
        _database: Option<&str>,
    ) -> Result<Vec<TableInfo>, DatabaseError> {
        // SQLite 不支持自定义存储函数（内置函数不在此处列出）
        Ok(Vec::new())
    }

    async fn get_function_definition(
        &self,
        _database: Option<&str>,
        function: &str,
    ) -> Result<String, DatabaseError> {
        Err(DatabaseError::Unknown(format!(
            "SQLite 不支持存储函数（{}）",
            function
        )))
    }

    async fn get_users(&self, _database: Option<&str>) -> Result<Vec<DbUserInfo>, DatabaseError> {
        // SQLite 无账号体系
        Ok(Vec::new())
    }

    async fn get_current_user(
        &self,
        _database: Option<&str>,
    ) -> Result<CurrentUserInfo, DatabaseError> {
        Ok(CurrentUserInfo {
            user: "local".to_string(),
            grants: vec!["对数据库文件拥有全部权限（SQLite 无账号体系）".to_string()],
        })
    }

    async fn resolve_table(
        &self,
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
        let conn = self.lock()?;
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
                "SELECT name FROM \"{}\".sqlite_master WHERE type IN ('table', 'view') AND name = ?1 COLLATE NOCASE",
                self.escape_identifier(&schema),
            );
            let mut identity_stmt = conn.prepare(&sql).map_err(map_err)?;
            let mut rows = identity_stmt.query([requested_table]).map_err(map_err)?;
            if let Some(row) = rows.next().map_err(map_err)? {
                let table: String = row.get(0).map_err(map_err)?;
                let columns = Self::columns_for_schema(&conn, Some(&schema), &table)?;
                if columns.is_empty() {
                    return Err(DatabaseError::QueryError("目标表列信息不可用".into()));
                }
                return Ok(ResolvedTable {
                    database: database.to_string(),
                    schema: Some(schema),
                    table,
                    columns,
                });
            }
        }
        Err(DatabaseError::QueryError(
            "未找到目标表或指定的 SQLite schema".into(),
        ))
    }

    async fn get_columns(
        &self,
        _database: Option<&str>,
        table: &str,
    ) -> Result<Vec<ColumnInfo>, DatabaseError> {
        let conn = self.lock()?;
        Self::columns_for_schema(&conn, None, table)
    }

    async fn get_indexes(
        &self,
        _database: Option<&str>,
        table: &str,
    ) -> Result<Vec<IndexInfo>, DatabaseError> {
        let conn = self.lock()?;
        let esc = table.replace('"', "\"\"");
        let mut stmt = conn
            .prepare(&format!("PRAGMA index_list(\"{}\")", esc))
            .map_err(map_err)?;
        let idx_list: Vec<(String, bool, String)> = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)? == 1,
                    r.get::<_, String>(3).unwrap_or_default(),
                ))
            })
            .map_err(map_err)?
            .filter_map(|r| r.ok())
            .collect();

        let mut indexes = Vec::new();
        for (name, unique, origin) in idx_list {
            let mut col_stmt = conn
                .prepare(&format!(
                    "PRAGMA index_info(\"{}\")",
                    name.replace('"', "\"\"")
                ))
                .map_err(map_err)?;
            let columns: Vec<String> = col_stmt
                .query_map([], |r| r.get::<_, String>(2))
                .map_err(map_err)?
                .filter_map(|r| r.ok())
                .collect();
            indexes.push(IndexInfo {
                is_primary: origin == "pk",
                is_unique: unique,
                name,
                columns,
            });
        }
        Ok(indexes)
    }

    async fn get_foreign_keys(
        &self,
        _database: Option<&str>,
        table: &str,
    ) -> Result<Vec<ForeignKeyInfo>, DatabaseError> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(&format!(
                "PRAGMA foreign_key_list(\"{}\")",
                table.replace('"', "\"\"")
            ))
            .map_err(map_err)?;
        // PRAGMA 列: id, seq, table, from, to, on_update, on_delete, match
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4).unwrap_or_default(),
                    r.get::<_, String>(5).unwrap_or_default(),
                    r.get::<_, String>(6).unwrap_or_default(),
                ))
            })
            .map_err(map_err)?
            .filter_map(|r| r.ok())
            .collect::<Vec<_>>();

        // 按 id 分组成复合外键
        let mut order: Vec<i64> = Vec::new();
        let mut map: std::collections::HashMap<i64, ForeignKeyInfo> =
            std::collections::HashMap::new();
        for (id, ref_table, from, to, on_update, on_delete) in rows {
            let entry = map.entry(id).or_insert_with(|| {
                order.push(id);
                ForeignKeyInfo {
                    name: format!("fk_{}", id),
                    columns: Vec::new(),
                    ref_table,
                    ref_schema: Some("main".to_string()),
                    ref_columns: Vec::new(),
                    on_update: Some(on_update),
                    on_delete: Some(on_delete),
                }
            });
            entry.columns.push(from);
            if !to.is_empty() {
                entry.ref_columns.push(to);
            }
        }
        Ok(order.into_iter().filter_map(|i| map.remove(&i)).collect())
    }

    async fn execute_batch(
        &self,
        sqls: Vec<String>,
        _database: Option<&str>,
    ) -> Result<u64, DatabaseError> {
        let mut conn = self.lock()?;
        let tx = conn
            .transaction()
            .map_err(|e| DatabaseError::ExecuteError(e.to_string()))?;
        let mut total: u64 = 0;
        for sql in &sqls {
            match tx.execute(sql.as_str(), []) {
                Ok(n) => total += n as u64,
                Err(e) => {
                    return Err(DatabaseError::ExecuteError(format!("{}（已回滚）", e)));
                }
            }
        }
        tx.commit()
            .map_err(|e| DatabaseError::ExecuteError(e.to_string()))?;
        Ok(total)
    }

    async fn export_table_csv(
        &self,
        _database: Option<&str>,
        table: &str,
        path: &str,
    ) -> Result<u64, DatabaseError> {
        let conn = self.lock()?;
        let sql = format!("SELECT * FROM \"{}\"", self.escape_identifier(table));
        let mut stmt = conn.prepare(&sql).map_err(map_err)?;
        let columns: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
        let col_count = columns.len();

        let mut data: Vec<Vec<Option<serde_json::Value>>> = Vec::new();
        let mut raw = stmt.query([]).map_err(map_err)?;
        while let Some(r) = raw.next().map_err(map_err)? {
            data.push(
                (0..col_count)
                    .map(|i| r.get_ref(i).ok().map(sqlite_value_to_json).flatten())
                    .collect(),
            );
        }
        crate::database::write_csv(path, &columns, data)
    }

    async fn dump_table(
        &self,
        _database: Option<&str>,
        table: &str,
        include_drop: bool,
        include_data: bool,
    ) -> Result<(String, u64), DatabaseError> {
        let conn = self.lock()?;
        let esc = self.escape_identifier(table);

        let mut out = String::new();
        out.push_str(&format!("-- ------------------------------------------------------\n-- 表 \"{}\"\n-- ------------------------------------------------------\n\n", table));

        if include_drop {
            out.push_str(&format!("DROP TABLE IF EXISTS \"{}\";\n\n", esc));
        }

        // SQLite 直接存了原始建表语句
        let create_sql: Option<String> = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?1",
                [table],
                |r| r.get(0),
            )
            .ok()
            .flatten();
        let create_sql =
            create_sql.ok_or_else(|| DatabaseError::Unknown(format!("表 {} 不存在", table)))?;
        out.push_str(&create_sql);
        if !create_sql.trim_end().ends_with(';') {
            out.push(';');
        }
        out.push_str("\n\n");

        let mut rows_written = 0u64;
        if include_data {
            let sql = format!("SELECT * FROM \"{}\"", esc);
            let mut stmt = conn.prepare(&sql).map_err(map_err)?;
            let columns: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
            let col_count = columns.len();
            let mut data: Vec<Vec<Option<serde_json::Value>>> = Vec::new();
            let mut raw = stmt.query([]).map_err(map_err)?;
            while let Some(r) = raw.next().map_err(map_err)? {
                data.push(
                    (0..col_count)
                        .map(|i| r.get_ref(i).ok().map(sqlite_value_to_json).flatten())
                        .collect(),
                );
            }
            let quoted_cols = columns
                .iter()
                .map(|c| format!("\"{}\"", self.escape_identifier(c)))
                .collect::<Vec<_>>()
                .join(", ");
            rows_written = crate::database::dump::write_insert_batches(
                &mut out,
                &format!("\"{}\"", esc),
                &quoted_cols,
                &data,
                false,
            );
        }

        Ok((out, rows_written))
    }

    async fn disconnect(&self) -> Result<(), DatabaseError> {
        self.connected.store(false, Ordering::Relaxed);
        Ok(())
    }

    fn escape_identifier(&self, identifier: &str) -> String {
        identifier.replace('"', "\"\"")
    }
}

impl SqliteDriver {
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
                "PRAGMA {}table_info(\"{}\")",
                schema_prefix,
                table.replace('"', "\"\"")
            ))
            .map_err(map_err)?;
        let cols = stmt
            .query_map([], |r| {
                Ok(ColumnInfo {
                    name: r.get(1)?,
                    data_type: r.get::<_, String>(2).unwrap_or_default(),
                    nullable: r.get::<_, i64>(3)? == 0,
                    is_primary_key: r.get::<_, i64>(5)? > 0,
                    default_value: r.get(4).ok(),
                    extra: None,
                    comment: None,
                })
            })
            .map_err(map_err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(map_err)?;
        Ok(cols)
    }
}

#[cfg(test)]
mod resolve_tests {
    use super::*;

    fn driver(sql: &str) -> SqliteDriver {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(sql).unwrap();
        SqliteDriver {
            conn: Mutex::new(conn),
            connected: AtomicBool::new(true),
        }
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
            .lock()
            .unwrap()
            .execute_batch("DROP TABLE temp.shared")
            .unwrap();
        assert_eq!(
            resolve(&driver, &["shared"]).await.schema.as_deref(),
            Some("main")
        );
        driver
            .lock()
            .unwrap()
            .execute_batch("DROP TABLE main.shared")
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
}
