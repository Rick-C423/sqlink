use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};
use tiberius::{AuthMethod, Client, Config};
use tiberius::ColumnType;
use crate::database::DatabaseDriver;
use crate::models::*;

/// 单次查询最多返回的行数（与其他驱动一致）
const MAX_ROWS: usize = 10_000;

type TdsClient = Client<Compat<TcpStream>>;

/// SQL Server 驱动（tiberius / TDS 协议）。
/// 单连接 + Mutex 串行化；跨库通过 USE [db] 切换上下文。
pub struct MsSqlDriver {
    connected: AtomicBool,
    client: Mutex<TdsClient>,
}

impl MsSqlDriver {
    pub async fn connect(config: &ConnectionConfig) -> Result<Self, DatabaseError> {
        let client = connect_client(config, config.database.as_deref().filter(|d| !d.is_empty())).await?;
        Ok(Self {
            connected: AtomicBool::new(true),
            client: Mutex::new(client),
        })
    }

    /// 锁定客户端并按需切换数据库上下文（USE [db]）
    async fn client_for<'a>(&'a self, database: Option<&str>) -> Result<tokio::sync::MutexGuard<'a, TdsClient>, DatabaseError> {
        let mut client = self.client.lock().await;
        if let Some(db) = database.filter(|d| !d.is_empty()) {
            let esc = db.replace(']', "]]" );
            client
                .simple_query(format!("USE [{}]", esc))
                .await
                .map_err(|e| DatabaseError::ConnectionError(format!("切换数据库 {} 失败: {}", db, e)))?;
        }
        Ok(client)
    }
}

fn build_config(config: &ConnectionConfig, database: Option<&str>) -> Config {
    let mut cfg = Config::new();
    cfg.host(&config.host);
    cfg.port(config.port);
    cfg.authentication(AuthMethod::sql_server(&config.user, &config.password));
    if let Some(db) = database {
        cfg.database(db);
    }
    // 信任服务器自签名证书（内网实例常见；SQL Server 2022 默认强制加密）
    cfg.trust_cert();
    cfg
}

async fn try_connect(
    config: &ConnectionConfig,
    database: Option<&str>,
    encryption: Option<tiberius::EncryptionLevel>,
) -> Result<TdsClient, DatabaseError> {
    let mut cfg = build_config(config, database);
    if let Some(level) = encryption {
        cfg.encryption(level);
    }
    let tcp = TcpStream::connect(cfg.get_addr())
        .await
        .map_err(|e| DatabaseError::ConnectionError(format!("TCP 连接失败: {}", e)))?;
    tcp.set_nodelay(true).ok();
    Client::connect(cfg, tcp.compat_write())
        .await
        .map_err(|e| DatabaseError::ConnectionError(e.to_string()))
}

async fn connect_client(config: &ConnectionConfig, database: Option<&str>) -> Result<TdsClient, DatabaseError> {
    match try_connect(config, database, None).await {
        Ok(client) => Ok(client),
        Err(e) => {
            let msg = e.to_string();
            // 老版本 SQL Server（2014 及更早，或未打 TLS 1.2 补丁）只支持 TLS 1.0/1.1，
            // rustls 无法与其握手（报 "tls handshake eof"）。降级为完全不加密重试：
            // 服务器未强制加密时可用，内网生产库常见。
            if msg.to_lowercase().contains("tls") {
                try_connect(config, database, Some(tiberius::EncryptionLevel::NotSupported)).await
            } else {
                Err(e)
            }
        }
    }
}

pub async fn test_connection(config: &ConnectionConfig) -> Result<(), DatabaseError> {
    let mut client = connect_client(config, config.database.as_deref().filter(|d| !d.is_empty())).await?;
    client
        .simple_query("SELECT 1")
        .await
        .map_err(|e| DatabaseError::ConnectionError(e.to_string()))?;
    Ok(())
}

fn map_query_err(e: tiberius::error::Error) -> DatabaseError {
    DatabaseError::QueryError(e.to_string())
}

