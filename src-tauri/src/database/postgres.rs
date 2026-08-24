use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::Mutex;
use tokio_postgres::{Client, Config, NoTls};
use tokio_postgres::types::Type;
use crate::database::DatabaseDriver;
use crate::models::*;

/// 单次查询最多返回的行数（与 MySQL 驱动一致）
const MAX_ROWS: usize = 10_000;

pub struct PgDriver {
    config: ConnectionConfig,
    connected: AtomicBool,
    /// database -> Client（PG 没有 USE 语句，跨库只能另建连接，这里做惰性缓存）
    clients: Mutex<HashMap<String, Arc<Client>>>,
    /// query_id -> CancelToken（用于取消正在执行的查询）
    cancel_tokens: Mutex<HashMap<u64, tokio_postgres::CancelToken>>,
}

impl PgDriver {
    pub async fn connect(config: &ConnectionConfig) -> Result<Self, DatabaseError> {
        let driver = Self {
            config: config.clone(),
            connected: AtomicBool::new(false),
            clients: Mutex::new(HashMap::new()),
            cancel_tokens: Mutex::new(HashMap::new()),
        };
        // 立即建立默认库连接验证可用性
        driver.client_for(None).await?;
        driver.connected.store(true, Ordering::Relaxed);
        Ok(driver)
    }

    fn pg_config(&self, database: &str) -> Config {
        let mut cfg = Config::new();
        cfg.host(&self.config.host)
            .port(self.config.port)
            .user(&self.config.user)
            .dbname(database);
        if !self.config.password.is_empty() {
            cfg.password(&self.config.password);
        }
        cfg
    }

    /// 取指定数据库的客户端（不存在则新建连接并缓存）
    async fn client_for(&self, database: Option<&str>) -> Result<Arc<Client>, DatabaseError> {
        let db_name = database
            .filter(|d| !d.is_empty())
            .or(self.config.database.as_deref().filter(|d| !d.is_empty()))
            .unwrap_or("postgres")
            .to_string();

        let mut guard = self.clients.lock().await;
        if let Some(client) = guard.get(&db_name) {
            if !client.is_closed() {
                return Ok(Arc::clone(client));
            }
        }

        let (client, connection) = self.pg_config(&db_name).connect(NoTls).await
            .map_err(|e| DatabaseError::ConnectionError(e.to_string()))?;

        tokio::spawn(async move {
            if let Err(e) = connection.await {
                eprintln!("postgres connection error: {}", e);
            }
        });

        let client = Arc::new(client);
        guard.insert(db_name, Arc::clone(&client));
        Ok(client)
    }
}

pub async fn test_connection(config: &ConnectionConfig) -> Result<(), DatabaseError> {
    let db = config.database.as_deref().filter(|d| !d.is_empty()).unwrap_or("postgres");

    let mut cfg = Config::new();
    cfg.host(&config.host)
        .port(config.port)
        .user(&config.user)
        .dbname(db);
    if !config.password.is_empty() {
        cfg.password(&config.password);
    }

    let (client, connection) = cfg.connect(NoTls).await
        .map_err(|e| DatabaseError::ConnectionError(e.to_string()))?;

    tokio::spawn(async move {
        let _ = connection.await;
    });

    client.query_one("SELECT 1", &[]).await
        .map_err(|e| DatabaseError::ConnectionError(e.to_string()))?;

    Ok(())
}

