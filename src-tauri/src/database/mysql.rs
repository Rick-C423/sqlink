use crate::database::DatabaseDriver;
use crate::models::*;
use async_trait::async_trait;
use mysql_async::{prelude::Queryable, Opts, OptsBuilder, Pool};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::timeout;

/// 单次查询最多返回的行数（超出部分丢弃并标记 truncated，防止大表拖垮前后端）
const MAX_ROWS: usize = 10_000;

fn build_opts(config: &ConnectionConfig) -> Opts {
    let mut builder = OptsBuilder::default()
        .ip_or_hostname(config.host.clone())
        .tcp_port(config.port)
        .user(Some(config.user.clone()))
        // 服务端可能强制 latin1（skip-character-set-client-handshake），
        // 用 setup 而非 init：连接池默认 reset_connection=true，
        // 还回池时会重置会话变量，setup 在每次重置后都会重跑
        .init(vec!["SET NAMES utf8mb4"])
        .setup(vec!["SET NAMES utf8mb4"]);

    if !config.password.is_empty() {
        builder = builder.pass(Some(config.password.clone()));
    }

    if let Some(ref db) = config.database {
        if !db.is_empty() {
            builder = builder.db_name(Some(db.clone()));
        }
    }

    Opts::from(builder)
}

pub struct MySqlDriver {
    pool: Pool,
    connected: AtomicBool,
    /// query_id -> 正在执行该查询的 MySQL 连接线程 id（用于 KILL QUERY）
    active_queries: Arc<Mutex<HashMap<u64, u32>>>,
}

impl MySqlDriver {
    pub async fn connect(config: &ConnectionConfig) -> Result<Self, DatabaseError> {
        let pool = Pool::new(build_opts(config));

        // 立即取一条连接验证可用性，失败则报错
        let conn = pool
            .get_conn()
            .await
            .map_err(|e| DatabaseError::ConnectionError(e.to_string()))?;
        drop(conn);

        Ok(Self {
            pool,
            connected: AtomicBool::new(true),
            active_queries: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    /// information_schema.ROUTINES 列表（routine_type 为 'PROCEDURE' 或 'FUNCTION'）
    async fn get_routines(
        &self,
        database: Option<&str>,
        routine_type: &str,
    ) -> Result<Vec<TableInfo>, DatabaseError> {
        let sql = match database {
            Some(db) => format!(
                "SELECT ROUTINE_NAME, ROUTINE_COMMENT FROM information_schema.ROUTINES \
                 WHERE ROUTINE_SCHEMA = '{}' AND ROUTINE_TYPE = '{}' ORDER BY ROUTINE_NAME",
                db.replace('\\', "\\\\").replace('\'', "\\'"),
                routine_type
            ),
            None => format!(
                "SELECT ROUTINE_NAME, ROUTINE_COMMENT FROM information_schema.ROUTINES \
                 WHERE ROUTINE_SCHEMA = DATABASE() AND ROUTINE_TYPE = '{}' ORDER BY ROUTINE_NAME",
                routine_type
            ),
        };
        let result = self.query(&sql, None, None).await?;
        Ok(result
            .rows
            .iter()
            .filter_map(|row| {
                let name = row
                    .values
                    .first()
                    .and_then(|v| v.as_ref())
                    .and_then(|v| v.as_str())?;
                let comment = row
                    .values
                    .get(1)
                    .and_then(|v| v.as_ref())
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string());
                Some(TableInfo {
                    name: name.to_string(),
                    schema: database.map(|d| d.to_string()),
                    row_count: None,
                    engine: None,
                    comment,
                })
            })
            .collect())
    }

    /// SHOW CREATE PROCEDURE / FUNCTION 取定义（第 3 列是创建语句）
    async fn get_routine_definition(
        &self,
        database: Option<&str>,
        routine_type: &str,
        name: &str,
    ) -> Result<String, DatabaseError> {
        let sql = match database {
            Some(db) => format!(
                "SHOW CREATE {} `{}`.`{}`",
                routine_type,
                self.escape_identifier(db),
                self.escape_identifier(name)
            ),
            None => format!(
                "SHOW CREATE {} `{}`",
                routine_type,
                self.escape_identifier(name)
            ),
        };
        let result = self.query(&sql, None, None).await?;
        // SHOW CREATE PROCEDURE 返回: Procedure | sql_mode | Create Procedure | ...
        result
            .rows
            .first()
            .and_then(|r| r.values.get(2))
            .and_then(|v| v.as_ref())
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| DatabaseError::Unknown(format!("{} {} 定义为空", routine_type, name)))
    }
}

pub async fn test_connection(config: &ConnectionConfig) -> Result<(), DatabaseError> {
    let mut conn = mysql_async::Conn::new(build_opts(config))
        .await
        .map_err(|e| DatabaseError::ConnectionError(e.to_string()))?;

    conn.query_drop("SELECT 1")
        .await
        .map_err(|e| DatabaseError::ConnectionError(e.to_string()))?;

    conn.disconnect()
        .await
        .map_err(|e| DatabaseError::ConnectionError(e.to_string()))?;

    Ok(())
}

#[async_trait]
impl DatabaseDriver for MySqlDriver {
    fn driver_type(&self) -> &'static str {
        "mysql"
    }

    fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    async fn query(
        &self,
        sql: &str,
        database: Option<&str>,
        query_id: Option<u64>,
    ) -> Result<QueryResult, DatabaseError> {
        let start = std::time::Instant::now();

        let mut conn = self
            .pool
            .get_conn()
            .await
            .map_err(|e| DatabaseError::ConnectionError(e.to_string()))?;

        // 切换到查询上下文数据库（池化连接的上次 schema 不可信，每次显式 USE）
        if let Some(db) = database {
            conn.query_drop(format!("USE `{}`", self.escape_identifier(db)))
                .await
                .map_err(map_query_error)?;
        }

        if let Some(qid) = query_id {
            self.active_queries.lock().await.insert(qid, conn.id());
        }

        let result = conn.query_iter(sql).await;
        let result = match result {
            Ok(r) => r,
            Err(e) => {
                if let Some(qid) = query_id {
                    self.active_queries.lock().await.remove(&qid);
                }
                return Err(map_query_error(e));
            }
        };

        let columns: Vec<ColumnMetadata> = result
            .columns_ref()
            .iter()
            .map(|col| ColumnMetadata {
                name: col.name_str().to_string(),
                data_type: format!("{:?}", col.column_type()),
                unified_type: "Unknown".to_string(),
                nullable: !col
                    .flags()
                    .contains(mysql_async::consts::ColumnFlags::NOT_NULL_FLAG),
                length: Some(col.column_length() as usize),
                precision: None,
                scale: None,
            })
            .collect();
        let kinds: Vec<ColKind> = result.columns_ref().iter().map(col_kind).collect();

        let result_set: Vec<mysql_async::Row> =
            result.collect_and_drop().await.map_err(map_query_error)?;

        if let Some(qid) = query_id {
            self.active_queries.lock().await.remove(&qid);
        }

        let mut rows = Vec::new();
        let mut truncated = false;
        for mysql_row in result_set {
            if rows.len() >= MAX_ROWS {
                truncated = true;
                continue; // 行已取回内存，丢弃超出的部分
            }
            let mut values = Vec::new();
            for idx in 0..columns.len() {
                let val: Option<mysql_async::Value> = mysql_row.get(idx);
                values.push(Some(value_to_json(val, kinds[idx])));
            }
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
        database: Option<&str>,
        query_id: Option<u64>,
    ) -> Result<QueryResult, DatabaseError> {
        let start = std::time::Instant::now();

        let mut conn = self
            .pool
            .get_conn()
            .await
            .map_err(|e| DatabaseError::ConnectionError(e.to_string()))?;

        if let Some(db) = database {
            conn.query_drop(format!("USE `{}`", self.escape_identifier(db)))
                .await
                .map_err(map_execute_error)?;
        }

        if let Some(qid) = query_id {
            self.active_queries.lock().await.insert(qid, conn.id());
        }

        let result = conn.query_iter(sql).await;
        let result = match result {
            Ok(r) => r,
            Err(e) => {
                if let Some(qid) = query_id {
                    self.active_queries.lock().await.remove(&qid);
                }
                return Err(map_execute_error(e));
            }
        };

        let affected = result.affected_rows();
        let last_id = result.last_insert_id();

        let collect_result: Result<Vec<mysql_async::Row>, _> = result.collect_and_drop().await;
        if let Some(qid) = query_id {
            self.active_queries.lock().await.remove(&qid);
        }
        collect_result.map_err(map_execute_error)?;

        Ok(QueryResult {
            columns: vec![],
            rows: vec![],
            total_count: Some(0),
            execution_time_ms: start.elapsed().as_millis() as u64,
            rows_affected: Some(affected),
            last_insert_id: last_id,
            truncated: false,
        })
    }

    async fn cancel_query(&self, query_id: u64) -> Result<(), DatabaseError> {
        let conn_id = self.active_queries.lock().await.get(&query_id).copied();
        let conn_id = conn_id.ok_or(DatabaseError::Unknown("查询不存在或已结束".to_string()))?;

        let mut killer = self
            .pool
            .get_conn()
            .await
            .map_err(|e| DatabaseError::ConnectionError(e.to_string()))?;
        killer
            .query_drop(format!("KILL QUERY {}", conn_id))
            .await
            .map_err(|e| DatabaseError::QueryError(e.to_string()))?;

        Ok(())
    }

    async fn get_databases(&self) -> Result<Vec<String>, DatabaseError> {
        let result = self.query("SHOW DATABASES", None, None).await?;
        let dbs = result
            .rows
            .iter()
            .filter_map(|row| {
                row.values
                    .first()
                    .and_then(|v| v.as_ref())
                    .and_then(|v| v.as_str().map(|s| s.to_string()))
            })
            .collect();
        Ok(dbs)
    }

    async fn get_tables(&self, database: Option<&str>) -> Result<Vec<TableInfo>, DatabaseError> {
        // information_schema 一次拿全：表名、引擎、估算行数、注释
        let sql = match database {
            Some(db) => format!(
                "SELECT TABLE_NAME, ENGINE, TABLE_ROWS, TABLE_COMMENT \
                 FROM information_schema.TABLES \
                 WHERE TABLE_SCHEMA = '{}' AND TABLE_TYPE = 'BASE TABLE' ORDER BY TABLE_NAME",
                db.replace('\\', "\\\\").replace('\'', "\\'")
            ),
            None => "SELECT TABLE_NAME, ENGINE, TABLE_ROWS, TABLE_COMMENT \
                     FROM information_schema.TABLES \
                     WHERE TABLE_SCHEMA = DATABASE() AND TABLE_TYPE = 'BASE TABLE' ORDER BY TABLE_NAME"
                .to_string(),
        };

        let result = self.query(&sql, None, None).await?;
        let tables = result
            .rows
            .iter()
            .filter_map(|row| {
                let name = row
                    .values
                    .first()
                    .and_then(|v| v.as_ref())
                    .and_then(|v| v.as_str())?;
                let engine = row
                    .values
                    .get(1)
                    .and_then(|v| v.as_ref())
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let row_count = row
                    .values
                    .get(2)
                    .and_then(|v| v.as_ref())
                    .and_then(|v| v.as_i64());
                let comment = row
                    .values
                    .get(3)
                    .and_then(|v| v.as_ref())
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string());
                Some(TableInfo {
                    name: name.to_string(),
                    schema: database.map(|d| d.to_string()),
                    row_count,
                    engine,
                    comment,
                })
            })
            .collect();
        Ok(tables)
    }