fn map_exec_err(e: tiberius::error::Error) -> DatabaseError {
    DatabaseError::ExecuteError(e.to_string())
}

#[async_trait]
impl DatabaseDriver for MsSqlDriver {
    fn driver_type(&self) -> &'static str {
        "sqlserver"
    }

    fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    async fn query(&self, sql: &str, database: Option<&str>, _query_id: Option<u64>) -> Result<QueryResult, DatabaseError> {
        let start = std::time::Instant::now();
        let mut client = self.client_for(database).await?;

        let stream = client.simple_query(sql).await.map_err(map_query_err)?;
        let tds_rows = stream.into_first_result().await.map_err(map_query_err)?;

        let mut columns: Vec<ColumnMetadata> = Vec::new();
        let mut rows: Vec<RowData> = Vec::new();
        let mut truncated = false;

        if let Some(first) = tds_rows.first() {
            columns = first.columns().iter().map(|col| ColumnMetadata {
                name: col.name().to_string(),
                data_type: format!("{:?}", col.column_type()),
                unified_type: "Unknown".to_string(),
                nullable: true,
                length: None,
                precision: None,
                scale: None,
            }).collect();
        }

        for row in &tds_rows {
            if rows.len() >= MAX_ROWS {
                truncated = true;
                break;
            }
            let values = row.columns().iter().enumerate()
                .map(|(idx, col)| Some(mssql_value_to_json(row, idx, &col.column_type())))
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

    async fn execute(&self, sql: &str, database: Option<&str>, _query_id: Option<u64>) -> Result<QueryResult, DatabaseError> {
        let start = std::time::Instant::now();
        let mut client = self.client_for(database).await?;
        let res = client.execute(sql, &[]).await.map_err(map_exec_err)?;

        Ok(QueryResult {
            columns: vec![],
            rows: vec![],
            total_count: Some(0),
            execution_time_ms: start.elapsed().as_millis() as u64,
            rows_affected: Some(res.total()),
            last_insert_id: None,
            truncated: false,
        })
    }

    async fn cancel_query(&self, _query_id: u64) -> Result<(), DatabaseError> {
        Err(DatabaseError::Unknown("SQL Server 驱动暂不支持取消查询".into()))
    }

    async fn get_databases(&self) -> Result<Vec<String>, DatabaseError> {
        let result = self.query("SELECT name FROM sys.databases ORDER BY name", None, None).await?;
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
        let mut client = self.client_for(database).await?;
        let stream = client.simple_query(
            "SELECT t.name, SCHEMA_NAME(t.schema_id), COALESCE(SUM(p.rows), 0) \
             FROM sys.tables t \
             LEFT JOIN sys.partitions p ON p.object_id = t.object_id AND p.index_id IN (0,1) \
             GROUP BY t.name, t.schema_id ORDER BY t.name",
        ).await.map_err(map_query_err)?;
        let rows = stream.into_first_result().await.map_err(map_query_err)?;

        let tables = rows.iter().map(|row| {
            let name: &str = row.get(0).unwrap_or("");
            let schema: Option<&str> = row.get(1);
            TableInfo {
                name: name.to_string(),
                schema: schema.map(|s| s.to_string()),
                row_count: row.get::<i64, _>(2).or_else(|| row.get::<i32, _>(2).map(|v| v as i64)),
                engine: None,
                comment: None,
            }
        }).collect();
        Ok(tables)
    }

    async fn get_views(&self, database: Option<&str>) -> Result<Vec<TableInfo>, DatabaseError> {
        let mut client = self.client_for(database).await?;
        let stream = client.simple_query(
            "SELECT v.name, SCHEMA_NAME(v.schema_id) \
             FROM sys.views v ORDER BY v.name",
        ).await.map_err(map_query_err)?;
        let rows = stream.into_first_result().await.map_err(map_query_err)?;

        Ok(rows.iter().map(|row| {
            let name: &str = row.get(0).unwrap_or("");
            let schema: Option<&str> = row.get(1);
            TableInfo {
                name: name.to_string(),
                schema: schema.map(|s| s.to_string()),
                row_count: None,
                engine: None,
                comment: None,
            }
        }).collect())
    }

    async fn get_view_definition(&self, database: Option<&str>, view: &str) -> Result<String, DatabaseError> {
        let mut client = self.client_for(database).await?;
        // OBJECT_DEFINITION 返回 CREATE VIEW 完整文本
        let stream = client.query(
            "SELECT OBJECT_DEFINITION(OBJECT_ID(@P1, 'V'))",
            &[&view],
        ).await.map_err(map_query_err)?;
        let rows = stream.into_first_result().await.map_err(map_query_err)?;
        rows.first()
            .and_then(|r| r.get::<&str, _>(0).map(|s| s.to_string()))
            .ok_or_else(|| DatabaseError::Unknown(format!("视图 {} 定义为空", view)))
    }

    async fn get_users(&self, database: Option<&str>) -> Result<Vec<DbUserInfo>, DatabaseError> {
        let mut client = self.client_for(database).await?;
        // 服务器级登录名：SQL 登录 + Windows 登录，排除系统内置
        let stream = client.simple_query(
            "SELECT name, type_desc FROM sys.server_principals \
             WHERE type IN ('S', 'U') AND name NOT LIKE '##%' AND name <> 'sa' \
             ORDER BY name",
        ).await.map_err(map_query_err)?;
        let rows = stream.into_first_result().await.map_err(map_query_err)?;
        Ok(rows.iter().map(|row| {
            let name: &str = row.get(0).unwrap_or("");
            let type_desc: &str = row.get(1).unwrap_or("");
            DbUserInfo {
                name: name.to_string(),
                host: None,
                note: Some(if type_desc.contains("SQL") { "SQL 登录".to_string() } else { "Windows 登录".to_string() }),
            }
        }).collect())
    }

    async fn get_current_user(&self, database: Option<&str>) -> Result<CurrentUserInfo, DatabaseError> {
        let mut client = self.client_for(database).await?;
        let stream = client.simple_query("SELECT SUSER_NAME()").await.map_err(map_query_err)?;
        let rows = stream.into_first_result().await.map_err(map_query_err)?;
        let user = rows.first()
            .and_then(|r| r.get::<&str, _>(0).map(|s| s.to_string()))
            .unwrap_or_else(|| "unknown".to_string());

        // 服务器级权限 + 服务器角色成员
        let stream = client.simple_query(
            "SELECT sp.state_desc + ' ' + sp.permission_name \
             FROM sys.server_permissions sp \
             JOIN sys.server_principals p ON sp.grantee_principal_id = p.principal_id \
             WHERE p.name = SUSER_NAME() AND sp.major_id = 0 \
             UNION ALL \
             SELECT '成员角色: ' + r.name \
             FROM sys.server_role_members m \
             JOIN sys.server_principals r ON m.role_principal_id = r.principal_id \
             JOIN sys.server_principals u ON m.member_principal_id = u.principal_id \
             WHERE u.name = SUSER_NAME()",
        ).await.map_err(map_query_err)?;
        let rows = stream.into_first_result().await.map_err(map_query_err)?;
        let grants = rows.iter()
            .filter_map(|r| r.get::<&str, _>(0).map(|s| s.to_string()))
            .collect();
        Ok(CurrentUserInfo { user, grants })
    }

    async fn get_columns(&self, database: Option<&str>, table: &str) -> Result<Vec<ColumnInfo>, DatabaseError> {
        let mut client = self.client_for(database).await?;
        let stream = client.query(
            "SELECT c.COLUMN_NAME, c.DATA_TYPE, c.IS_NULLABLE, c.COLUMN_DEFAULT, \
                    CASE WHEN kcu.COLUMN_NAME IS NOT NULL THEN 1 ELSE 0 END, \
                    CAST(ep.value AS NVARCHAR(4000)) \
             FROM INFORMATION_SCHEMA.COLUMNS c \
             LEFT JOIN INFORMATION_SCHEMA.TABLE_CONSTRAINTS tc \
               ON tc.TABLE_NAME = c.TABLE_NAME AND tc.TABLE_SCHEMA = c.TABLE_SCHEMA \
              AND tc.CONSTRAINT_TYPE = 'PRIMARY KEY' \
             LEFT JOIN INFORMATION_SCHEMA.KEY_COLUMN_USAGE kcu \
               ON kcu.CONSTRAINT_NAME = tc.CONSTRAINT_NAME \
              AND kcu.TABLE_NAME = c.TABLE_NAME AND kcu.TABLE_SCHEMA = c.TABLE_SCHEMA \
              AND kcu.COLUMN_NAME = c.COLUMN_NAME \
             LEFT JOIN sys.tables st ON st.name = c.TABLE_NAME \
             LEFT JOIN sys.columns sc ON sc.object_id = st.object_id AND sc.name = c.COLUMN_NAME \
             LEFT JOIN sys.extended_properties ep \
               ON ep.major_id = st.object_id AND ep.minor_id = sc.column_id AND ep.name = 'MS_Description' \
             WHERE c.TABLE_NAME = @P1 \
             ORDER BY c.ORDINAL_POSITION",
            &[&table],
        ).await.map_err(map_query_err)?;
        let rows = stream.into_first_result().await.map_err(map_query_err)?;

        let columns = rows.iter().map(|row| {
            let name: &str = row.get(0).unwrap_or("");
            let data_type: &str = row.get(1).unwrap_or("");
            let nullable: &str = row.get(2).unwrap_or("YES");
            let default: Option<&str> = row.get(3);
            let is_pk: i32 = row.get(4).unwrap_or(0);
            let comment: Option<&str> = row.get(5);
            ColumnInfo {
                name: name.to_string(),
                data_type: data_type.to_string(),
                nullable: nullable == "YES",
                is_primary_key: is_pk == 1,
                default_value: default.map(|s| s.to_string()),
                extra: None,
                comment: comment.map(|s| s.to_string()),
            }
        }).collect();
        Ok(columns)
    }

    async fn get_indexes(&self, database: Option<&str>, table: &str) -> Result<Vec<IndexInfo>, DatabaseError> {
        let mut client = self.client_for(database).await?;
        let stream = client.query(
            "SELECT i.name, i.is_unique, i.is_primary_key, c.name \
             FROM sys.indexes i \
             JOIN sys.tables t ON t.object_id = i.object_id \
             JOIN sys.index_columns ic ON ic.object_id = i.object_id AND ic.index_id = i.index_id \
             JOIN sys.columns c ON c.object_id = ic.object_id AND c.column_id = ic.column_id \
             WHERE t.name = @P1 AND i.type > 0 \
             ORDER BY i.name, ic.key_ordinal",
            &[&table],
        ).await.map_err(map_query_err)?;
        let rows = stream.into_first_result().await.map_err(map_query_err)?;

        let mut order: Vec<String> = Vec::new();
        let mut map: HashMap<String, (bool, bool, Vec<String>)> = HashMap::new();
        for row in rows.iter() {
            let name: &str = row.get(0).unwrap_or("");
            let unique: bool = row.get(1).unwrap_or(false);
            let primary: bool = row.get(2).unwrap_or(false);
            let col: &str = row.get(3).unwrap_or("");
            let entry = map.entry(name.to_string()).or_insert_with(|| {
                order.push(name.to_string());
                (unique, primary, Vec::new())
            });
            entry.2.push(col.to_string());
        }
        Ok(order.into_iter().filter_map(|n| {
            map.remove(&n).map(|(unique, primary, columns)| IndexInfo {
                is_primary: primary,
                is_unique: unique,
                name: n,
                columns,
            })
        }).collect())
    }

    async fn get_foreign_keys(&self, database: Option<&str>, table: &str) -> Result<Vec<ForeignKeyInfo>, DatabaseError> {
        let mut client = self.client_for(database).await?;
        let stream = client.query(
            "SELECT fk.name, pc.name, rt.name, rc.name, \
                    fk.update_referential_action_desc, fk.delete_referential_action_desc \
             FROM sys.foreign_keys fk \
             JOIN sys.foreign_key_columns fkc ON fkc.constraint_object_id = fk.object_id \
             JOIN sys.tables pt ON pt.object_id = fk.parent_object_id \
             JOIN sys.columns pc ON pc.object_id = pt.object_id AND pc.column_id = fkc.parent_column_id \
             JOIN sys.tables rt ON rt.object_id = fk.referenced_object_id \
             JOIN sys.columns rc ON rc.object_id = rt.object_id AND rc.column_id = fkc.referenced_column_id \
             WHERE pt.name = @P1 \
             ORDER BY fk.name, fkc.constraint_column_id",
            &[&table],
        ).await.map_err(map_query_err)?;
        let rows = stream.into_first_result().await.map_err(map_query_err)?;

        let mut order: Vec<String> = Vec::new();
        let mut map: HashMap<String, ForeignKeyInfo> = HashMap::new();
        for row in rows.iter() {
            let name: &str = row.get(0).unwrap_or("");
            let col: &str = row.get(1).unwrap_or("");
            let entry = map.entry(name.to_string()).or_insert_with(|| {
                order.push(name.to_string());
                ForeignKeyInfo {
                    name: name.to_string(),
                    columns: Vec::new(),
                    ref_table: row.get::<&str, _>(2).unwrap_or("").to_string(),
                    ref_schema: Some("dbo".to_string()),
                    ref_columns: Vec::new(),
                    on_update: row.get::<&str, _>(4).map(|s| s.to_string()),
                    on_delete: row.get::<&str, _>(5).map(|s| s.to_string()),
                }
            });
            entry.columns.push(col.to_string());
            let ref_col: &str = row.get(3).unwrap_or("");
            entry.ref_columns.push(ref_col.to_string());
        }
        Ok(order.into_iter().filter_map(|n| map.remove(&n)).collect())
    }

    async fn execute_batch(&self, sqls: Vec<String>, database: Option<&str>) -> Result<u64, DatabaseError> {
        let mut client = self.client_for(database).await?;

        client.simple_query("BEGIN TRAN").await.map_err(map_exec_err)?;

        let mut total: u64 = 0;
        for sql in &sqls {
            match client.execute(sql.as_str(), &[]).await {
                Ok(res) => total += res.total(),
                Err(e) => {
                    let _ = client.simple_query("ROLLBACK TRAN").await;
                    return Err(map_exec_err(e));
                }
            }
        }

        client.simple_query("COMMIT TRAN").await.map_err(map_exec_err)?;
        Ok(total)
    }

    async fn export_table_csv(&self, database: Option<&str>, table: &str, path: &str) -> Result<u64, DatabaseError> {
        let mut client = self.client_for(database).await?;
        let sql = format!("SELECT * FROM [{}]", self.escape_identifier(table));
        let stream = client.simple_query(sql).await.map_err(map_query_err)?;
        let rows = stream.into_first_result().await.map_err(map_query_err)?;

        let columns: Vec<String> = rows.first()
            .map(|r| r.columns().iter().map(|c| c.name().to_string()).collect())
            .unwrap_or_default();
        let data: Vec<Vec<Option<serde_json::Value>>> = rows.iter()
            .map(|r| {
                r.columns().iter().enumerate()
                    .map(|(i, c)| Some(mssql_value_to_json(r, i, &c.column_type())))
                    .collect()
            })
            .collect();

        crate::database::write_csv(path, &columns, data)
    }

    async fn dump_table(&self, database: Option<&str>, table: &str, include_drop: bool, include_data: bool) -> Result<(String, u64), DatabaseError> {
        let esc = self.escape_identifier(table);

        let mut out = String::new();
        out.push_str(&format!("-- ------------------------------------------------------\n-- 表 [{}]\n-- ------------------------------------------------------\n\n", table));

        if include_drop {
            out.push_str(&format!("DROP TABLE IF EXISTS [{}];\n\n", esc));
        }

        // SQL Server 没有 SHOW CREATE TABLE，用列信息重建近似定义
        let cols = self.get_columns(database, table).await?;
        if cols.is_empty() {
            return Err(DatabaseError::Unknown(format!("表 {} 不存在或无列", table)));
        }
        let mut defs: Vec<String> = cols.iter().map(|c| {
            let mut d = format!("[{}] {}", self.escape_identifier(&c.name), c.data_type);
            if let Some(def) = &c.default_value {
                d.push_str(&format!(" DEFAULT {}", def));
            }
            d.push_str(if c.nullable { " NULL" } else { " NOT NULL" });
            d
        }).collect();
        let pk: Vec<String> = cols.iter()
            .filter(|c| c.is_primary_key)
            .map(|c| format!("[{}]", self.escape_identifier(&c.name)))
            .collect();
        if !pk.is_empty() {
            defs.push(format!("PRIMARY KEY ({})", pk.join(", ")));
        }
        out.push_str(&format!("CREATE TABLE [{}] (\n  {}\n);\n\n", esc, defs.join(",\n  ")));

        let mut rows_written = 0u64;
        if include_data {
            let mut client = self.client_for(database).await?;
            let stream = client
                .simple_query(format!("SELECT * FROM [{}]", esc))
                .await
                .map_err(map_query_err)?;
            let rows = stream.into_first_result().await.map_err(map_query_err)?;
            let columns: Vec<String> = rows.first()
                .map(|r| r.columns().iter().map(|c| c.name().to_string()).collect())
                .unwrap_or_default();
            let data: Vec<Vec<Option<serde_json::Value>>> = rows.iter()
                .map(|r| {
                    r.columns().iter().enumerate()
                        .map(|(i, c)| Some(mssql_value_to_json(r, i, &c.column_type())))
                        .collect()
                })
                .collect();
            let quoted_cols = columns.iter()
                .map(|c| format!("[{}]", self.escape_identifier(c)))
                .collect::<Vec<_>>()
                .join(", ");
            rows_written = crate::database::dump::write_insert_batches(
                &mut out,
                &format!("[{}]", esc),
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
        identifier.replace(']', "]]" )
    }
}

/// TDS 值转 JSON（按可能性链式探测解码，不支持的类型给占位符）
fn mssql_value_to_json(row: &tiberius::Row, idx: usize, ty: &ColumnType) -> serde_json::Value {
    use serde_json::Value as J;

    macro_rules! get {
        ($t:ty) => {
            match row.try_get::<$t, usize>(idx) {
                Ok(Some(v)) => return J::from(v),
                Ok(None) => return J::Null,
                Err(_) => {}
            }
        };
    }
    macro_rules! get_str {
        ($t:ty, $f:expr) => {
            match row.try_get::<$t, usize>(idx) {
                Ok(Some(v)) => return J::String($f(v)),
                Ok(None) => return J::Null,
                Err(_) => {}
            }
        };
    }

    get!(bool);
    get!(i64);
    get!(i32);
    get!(i16);
    get!(u8);
    get!(f64);
    get!(f32);
    get!(&str);
    get_str!(tiberius::numeric::Numeric, |v: tiberius::numeric::Numeric| v.to_string());
    get_str!(uuid::Uuid, |v: uuid::Uuid| v.to_string());
    get_str!(chrono::NaiveDateTime, |v: chrono::NaiveDateTime| v.format("%Y-%m-%d %H:%M:%S").to_string());
    get_str!(chrono::NaiveDate, |v: chrono::NaiveDate| v.format("%Y-%m-%d").to_string());
    get_str!(chrono::NaiveTime, |v: chrono::NaiveTime| v.format("%H:%M:%S").to_string());
    get_str!(chrono::DateTime<chrono::Utc>, |v: chrono::DateTime<chrono::Utc>| v.format("%Y-%m-%d %H:%M:%S %z").to_string());
    get_str!(&[u8], |v: &[u8]| format!("\\x{}", v.iter().map(|b| format!("{:02x}", b)).collect::<String>()));
    get_str!(&tiberius::xml::XmlData, |v: &tiberius::xml::XmlData| v.to_string());

    J::String(format!("‹{:?}›", ty))
}