#[async_trait]
impl DatabaseDriver for PgDriver {
    fn driver_type(&self) -> &'static str {
        "postgres"
    }

    fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    async fn query(&self, sql: &str, database: Option<&str>, query_id: Option<u64>) -> Result<QueryResult, DatabaseError> {
        let start = std::time::Instant::now();
        let client = self.client_for(database).await?;

        if let Some(qid) = query_id {
            self.cancel_tokens.lock().await.insert(qid, client.cancel_token());
        }

        let rows_result = client.query(sql, &[]).await;
        if let Some(qid) = query_id {
            self.cancel_tokens.lock().await.remove(&qid);
        }
        let pg_rows = rows_result.map_err(map_pg_query_error)?;

        let mut columns: Vec<ColumnMetadata> = Vec::new();
        let mut rows: Vec<RowData> = Vec::new();
        let mut truncated = false;

        if let Some(first) = pg_rows.first() {
            columns = first.columns().iter().map(|col| ColumnMetadata {
                name: col.name().to_string(),
                data_type: format!("{}", col.type_()),
                unified_type: "Unknown".to_string(),
                nullable: true, // information_schema 才有准确值，结果集层面默认为 true
                length: None,
                precision: None,
                scale: None,
            }).collect();
        }

        for pg_row in &pg_rows {
            if rows.len() >= MAX_ROWS {
                truncated = true;
                break;
            }
            let values = pg_row.columns().iter().enumerate()
                .map(|(idx, col)| Some(pg_value_to_json(pg_row, idx, col.type_())))
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

    async fn execute(&self, sql: &str, database: Option<&str>, query_id: Option<u64>) -> Result<QueryResult, DatabaseError> {
        let start = std::time::Instant::now();
        let client = self.client_for(database).await?;

        if let Some(qid) = query_id {
            self.cancel_tokens.lock().await.insert(qid, client.cancel_token());
        }

        let exec_result = client.execute(sql, &[]).await;
        if let Some(qid) = query_id {
            self.cancel_tokens.lock().await.remove(&qid);
        }
        let affected = exec_result.map_err(map_pg_execute_error)?;

        Ok(QueryResult {
            columns: vec![],
            rows: vec![],
            total_count: Some(0),
            execution_time_ms: start.elapsed().as_millis() as u64,
            rows_affected: Some(affected),
            last_insert_id: None,
            truncated: false,
        })
    }

    async fn cancel_query(&self, query_id: u64) -> Result<(), DatabaseError> {
        let token = self.cancel_tokens.lock().await.get(&query_id).cloned();
        let token = token.ok_or(DatabaseError::Unknown("查询不存在或已结束".to_string()))?;
        token.cancel_query(NoTls).await
            .map_err(|e| DatabaseError::QueryError(e.to_string()))?;
        Ok(())
    }

    async fn get_databases(&self) -> Result<Vec<String>, DatabaseError> {
        let result = self.query(
            "SELECT datname FROM pg_database WHERE NOT datistemplate ORDER BY datname",
            None, None,
        ).await?;
        let dbs = result.rows.iter()
            .filter_map(|row| {
                row.values.first()
                    .and_then(|v| v.as_ref())
                    .and_then(|v| v.as_str().map(|s| s.to_string()))
            })
            .collect();
        Ok(dbs)
    }

    async fn get_tables(&self, database: Option<&str>) -> Result<Vec<TableInfo>, DatabaseError> {
        let client = self.client_for(database).await?;
        // reltuples 是优化器估算值，够用且快；obj_description 取表注释
        let rows = client.query(
            "SELECT c.relname, GREATEST(c.reltuples, 0)::bigint, obj_description(c.oid) \
             FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = 'public' AND c.relkind = 'r' ORDER BY c.relname",
            &[],
        ).await.map_err(map_pg_query_error)?;

        let tables = rows.iter().map(|row| {
            let name: String = row.get(0);
            let row_count: i64 = row.get(1);
            let comment: Option<String> = row.get(2);
            TableInfo {
                name,
                schema: Some("public".to_string()),
                row_count: Some(row_count),
                engine: None,
                comment,
            }
        }).collect();
        Ok(tables)
    }

    async fn get_views(&self, database: Option<&str>) -> Result<Vec<TableInfo>, DatabaseError> {
        let client = self.client_for(database).await?;
        // relkind: v = 普通视图, m = 物化视图
        let rows = client.query(
            "SELECT c.relname, CASE c.relkind WHEN 'm' THEN 'materialized' ELSE 'view' END \
             FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = 'public' AND c.relkind IN ('v', 'm') ORDER BY c.relname",
            &[],
        ).await.map_err(map_pg_query_error)?;

        Ok(rows.iter().map(|row| {
            let name: String = row.get(0);
            let kind: &str = row.get(1);
            TableInfo {
                name,
                schema: Some("public".to_string()),
                row_count: None,
                engine: Some(kind.to_string()),
                comment: None,
            }
        }).collect())
    }

    async fn get_view_definition(&self, database: Option<&str>, view: &str) -> Result<String, DatabaseError> {
        let client = self.client_for(database).await?;
        let row = client.query_opt(
            "SELECT pg_get_viewdef(format('%I.%I', 'public', $1)::regclass, true)",
            &[&view],
        ).await.map_err(map_pg_query_error)?;
        let def: Option<String> = row.and_then(|r| r.get(0));
        def.map(|d| format!("CREATE OR REPLACE VIEW public.\"{}\" AS\n{}", view.replace('"', "\"\""), d))
            .ok_or_else(|| DatabaseError::Unknown(format!("视图 {} 不存在", view)))
    }

    async fn get_columns(&self, database: Option<&str>, table: &str) -> Result<Vec<ColumnInfo>, DatabaseError> {
        let client = self.client_for(database).await?;
        let rows = client.query(
            "SELECT c.column_name, c.data_type, c.is_nullable, c.column_default, \
             EXISTS ( \
               SELECT 1 FROM information_schema.table_constraints tc \
               JOIN information_schema.key_column_usage kcu \
                 ON tc.constraint_name = kcu.constraint_name \
                AND tc.table_schema = kcu.table_schema \
                AND tc.table_name = kcu.table_name \
               WHERE tc.constraint_type = 'PRIMARY KEY' \
                 AND kcu.table_schema = c.table_schema \
                 AND kcu.table_name = c.table_name \
                 AND kcu.column_name = c.column_name \
             ) AS is_pk, \
             pg_catalog.col_description(\
               format('%I.%I', c.table_schema, c.table_name)::regclass, c.ordinal_position\
             ) AS comment \
             FROM information_schema.columns c \
             WHERE c.table_schema = 'public' AND c.table_name = $1 \
             ORDER BY c.ordinal_position",
            &[&table],
        ).await.map_err(map_pg_query_error)?;

        let columns = rows.iter().map(|row| {
            let name: String = row.get(0);
            let data_type: String = row.get(1);
            let is_nullable: String = row.get(2);
            let default: Option<String> = row.get(3);
            let is_pk: bool = row.get(4);
            let comment: Option<String> = row.get(5);
            ColumnInfo {
                name,
                data_type,
                nullable: is_nullable == "YES",
                is_primary_key: is_pk,
                default_value: default,
                extra: None,
                comment,
            }
        }).collect();
        Ok(columns)
    }

    async fn get_indexes(&self, database: Option<&str>, table: &str) -> Result<Vec<IndexInfo>, DatabaseError> {
        let client = self.client_for(database).await?;
        let rows = client.query(
            "SELECT indexname, indexdef FROM pg_indexes \
             WHERE schemaname = 'public' AND tablename = $1 ORDER BY indexname",
            &[&table],
        ).await.map_err(map_pg_query_error)?;

        let indexes = rows.iter().map(|row| {
            let name: String = row.get(0);
            let def: String = row.get(1);
            // 从 indexdef 的括号里粗解析列名
            let columns = def
                .find('(')
                .and_then(|open| def.rfind(')').map(|close| &def[open + 1..close]))
                .map(|inner| inner.split(',').map(|c| c.trim().trim_matches('"').to_string()).collect())
                .unwrap_or_default();
            IndexInfo {
                is_primary: name.ends_with("_pkey"),
                is_unique: def.to_uppercase().contains("UNIQUE"),
                name,
                columns,
            }
        }).collect();
        Ok(indexes)
    }

    async fn get_foreign_keys(&self, database: Option<&str>, table: &str) -> Result<Vec<ForeignKeyInfo>, DatabaseError> {
        let client = self.client_for(database).await?;
        let rows = client.query(
            "SELECT tc.constraint_name, kcu.column_name, \
                    ccu.table_schema, ccu.table_name, ccu.column_name, \
                    rc.update_rule, rc.delete_rule \
             FROM information_schema.table_constraints tc \
             JOIN information_schema.key_column_usage kcu \
               ON tc.constraint_name = kcu.constraint_name \
              AND tc.table_schema = kcu.table_schema \
              AND tc.table_name = kcu.table_name \
             JOIN information_schema.constraint_column_usage ccu \
               ON ccu.constraint_name = tc.constraint_name \
              AND ccu.table_schema = tc.table_schema \
             JOIN information_schema.referential_constraints rc \
               ON rc.constraint_name = tc.constraint_name \
              AND rc.constraint_schema = tc.table_schema \
             WHERE tc.constraint_type = 'FOREIGN KEY' \
               AND tc.table_schema = 'public' AND tc.table_name = $1 \
             ORDER BY tc.constraint_name, kcu.ordinal_position",
            &[&table],
        ).await.map_err(map_pg_query_error)?;

        // 按约束名分组（保持列顺序）
        let mut order: Vec<String> = Vec::new();
        let mut map: HashMap<String, ForeignKeyInfo> = HashMap::new();
        for row in rows.iter() {
            let name: String = row.get(0);
            let col: String = row.get(1);
            let entry = map.entry(name.clone()).or_insert_with(|| {
                order.push(name.clone());
                ForeignKeyInfo {
                    name: name.clone(),
                    columns: Vec::new(),
                    ref_table: row.get(3),
                    ref_schema: Some(row.get(2)),
                    ref_columns: Vec::new(),
                    on_update: row.get(5),
                    on_delete: row.get(6),
                }
            });
            entry.columns.push(col);
            let ref_col: String = row.get(4);
            entry.ref_columns.push(ref_col);
        }
        Ok(order.into_iter().filter_map(|n| map.remove(&n)).collect())
    }

    async fn execute_batch(&self, sqls: Vec<String>, database: Option<&str>) -> Result<u64, DatabaseError> {
        let client = self.client_for(database).await?;

        client.batch_execute("BEGIN").await
            .map_err(|e| DatabaseError::ExecuteError(e.to_string()))?;

        let mut total: u64 = 0;
        for sql in &sqls {
            match client.execute(sql.as_str(), &[]).await {
                Ok(affected) => total += affected,
                Err(e) => {
                    let _ = client.batch_execute("ROLLBACK").await;
                    return Err(map_pg_execute_error(e));
                }
            }
        }

        client.batch_execute("COMMIT").await
            .map_err(|e| DatabaseError::ExecuteError(e.to_string()))?;

        Ok(total)
    }

    async fn disconnect(&self) -> Result<(), DatabaseError> {
        self.connected.store(false, Ordering::Relaxed);
        self.clients.lock().await.clear(); // Client 句柄 drop 后连接任务随之结束
        Ok(())
    }

    async fn export_table_csv(&self, database: Option<&str>, table: &str, path: &str) -> Result<u64, DatabaseError> {
        let client = self.client_for(database).await?;

        // 先 prepare 拿列信息（空表也能拿到表头），再执行
        let stmt = client
            .prepare(&format!("SELECT * FROM \"{}\"", self.escape_identifier(table)))
            .await
            .map_err(map_pg_query_error)?;
        let columns: Vec<String> = stmt.columns().iter().map(|c| c.name().to_string()).collect();
        let col_types: Vec<Type> = stmt.columns().iter().map(|c| c.type_().clone()).collect();

        // 全量导出，不受 MAX_ROWS 限制
        let rows = client.query(&stmt, &[]).await.map_err(map_pg_query_error)?;
        let data: Vec<Vec<Option<serde_json::Value>>> = rows.iter()
            .map(|r| {
                (0..columns.len())
                    .map(|i| Some(pg_value_to_json(r, i, &col_types[i])))
                    .collect()
            })
            .collect();

        crate::database::write_csv(path, &columns, data)
    }

    async fn dump_table(&self, database: Option<&str>, table: &str, include_drop: bool, include_data: bool) -> Result<(String, u64), DatabaseError> {
        let client = self.client_for(database).await?;
        let esc = self.escape_identifier(table);

        let mut out = String::new();
        out.push_str(&format!("-- ------------------------------------------------------\n-- 表 \"{}\"\n-- ------------------------------------------------------\n\n", table));

        if include_drop {
            out.push_str(&format!("DROP TABLE IF EXISTS \"{}\";\n\n", esc));
        }

        // PG 没有 SHOW CREATE TABLE，用 information_schema 的列信息重建近似定义
        let cols = self.get_columns(database, table).await?;
        if cols.is_empty() {
            return Err(DatabaseError::Unknown(format!("表 {} 不存在或无列", table)));
        }
        let mut defs: Vec<String> = cols.iter().map(|c| {
            let mut d = format!("\"{}\" {}", self.escape_identifier(&c.name), c.data_type);
            if let Some(def) = &c.default_value {
                d.push_str(&format!(" DEFAULT {}", def));
            }
            if !c.nullable {
                d.push_str(" NOT NULL");
            }
            d
        }).collect();
        let pk: Vec<String> = cols.iter()
            .filter(|c| c.is_primary_key)
            .map(|c| format!("\"{}\"", self.escape_identifier(&c.name)))
            .collect();
        if !pk.is_empty() {
            defs.push(format!("PRIMARY KEY ({})", pk.join(", ")));
        }
        out.push_str(&format!("CREATE TABLE \"{}\" (\n  {}\n);\n\n", esc, defs.join(",\n  ")));

        let mut rows_written = 0u64;
        if include_data {
            let stmt = client
                .prepare(&format!("SELECT * FROM \"{}\"", esc))
                .await
                .map_err(map_pg_query_error)?;
            let columns: Vec<String> = stmt.columns().iter().map(|c| c.name().to_string()).collect();
            let col_types: Vec<Type> = stmt.columns().iter().map(|c| c.type_().clone()).collect();
            let rows = client.query(&stmt, &[]).await.map_err(map_pg_query_error)?;
            let data: Vec<Vec<Option<serde_json::Value>>> = rows.iter()
                .map(|r| (0..columns.len()).map(|i| Some(pg_value_to_json(r, i, &col_types[i]))).collect())
                .collect();
            let quoted_cols = columns.iter()
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

    fn escape_identifier(&self, identifier: &str) -> String {
        identifier.replace('"', "\"\"")
    }
}

/// PG 值转 JSON（NUMERIC 等无原生 FromSql 的类型兜底为文本）
fn pg_value_to_json(row: &tokio_postgres::Row, idx: usize, ty: &Type) -> serde_json::Value {
    use serde_json::Value as J;

    macro_rules! get {
        ($t:ty) => {
            match row.try_get::<usize, Option<$t>>(idx) {
                Ok(Some(v)) => return J::from(v),
                Ok(None) => return J::Null,
                Err(_) => {}
            }
        };
    }

    match *ty {
        Type::BOOL => get!(bool),
        Type::INT2 => get!(i16),
        Type::INT4 => get!(i32),
        Type::INT8 => get!(i64),
        Type::FLOAT4 => get!(f32),
        Type::FLOAT8 => get!(f64),
        Type::TEXT | Type::VARCHAR | Type::BPCHAR | Type::NAME | Type::UNKNOWN => get!(String),
        Type::JSON | Type::JSONB => {
            match row.try_get::<usize, Option<serde_json::Value>>(idx) {
                Ok(Some(v)) => return J::String(v.to_string()),
                Ok(None) => return J::Null,
                Err(_) => {}
            }
        }
        Type::TIMESTAMP => {
            match row.try_get::<usize, Option<chrono::NaiveDateTime>>(idx) {
                Ok(Some(v)) => return J::String(v.format("%Y-%m-%d %H:%M:%S").to_string()),
                Ok(None) => return J::Null,
                Err(_) => {}
            }
        }
        Type::TIMESTAMPTZ => {
            match row.try_get::<usize, Option<chrono::DateTime<chrono::Utc>>>(idx) {
                Ok(Some(v)) => return J::String(v.format("%Y-%m-%d %H:%M:%S").to_string()),
                Ok(None) => return J::Null,
                Err(_) => {}
            }
        }
        Type::DATE => {
            match row.try_get::<usize, Option<chrono::NaiveDate>>(idx) {
                Ok(Some(v)) => return J::String(v.format("%Y-%m-%d").to_string()),
                Ok(None) => return J::Null,
                Err(_) => {}
            }
        }
        Type::TIME => {
            match row.try_get::<usize, Option<chrono::NaiveTime>>(idx) {
                Ok(Some(v)) => return J::String(v.format("%H:%M:%S").to_string()),
                Ok(None) => return J::Null,
                Err(_) => {}
            }
        }
        Type::BYTEA => {
            match row.try_get::<usize, Option<Vec<u8>>>(idx) {
                Ok(Some(v)) => return J::String(format!("\\x{}", v.iter().map(|b| format!("{:02x}", b)).collect::<String>())),
                Ok(None) => return J::Null,
                Err(_) => {}
            }
        }
        _ => {}
    }

    // 兜底：文本格式解码（NUMERIC 等）；再不行给占位符
    match row.try_get::<usize, Option<String>>(idx) {
        Ok(Some(s)) => J::String(s),
        Ok(None) => J::Null,
        Err(_) => J::String(format!("‹{}›", ty)),
    }
}

/// PG 错误码 57014 = query_canceled（被 CANCEL 中断）
fn map_pg_query_error(e: tokio_postgres::Error) -> DatabaseError {
    if is_pg_cancelled(&e) {
        DatabaseError::QueryCancelled
    } else {
        DatabaseError::QueryError(e.to_string())
    }
}

fn map_pg_execute_error(e: tokio_postgres::Error) -> DatabaseError {
    if is_pg_cancelled(&e) {
        DatabaseError::QueryCancelled
    } else {
        DatabaseError::ExecuteError(e.to_string())
    }
}

fn is_pg_cancelled(e: &tokio_postgres::Error) -> bool {
    e.as_db_error()
        .map(|d| d.code().code() == "57014")
        .unwrap_or(false)
}