    async fn get_views(&self, database: Option<&str>) -> Result<Vec<TableInfo>, DatabaseError> {
        let sql = match database {
            Some(db) => format!(
                "SELECT TABLE_NAME FROM information_schema.TABLES \
                 WHERE TABLE_SCHEMA = '{}' AND TABLE_TYPE = 'VIEW' ORDER BY TABLE_NAME",
                db.replace('\\', "\\\\").replace('\'', "\\'")
            ),
            None => "SELECT TABLE_NAME FROM information_schema.TABLES \
                     WHERE TABLE_SCHEMA = DATABASE() AND TABLE_TYPE = 'VIEW' ORDER BY TABLE_NAME"
                .to_string(),
        };
        let result = self.query(&sql, None, None).await?;
        Ok(result
            .rows
            .iter()
            .filter_map(|row| {
                let name = row
                    .values
                    .first()
                    .and_then(|v| v.as_ref())
                    .and_then(|v| v.as_str())?;
                Some(TableInfo {
                    name: name.to_string(),
                    schema: database.map(|d| d.to_string()),
                    row_count: None,
                    engine: None,
                    comment: None,
                })
            })
            .collect())
    }

    async fn get_view_definition(
        &self,
        database: Option<&str>,
        view: &str,
    ) -> Result<String, DatabaseError> {
        let sql = match database {
            Some(db) => format!(
                "SHOW CREATE VIEW `{}`.`{}`",
                self.escape_identifier(db),
                self.escape_identifier(view)
            ),
            None => format!("SHOW CREATE VIEW `{}`", self.escape_identifier(view)),
        };
        let result = self.query(&sql, None, None).await?;
        // SHOW CREATE VIEW 返回: View | Create View | character_set_client | collation_connection
        result
            .rows
            .first()
            .and_then(|r| r.values.get(1))
            .and_then(|v| v.as_ref())
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| DatabaseError::Unknown(format!("视图 {} 定义为空", view)))
    }

    async fn get_triggers(&self, database: Option<&str>) -> Result<Vec<TableInfo>, DatabaseError> {
        let sql = match database {
            Some(db) => format!(
                "SELECT TRIGGER_NAME, EVENT_OBJECT_TABLE, ACTION_TIMING, EVENT_MANIPULATION \
                 FROM information_schema.TRIGGERS \
                 WHERE TRIGGER_SCHEMA = '{}' ORDER BY TRIGGER_NAME",
                db.replace('\\', "\\\\").replace('\'', "\\'")
            ),
            None => "SELECT TRIGGER_NAME, EVENT_OBJECT_TABLE, ACTION_TIMING, EVENT_MANIPULATION \
                     FROM information_schema.TRIGGERS \
                     WHERE TRIGGER_SCHEMA = DATABASE() ORDER BY TRIGGER_NAME"
                .to_string(),
        };
        let result = self.query(&sql, None, None).await?;
        Ok(result
            .rows
            .iter()
            .filter_map(|row| {
                let name = row
                    .values
                    .first()
                    .and_then(|v| v.as_ref())
                    .and_then(|v| v.as_str())?;
                let table = row
                    .values
                    .get(1)
                    .and_then(|v| v.as_ref())
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let timing = row
                    .values
                    .get(2)
                    .and_then(|v| v.as_ref())
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let event = row
                    .values
                    .get(3)
                    .and_then(|v| v.as_ref())
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                Some(TableInfo {
                    name: name.to_string(),
                    schema: database.map(|d| d.to_string()),
                    row_count: None,
                    engine: None,
                    comment: Some(format!("{} {} ON {}", timing, event, table)),
                })
            })
            .collect())
    }

    async fn get_trigger_definition(
        &self,
        database: Option<&str>,
        trigger: &str,
    ) -> Result<String, DatabaseError> {
        let sql = match database {
            Some(db) => format!(
                "SHOW CREATE TRIGGER `{}`.`{}`",
                self.escape_identifier(db),
                self.escape_identifier(trigger)
            ),
            None => format!("SHOW CREATE TRIGGER `{}`", self.escape_identifier(trigger)),
        };
        let result = self.query(&sql, None, None).await?;
        // SHOW CREATE TRIGGER 返回: Trigger | sql_mode | SQL Original Statement | ...
        result
            .rows
            .first()
            .and_then(|r| r.values.get(2))
            .and_then(|v| v.as_ref())
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| DatabaseError::Unknown(format!("触发器 {} 定义为空", trigger)))
    }

    async fn get_procedures(
        &self,
        database: Option<&str>,
    ) -> Result<Vec<TableInfo>, DatabaseError> {
        self.get_routines(database, "PROCEDURE").await
    }

    async fn get_functions(&self, database: Option<&str>) -> Result<Vec<TableInfo>, DatabaseError> {
        self.get_routines(database, "FUNCTION").await
    }

    async fn get_procedure_definition(
        &self,
        database: Option<&str>,
        procedure: &str,
    ) -> Result<String, DatabaseError> {
        self.get_routine_definition(database, "PROCEDURE", procedure)
            .await
    }

    async fn get_function_definition(
        &self,
        database: Option<&str>,
        function: &str,
    ) -> Result<String, DatabaseError> {
        self.get_routine_definition(database, "FUNCTION", function)
            .await
    }

    async fn get_users(&self, _database: Option<&str>) -> Result<Vec<DbUserInfo>, DatabaseError> {
        let result = self
            .query(
                "SELECT User, Host FROM mysql.user ORDER BY User, Host",
                None,
                None,
            )
            .await?;
        Ok(result
            .rows
            .iter()
            .filter_map(|row| {
                let name = row.values.first()?.as_ref()?.as_str()?.to_string();
                let host = row
                    .values
                    .get(1)
                    .and_then(|v| v.as_ref())
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                Some(DbUserInfo {
                    name,
                    host,
                    note: None,
                })
            })
            .collect())
    }

    async fn get_current_user(
        &self,
        _database: Option<&str>,
    ) -> Result<CurrentUserInfo, DatabaseError> {
        let user_result = self.query("SELECT CURRENT_USER()", None, None).await?;
        let user = user_result
            .rows
            .first()
            .and_then(|r| r.values.first())
            .and_then(|v| v.as_ref())
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();
        let grants_result = self.query("SHOW GRANTS", None, None).await?;
        let grants = grants_result
            .rows
            .iter()
            .filter_map(|r| {
                r.values
                    .first()
                    .and_then(|v| v.as_ref())
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            })
            .collect();
        Ok(CurrentUserInfo { user, grants })
    }

    async fn resolve_table(
        &self,
        database: &str,
        table_parts: &[String],
    ) -> Result<ResolvedTable, DatabaseError> {
        let (target_database, table) = match table_parts {
            [table] => (database, table.as_str()),
            [database, table] => (database.as_str(), table.as_str()),
            _ => {
                return Err(DatabaseError::QueryError(
                    "MySQL 表名需要一段或两段标识符".into(),
                ))
            }
        };
        if target_database.is_empty() || table.is_empty() {
            return Err(DatabaseError::QueryError("数据库名和表名不能为空".into()));
        }
        let mut conn = self
            .pool
            .get_conn()
            .await
            .map_err(|e| DatabaseError::ConnectionError(e.to_string()))?;
        let identities: Vec<(String, String)> = conn.exec(
            "SELECT TABLE_SCHEMA, TABLE_NAME FROM information_schema.TABLES \
             WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? \
             AND BINARY LOWER(TABLE_SCHEMA) = BINARY LOWER(?) \
             AND BINARY LOWER(TABLE_NAME) = BINARY LOWER(?) \
             AND (@@lower_case_table_names <> 0 OR (BINARY TABLE_SCHEMA = BINARY ? AND BINARY TABLE_NAME = BINARY ?)) \
             LIMIT 2",
            (target_database, table, target_database, table, target_database, table),
        ).await.map_err(map_query_error)?;
        drop(conn);
        if identities.len() != 1 {
            return Err(DatabaseError::QueryError("未找到唯一的目标表".into()));
        }
        let (database, table) = identities.into_iter().next().unwrap();
        let columns = self.get_columns(Some(&database), &table).await?;
        if columns.is_empty() {
            return Err(DatabaseError::QueryError("目标表列信息不可用".into()));
        }
        Ok(ResolvedTable {
            database,
            schema: None,
            table,
            columns,
        })
    }

    async fn get_columns(
        &self,
        database: Option<&str>,
        table: &str,
    ) -> Result<Vec<ColumnInfo>, DatabaseError> {
        // 不使用 SHOW FULL COLUMNS：它在部分 MySQL 版本上会等待目标表的元数据锁，
        // 导致结构页一直转圈。information_schema.COLUMNS 能提供这里需要的全部字段，
        // 并且明确限定库和表，避免扫描/返回其他对象的元数据。
        let sql =
            "SELECT COLUMN_NAME, COLUMN_TYPE, IS_NULLABLE, COLUMN_KEY, COLUMN_DEFAULT, EXTRA, COLUMN_COMMENT \
             FROM information_schema.COLUMNS \
             WHERE TABLE_SCHEMA = COALESCE(NULLIF(?, ''), DATABASE()) AND TABLE_NAME = ? \
             AND BINARY LOWER(TABLE_SCHEMA) = BINARY LOWER(COALESCE(NULLIF(?, ''), DATABASE())) \
             AND BINARY LOWER(TABLE_NAME) = BINARY LOWER(?) \
             AND (@@lower_case_table_names <> 0 OR (BINARY TABLE_SCHEMA = BINARY COALESCE(NULLIF(?, ''), DATABASE()) AND BINARY TABLE_NAME = BINARY ?)) \
             ORDER BY ORDINAL_POSITION";
        let result_rows = timeout(Duration::from_secs(15), async {
            let mut conn = self.pool.get_conn().await.map_err(map_query_error)?;
            let rows: Vec<mysql_async::Row> = conn
                .exec(
                    sql,
                    (
                        database.unwrap_or(""),
                        table,
                        database.unwrap_or(""),
                        table,
                        database.unwrap_or(""),
                        table,
                    ),
                )
                .await
                .map_err(map_query_error)?;
            Ok::<_, DatabaseError>(
                rows.into_iter()
                    .map(|row| RowData {
                        values: (0..row.len())
                            .map(|i| Some(value_to_json(row.get(i), ColKind::Text)))
                            .collect(),
                    })
                    .collect::<Vec<_>>(),
            )
        })
        .await
        .map_err(|_| {
            DatabaseError::QueryError("读取列信息超时（可能存在未提交的表结构锁）".to_string())
        })??;

        let columns = result_rows
            .iter()
            .filter_map(|row| {
                if row.values.len() >= 7 {
                    let name = row
                        .values
                        .get(0)
                        .and_then(|v| v.as_ref())
                        .and_then(|v| v.as_str())?;
                    let data_type = row
                        .values
                        .get(1)
                        .and_then(|v| v.as_ref())
                        .and_then(|v| v.as_str())?;
                    let nullable = row
                        .values
                        .get(2)
                        .and_then(|v| v.as_ref())
                        .and_then(|v| v.as_str())
                        .map(|s| s == "YES")
                        .unwrap_or(true);
                    let key = row
                        .values
                        .get(3)
                        .and_then(|v| v.as_ref())
                        .and_then(|v| v.as_str());
                    let default = row
                        .values
                        .get(4)
                        .and_then(|v| v.as_ref())
                        .and_then(|v| v.as_str());
                    let extra = row
                        .values
                        .get(5)
                        .and_then(|v| v.as_ref())
                        .and_then(|v| v.as_str());
                    let comment = row
                        .values
                        .get(6)
                        .and_then(|v| v.as_ref())
                        .and_then(|v| v.as_str());

                    Some(ColumnInfo {
                        name: name.to_string(),
                        data_type: data_type.to_string(),
                        nullable,
                        is_primary_key: key.map(|k| k == "PRI").unwrap_or(false),
                        default_value: default.map(|s| s.to_string()),
                        extra: extra.map(|s| s.to_string()),
                        comment: comment.map(|s| s.to_string()),
                    })
                } else {
                    None
                }
            })
            .collect();

        Ok(columns)
    }

    async fn get_indexes(
        &self,
        database: Option<&str>,
        table: &str,
    ) -> Result<Vec<IndexInfo>, DatabaseError> {
        let sql = match database {
            Some(db) => format!(
                "SHOW INDEX FROM `{}` FROM `{}`",
                self.escape_identifier(table),
                self.escape_identifier(db)
            ),
            None => format!("SHOW INDEX FROM `{}`", self.escape_identifier(table)),
        };
        let result = self.query(&sql, None, None).await?;

        let mut index_map: HashMap<String, Vec<String>> = HashMap::new();
        let mut index_unique: HashMap<String, bool> = HashMap::new();

        for row in &result.rows {
            if row.values.len() >= 5 {
                let index_name = row
                    .values
                    .get(2)
                    .and_then(|v| v.as_ref())
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let column_name = row
                    .values
                    .get(4)
                    .and_then(|v| v.as_ref())
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let non_unique = row
                    .values
                    .get(1)
                    .and_then(|v| v.as_ref())
                    .and_then(|v| v.as_i64())
                    .map(|v| v != 0)
                    .unwrap_or(true);

                index_map
                    .entry(index_name.clone())
                    .or_default()
                    .push(column_name);
                index_unique.insert(index_name.clone(), !non_unique);
            }
        }

        let indexes = index_map
            .into_iter()
            .map(|(name, columns)| {
                let is_unique = *index_unique.get(&name).unwrap_or(&false);
                IndexInfo {
                    is_primary: name == "PRIMARY",
                    name,
                    columns,
                    is_unique,
                }
            })
            .collect();

        Ok(indexes)
    }

    async fn get_foreign_keys(
        &self,
        database: Option<&str>,
        table: &str,
    ) -> Result<Vec<ForeignKeyInfo>, DatabaseError> {
        let esc = |s: &str| s.replace('\\', "\\\\").replace('\'', "\\'");
        let schema_cond = match database {
            Some(db) => format!("k.TABLE_SCHEMA = '{}'", esc(db)),
            None => "k.TABLE_SCHEMA = DATABASE()".to_string(),
        };
        let sql = format!(
            "SELECT k.CONSTRAINT_NAME, k.COLUMN_NAME, k.REFERENCED_TABLE_NAME, \
                    k.REFERENCED_COLUMN_NAME, rc.UPDATE_RULE, rc.DELETE_RULE \
             FROM information_schema.KEY_COLUMN_USAGE k \
             JOIN information_schema.REFERENTIAL_CONSTRAINTS rc \
               ON rc.CONSTRAINT_NAME = k.CONSTRAINT_NAME \
              AND rc.CONSTRAINT_SCHEMA = k.CONSTRAINT_SCHEMA \
              AND rc.TABLE_NAME = k.TABLE_NAME \
             WHERE k.REFERENCED_TABLE_NAME IS NOT NULL \
               AND {} AND k.TABLE_NAME = '{}' \
             ORDER BY k.CONSTRAINT_NAME, k.ORDINAL_POSITION",
            schema_cond,
            esc(table)
        );
        let result = self.query(&sql, None, None).await?;

        // 按约束名分组（保持列顺序）
        let mut order: Vec<String> = Vec::new();
        let mut map: HashMap<String, ForeignKeyInfo> = HashMap::new();
        for row in &result.rows {
            let get_str = |i: usize| {
                row.values
                    .get(i)
                    .and_then(|v| v.as_ref())
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            };
            let name = match get_str(0) {
                Some(n) => n,
                None => continue,
            };
            let col = match get_str(1) {
                Some(c) => c,
                None => continue,
            };
            let entry = map.entry(name.clone()).or_insert_with(|| {
                order.push(name.clone());
                ForeignKeyInfo {
                    name: name.clone(),
                    columns: Vec::new(),
                    ref_table: get_str(2).unwrap_or_default(),
                    ref_schema: database.map(|d| d.to_string()),
                    ref_columns: Vec::new(),
                    on_update: get_str(4),
                    on_delete: get_str(5),
                }
            });
            entry.columns.push(col);
            if let Some(rc) = get_str(3) {
                entry.ref_columns.push(rc);
            }
        }
        Ok(order.into_iter().filter_map(|n| map.remove(&n)).collect())
    }

    async fn disconnect(&self) -> Result<(), DatabaseError> {
        self.connected.store(false, Ordering::Relaxed);
        let pool = self.pool.clone();
        pool.disconnect()
            .await
            .map_err(|e| DatabaseError::ConnectionError(e.to_string()))?;
        Ok(())
    }

    async fn export_table_csv(
        &self,
        database: Option<&str>,
        table: &str,
        path: &str,
    ) -> Result<u64, DatabaseError> {
        let mut conn = self
            .pool
            .get_conn()
            .await
            .map_err(|e| DatabaseError::ConnectionError(e.to_string()))?;

        if let Some(db) = database {
            conn.query_drop(format!("USE `{}`", self.escape_identifier(db)))
                .await
                .map_err(map_query_error)?;
        }

        let result = conn
            .query_iter(format!("SELECT * FROM `{}`", self.escape_identifier(table)))
            .await
            .map_err(map_query_error)?;

        let columns: Vec<String> = result
            .columns_ref()
            .iter()
            .map(|c| c.name_str().to_string())
            .collect();
        let kinds: Vec<ColKind> = result.columns_ref().iter().map(col_kind).collect();

        // 全量导出，不受 MAX_ROWS 限制
        let rows: Vec<mysql_async::Row> =
            result.collect_and_drop().await.map_err(map_query_error)?;
        let data: Vec<Vec<Option<serde_json::Value>>> = rows
            .iter()
            .map(|r| {
                (0..columns.len())
                    .map(|i| Some(value_to_json(r.get(i), kinds[i])))
                    .collect()
            })
            .collect();

        crate::database::write_csv(path, &columns, data)
    }

    async fn dump_table(
        &self,
        database: Option<&str>,
        table: &str,
        include_drop: bool,
        include_data: bool,
    ) -> Result<(String, u64), DatabaseError> {
        let mut conn = self
            .pool
            .get_conn()
            .await
            .map_err(|e| DatabaseError::ConnectionError(e.to_string()))?;

        if let Some(db) = database {
            conn.query_drop(format!("USE `{}`", self.escape_identifier(db)))
                .await
                .map_err(map_query_error)?;
        }

        let esc = self.escape_identifier(table);
        let mut out = String::new();
        out.push_str(&format!("-- ------------------------------------------------------\n-- 表 `{}`\n-- ------------------------------------------------------\n\n", table));

        if include_drop {
            out.push_str(&format!("DROP TABLE IF EXISTS `{}`;\n\n", esc));
        }

        // DDL：SHOW CREATE TABLE 直接拿服务器原始定义
        let row: Option<mysql_async::Row> = conn
            .query_first(format!("SHOW CREATE TABLE `{}`", esc))
            .await
            .map_err(map_query_error)?;
        let row = row.ok_or(DatabaseError::Unknown(format!("表 {} 不存在", table)))?;
        let ddl: Option<String> = row.get(1);
        let ddl = ddl.ok_or(DatabaseError::Unknown(format!(
            "无法获取表 {} 的定义",
            table
        )))?;
        out.push_str(&ddl);
        out.push_str(";\n\n");

        let mut rows_written = 0u64;
        if include_data {
            let result = conn
                .query_iter(format!("SELECT * FROM `{}`", esc))
                .await
                .map_err(map_query_error)?;
            let columns: Vec<String> = result
                .columns_ref()
                .iter()
                .map(|c| c.name_str().to_string())
                .collect();
            let kinds: Vec<ColKind> = result.columns_ref().iter().map(col_kind).collect();
            let rows: Vec<mysql_async::Row> =
                result.collect_and_drop().await.map_err(map_query_error)?;
            let data: Vec<Vec<Option<serde_json::Value>>> = rows
                .iter()
                .map(|r| {
                    (0..columns.len())
                        .map(|i| Some(value_to_json(r.get(i), kinds[i])))
                        .collect()
                })
                .collect();
            let quoted_cols = columns
                .iter()
                .map(|c| format!("`{}`", self.escape_identifier(c)))
                .collect::<Vec<_>>()
                .join(", ");
            rows_written = crate::database::dump::write_insert_batches(
                &mut out,
                &format!("`{}`", esc),
                &quoted_cols,
                &data,
                true,
            );
        }

        Ok((out, rows_written))
    }

    async fn execute_batch(
        &self,
        sqls: Vec<String>,
        database: Option<&str>,
    ) -> Result<u64, DatabaseError> {
        let mut conn = self
            .pool
            .get_conn()
            .await
            .map_err(|e| DatabaseError::ConnectionError(e.to_string()))?;

        if let Some(db) = database {
            conn.query_drop(format!("USE `{}`", self.escape_identifier(db)))
                .await
                .map_err(map_execute_error)?;
        }

        conn.query_drop("START TRANSACTION")
            .await
            .map_err(map_execute_error)?;

        let mut total: u64 = 0;
        for sql in &sqls {
            let result = conn.query_iter(sql).await;
            match result {
                Ok(r) => {
                    let affected = r.affected_rows();
                    if let Err(e) = r.collect_and_drop::<mysql_async::Row>().await {
                        let _ = conn.query_drop("ROLLBACK").await;
                        return Err(map_execute_error(e));
                    }
                    total += affected;
                }
                Err(e) => {
                    let _ = conn.query_drop("ROLLBACK").await;
                    return Err(map_execute_error(e));
                }
            }
        }

        conn.query_drop("COMMIT").await.map_err(map_execute_error)?;

        Ok(total)
    }

    fn escape_identifier(&self, identifier: &str) -> String {
        identifier.replace('`', "``")
    }
}

/// MySQL 列的内容类别：决定 Value::Bytes 如何解码
#[derive(Clone, Copy, PartialEq)]
enum ColKind {
    /// UTF-8 系文本（含非字符串列的字节值）
    Text,
    /// GBK/GB18030 字符集的文本列
    Gbk,
    /// 真二进制（binary 字符集、GEOMETRY、BIT）
    Binary,
}

/// 根据列类型与字符集（collation id）判断内容类别
fn col_kind(col: &mysql_async::Column) -> ColKind {
    use mysql_async::consts::ColumnType as T;
    match col.column_type() {
        T::MYSQL_TYPE_GEOMETRY | T::MYSQL_TYPE_BIT => ColKind::Binary,
        T::MYSQL_TYPE_STRING
        | T::MYSQL_TYPE_VAR_STRING
        | T::MYSQL_TYPE_VARCHAR
        | T::MYSQL_TYPE_BLOB
        | T::MYSQL_TYPE_TINY_BLOB
        | T::MYSQL_TYPE_MEDIUM_BLOB
        | T::MYSQL_TYPE_LONG_BLOB => {
            match col.character_set() {
                63 => ColKind::Binary,               // binary collation
                28 | 87 | 248 | 249 => ColKind::Gbk, // gbk / gb18030
                _ => ColKind::Text,
            }
        }
        _ => ColKind::Text,
    }
}

/// 字节按列类别解码：Binary → 0x 十六进制；GBK → GBK 解码；
/// Text → 严格 UTF-8，失败则按 GBK 兜底（列标称 utf8 但存了 GBK 的老库），最后才 lossy
fn bytes_to_json(b: &[u8], kind: ColKind) -> serde_json::Value {
    match kind {
        ColKind::Binary => {
            let hex: String = b.iter().map(|x| format!("{:02X}", x)).collect();
            serde_json::Value::String(format!("0x{}", hex))
        }
        ColKind::Gbk => {
            let (s, _, _) = encoding_rs::GBK.decode(b);
            serde_json::Value::String(s.into_owned())
        }
        ColKind::Text => match std::str::from_utf8(b) {
            Ok(s) => serde_json::Value::String(s.to_string()),
            Err(_) => {
                let (s, _, had_errors) = encoding_rs::GBK.decode(b);
                if had_errors {
                    serde_json::Value::String(String::from_utf8_lossy(b).to_string())
                } else {
                    serde_json::Value::String(s.into_owned())
                }
            }
        },
    }
}

/// MySQL 值转 JSON（DECIMAL/BLOB 走字符串；NaN/Inf 转 null 而不是静默变 0）
fn value_to_json(val: Option<mysql_async::Value>, kind: ColKind) -> serde_json::Value {
    match val {
        None => serde_json::Value::Null,
        Some(v) => match v {
            mysql_async::Value::NULL => serde_json::Value::Null,
            mysql_async::Value::Bytes(b) => bytes_to_json(&b, kind),
            mysql_async::Value::Int(i) => serde_json::Value::Number(i.into()),
            mysql_async::Value::UInt(u) => serde_json::Value::Number(u.into()),
            mysql_async::Value::Float(f) => serde_json::Number::from_f64(f as f64)
                .map(serde_json::Value::Number)
                .unwrap_or(serde_json::Value::Null),
            mysql_async::Value::Double(d) => serde_json::Number::from_f64(d)
                .map(serde_json::Value::Number)
                .unwrap_or(serde_json::Value::Null),
            mysql_async::Value::Date(y, m, d, h, min, s, _us) => serde_json::Value::String(
                format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", y, m, d, h, min, s),
            ),
            mysql_async::Value::Time(neg, days, h, m, s, _us) => {
                let total_hours = days as u32 * 24 + h as u32;
                serde_json::Value::String(format!(
                    "{}{:02}:{:02}:{:02}",
                    if neg { "-" } else { "" },
                    total_hours,
                    m,
                    s
                ))
            }
        },
    }
}

/// 被 KILL QUERY 中断的错误（errno 1317）归类为 QueryCancelled
fn map_query_error(e: mysql_async::Error) -> DatabaseError {
    if is_killed(&e) {
        DatabaseError::QueryCancelled
    } else {
        DatabaseError::QueryError(e.to_string())
    }
}

fn map_execute_error(e: mysql_async::Error) -> DatabaseError {
    if is_killed(&e) {
        DatabaseError::QueryCancelled
    } else {
        DatabaseError::ExecuteError(e.to_string())
    }
}

fn is_killed(e: &mysql_async::Error) -> bool {
    match e {
        mysql_async::Error::Server(ref se) => se.code == 1317, // ER_QUERY_INTERRUPTED
        _ => false,
    }
}
