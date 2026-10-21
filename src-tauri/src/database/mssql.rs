use crate::database::result_budget::QueryBudget;
use crate::database::DatabaseDriver;
use crate::models::*;
use crate::session::{SessionCancellation, SessionCapabilities, SessionLease, TargetSnapshot};
use crate::typed_value::{
    validate_values, TimeValue, TimestampValue, TimezoneKind, TypedQueryResult, TypedValue,
};
use async_trait::async_trait;
use futures_util::{Stream, StreamExt, TryStreamExt};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tiberius::ColumnType;
use tiberius::{AuthMethod, Client, Config};
use tokio::net::TcpStream;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};

type TdsClient = Client<Compat<TcpStream>>;

/// SQL Server 驱动。每项任务新建独占 TDS 连接；元数据也使用独立短连接。
/// 当前不池化物理连接：未知事务或被丢弃的 future 永远不会进入下一项任务。
pub struct MsSqlDriver {
    connected: AtomicBool,
    config: ConnectionConfig,
}

impl MsSqlDriver {
    pub async fn connect(config: &ConnectionConfig) -> Result<Self, DatabaseError> {
        let client =
            connect_client(config, config.database.as_deref().filter(|d| !d.is_empty())).await?;
        client
            .close()
            .await
            .map_err(|e| DatabaseError::ConnectionError(e.to_string()))?;
        Ok(Self {
            connected: AtomicBool::new(true),
            config: config.clone(),
        })
    }

    async fn client_for(&self, database: Option<&str>) -> Result<TdsClient, DatabaseError> {
        if !self.is_connected() {
            return Err(DatabaseError::ConnectionError(
                "SQL Server 连接已断开，拒绝创建新会话".into(),
            ));
        }
        let database = database.filter(|d| !d.is_empty()).or(self
            .config
            .database
            .as_deref()
            .filter(|d| !d.is_empty()));
        let client = connect_client(&self.config, database).await?;
        if !self.is_connected() {
            drop(client);
            return Err(DatabaseError::ConnectionError(
                "SQL Server 连接已断开，拒绝创建新会话".into(),
            ));
        }
        Ok(client)
    }
}

const CANCELLATION_UNSUPPORTED: &str =
    "SQL Server 当前 TDS 驱动未实现可靠的服务端取消；关闭本地连接不代表服务端查询已经停止";

struct MsSqlSessionLease {
    id: String,
    client: Option<TdsClient>,
}

impl MsSqlSessionLease {
    fn take_client(&mut self) -> Result<TdsClient, DatabaseError> {
        self.client.take().ok_or_else(|| {
            DatabaseError::ConnectionError(
                "SQL Server 会话已隔离：前一次查询提前停止、操作失败或 future 被丢弃，事务结果可能未知".into(),
            )
        })
    }

    async fn finish_operation<T>(
        &mut self,
        mut client: TdsClient,
        result: Result<T, DatabaseError>,
    ) -> Result<T, DatabaseError> {
        if result.is_ok() {
            self.client = Some(client);
        } else {
            // 尽力回滚有界等待；失败、未读流或断线均丢弃 socket，且绝不覆盖原始错误。
            // COMMIT 失败可能已在服务端生效，因此此清理不构成“已回滚”的承诺。
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                client.execute("IF @@TRANCOUNT > 0 ROLLBACK TRAN", &[]),
            )
            .await;
        }
        result
    }

    fn finish_query_operation<T>(
        &mut self,
        client: TdsClient,
        result: Result<MsSqlQueryOutput<T>, DatabaseError>,
    ) -> Result<T, DatabaseError> {
        match result {
            Ok(output) => {
                if output.stream_complete {
                    self.client = Some(client);
                }
                // 未读响应直接丢弃物理连接，不能通过清理 SQL 隐式 drain 大结果。
                // 关闭本地连接仍不构成服务端取消或事务已回滚的承诺。
                Ok(output.value)
            }
            Err(error) => Err(error),
        }
    }
}

// Client 独占底层 TcpStream。Drop 直接丢弃连接，不发异步清理任务，也不放回池。
// 每个操作先将 Client 移入自己的 future，借用操作的 future 被 drop 时也会隔离 socket。
#[async_trait]
impl SessionLease for MsSqlSessionLease {
    fn session_id(&self) -> &str {
        &self.id
    }
    fn capabilities(&self) -> SessionCapabilities {
        SessionCapabilities::without_cancellation(CANCELLATION_UNSUPPORTED)
    }
    fn cancel_handle(&self) -> Option<Arc<dyn SessionCancellation>> {
        None
    }

    async fn query(&mut self, sql: &str) -> Result<QueryResult, DatabaseError> {
        let mut client = self.take_client()?;
        let result = mssql_query(&mut client, sql).await;
        self.finish_query_operation(client, result)
    }
    async fn execute(&mut self, sql: &str) -> Result<QueryResult, DatabaseError> {
        let mut client = self.take_client()?;
        let result = mssql_execute(&mut client, sql).await;
        self.finish_operation(client, result).await
    }
    async fn query_typed(
        &mut self,
        sql: &str,
        params: &[TypedValue],
    ) -> Result<TypedQueryResult, DatabaseError> {
        validate_values(params)?;
        let query = mssql_bound_query(sql, params)?;
        let mut client = self.take_client()?;
        let result = mssql_query_typed(&mut client, query).await;
        self.finish_query_operation(client, result)
    }
    async fn stream_export(
        &mut self,
        sql: &str,
        sink: Arc<dyn crate::export::ExportSink>,
    ) -> Result<u64, DatabaseError> {
        let mut client = self.take_client()?;
        let result = async {
            sink.check_cancelled()?;
            let mut stream = client.simple_query(sql).await.map_err(map_query_err)?;
            let mut seen_metadata = false;
            let mut count = 0u64;
            while let Some(item) = stream.try_next().await.map_err(map_query_err)? {
                sink.check_cancelled()?;
                match item {
                    tiberius::QueryItem::Metadata(metadata) => {
                        if seen_metadata || metadata.result_index() != 0 || metadata.columns().is_empty() {
                            return Err(DatabaseError::QueryError(
                                "EXPORT_RESULT_UNSUPPORTED: 导出只接受一个 SELECT 结果集".into(),
                            ));
                        }
                        seen_metadata = true;
                        let columns: Vec<_> = metadata.columns().iter().map(|column| {
                            crate::export::ExportColumn { name: column.name().into() }
                        }).collect();
                        sink.columns(&columns)?;
                    }
                    tiberius::QueryItem::Row(row) => {
                        if !seen_metadata || row.result_index() != 0 {
                            return Err(DatabaseError::QueryError(
                                "EXPORT_RESULT_UNSUPPORTED: 导出只接受一个 SELECT 结果集".into(),
                            ));
                        }
                        // TDS 已解码单个 wire row，在复制和编码字段前检查长度。
                        crate::export::check_raw_lengths(
                            row.cells().map(|(_, value)| mssql_raw_cell_length(value)),
                        )?;
                        let columns: Vec<_> = row.columns().iter().map(|column| {
                            (column.name().to_owned(), column.column_type())
                        }).collect();
                        let typed = row.into_iter().zip(columns).map(|(value, (name, ty))| {
                            mssql_typed_value(value, ty).ok_or_else(|| {
                                DatabaseError::TypeError(format!(
                                    "EXPORT_TYPE_UNSUPPORTED: SQL Server 列 {name} 的类型 {ty:?} 无法无损导出",
                                ))
                            })
                        }).collect::<Result<Vec<_>, _>>()?;
                        sink.row(&typed)?;
                        count = count.checked_add(1).ok_or_else(|| {
                            DatabaseError::QueryError("EXPORT_ROW_COUNT_OVERFLOW: 导出行数溢出".into())
                        })?;
                    }
                }
            }
            if !seen_metadata {
                return Err(DatabaseError::QueryError(
                    "EXPORT_RESULT_UNSUPPORTED: 导出需要一个 SELECT 结果集".into(),
                ));
            }
            sink.check_cancelled()?;
            Ok(count)
        }.await;
        // 只有流完整消费且 sink 全部成功时归还 Client；失败和 future 丢弃都直接隔离。
        if result.is_ok() {
            self.client = Some(client);
        }
        result
    }

    async fn execute_bound(
        &mut self,
        sql: &str,
        params: &[TypedValue],
    ) -> Result<u64, DatabaseError> {
        validate_values(params)?;
        mssql_single_statement(sql)?;
        let query = mssql_bound_query(sql, params)?;
        let mut client = self.take_client()?;
        let result = query
            .execute(&mut client)
            .await
            .map_err(map_exec_err)
            .map(|res| res.total());
        self.finish_operation(client, result).await
    }

    async fn inspect_target(
        &mut self,
        target: &ResolvedTable,
        lock: bool,
    ) -> Result<TargetSnapshot, DatabaseError> {
        let schema = target.schema.as_deref().ok_or_else(|| {
            DatabaseError::QueryError("OBJECT_IDENTITY_UNSUPPORTED: 缺少明确 schema".into())
        })?;
        let quoted = format!(
            "[{}].[{}]",
            schema.replace(']', "]]"),
            target.table.replace(']', "]]")
        );
        let mut client = self.take_client()?;
        let result = async {
            // 实际访问最多一行；HOLDLOCK/TABLOCK 保留对象保护，不为首屏扫描整表计数。
            client.query(format!("SELECT TOP (1) 1 FROM {quoted} WITH ({}, HOLDLOCK)",
                if lock { "TABLOCKX" } else { "TABLOCK" }), &[])
                .await.map_err(map_query_err)?.into_results().await.map_err(map_query_err)?;
            let rows = client.query(
                "SELECT DB_NAME(), CAST(DB_ID() AS varchar(20)), s.name, t.name, \
                 CAST(t.object_id AS varchar(20)), CONVERT(varchar(33), t.create_date, 126), \
                 CONVERT(varchar(33), t.modify_date, 126), t.is_memory_optimized, t.temporal_type, \
                 t.is_filetable, t.is_remote_data_archive_enabled, \
                 CASE WHEN EXISTS (SELECT 1 FROM sys.triggers tr WHERE tr.parent_id = t.object_id) THEN 1 ELSE 0 END, \
                 CASE WHEN EXISTS (SELECT 1 FROM sys.security_predicates sp WHERE sp.target_object_id = t.object_id) THEN 1 ELSE 0 END, \
                 HAS_PERMS_BY_NAME(QUOTENAME(s.name) + '.' + QUOTENAME(t.name), 'OBJECT', 'VIEW DEFINITION') \
                 FROM sys.tables t JOIN sys.schemas s ON s.schema_id = t.schema_id \
                 WHERE s.name = @P1 AND t.name = @P2",
                &[&schema, &target.table],
            ).await.map_err(map_query_err)?.into_first_result().await.map_err(map_query_err)?;
            let row = rows.first().ok_or_else(||
                DatabaseError::QueryError("OBJECT_IDENTITY_UNSUPPORTED: 不是可证明身份的基础表".into()))?;
            let string = |index| row.get::<&str, _>(index).map(str::to_owned).ok_or_else(||
                DatabaseError::QueryError("OBJECT_IDENTITY_UNSUPPORTED: 对象目录不完整".into()));
            let database = string(0)?;
            if database != target.database {
                return Err(DatabaseError::QueryError("OBJECT_IDENTITY_UNSUPPORTED: 当前会话数据库不符".into()));
            }
            if row.get::<i32, _>(13) != Some(1) {
                return Err(DatabaseError::QueryError("OBJECT_IDENTITY_UNSUPPORTED: 需要完整对象目录查看权限".into()));
            }
            let mut write_reasons: Vec<String> = Vec::new();
            if row.get::<bool, _>(7) != Some(false) || row.get::<u8, _>(8) != Some(0)
                || row.get::<bool, _>(9) != Some(false) || row.get::<bool, _>(10) != Some(false) {
                write_reasons.push("OBJECT_WRITE_SEMANTICS_UNSUPPORTED".into());
            }
            if row.get::<i32, _>(11) != Some(0) { write_reasons.push("TRIGGERS_UNSUPPORTED".into()); }
            if row.get::<i32, _>(12) != Some(0) { write_reasons.push("ROW_SECURITY_UNSUPPORTED".into()); }
            let allocations = client.query(
                "SELECT CAST(partition_id AS varchar(30)), CAST(hobt_id AS varchar(30)) \
                 FROM sys.partitions WHERE object_id = OBJECT_ID(@P1) AND index_id IN (0, 1) ORDER BY partition_number",
                &[&quoted],
            ).await.map_err(map_query_err)?.into_first_result().await.map_err(map_query_err)?;
            let allocations: Vec<(String, String)> = allocations.iter().map(|r| {
                Ok((r.get::<&str, _>(0).ok_or_else(|| DatabaseError::QueryError("分配身份不可用".into()))?.into(),
                    r.get::<&str, _>(1).ok_or_else(|| DatabaseError::QueryError("分配身份不可用".into()))?.into()))
            }).collect::<Result<_, DatabaseError>>()?;
            if allocations.is_empty() { return Err(DatabaseError::QueryError("OBJECT_IDENTITY_UNSUPPORTED: 分配身份不可用".into())); }
            let identity = serde_json::to_string(&(string(1)?, string(4)?, string(5)?, string(6)?, allocations))
                .map_err(|_| DatabaseError::Unknown("对象身份编码失败".into()))?;
            let column_rows = client.query("SELECT c.COLUMN_NAME, CASE \
                    WHEN c.DATA_TYPE IN ('decimal', 'numeric') THEN c.DATA_TYPE + '(' + CAST(c.NUMERIC_PRECISION AS VARCHAR(3)) + ',' + CAST(c.NUMERIC_SCALE AS VARCHAR(3)) + ')' \
                    WHEN c.DATA_TYPE IN ('time', 'datetime2', 'datetimeoffset') THEN c.DATA_TYPE + '(' + CAST(c.DATETIME_PRECISION AS VARCHAR(3)) + ')' \
                    WHEN c.DATA_TYPE IN ('binary', 'varbinary', 'varchar', 'nvarchar', 'char', 'nchar') THEN c.DATA_TYPE + '(' + CASE WHEN c.CHARACTER_MAXIMUM_LENGTH = -1 THEN 'max' ELSE CAST(c.CHARACTER_MAXIMUM_LENGTH AS VARCHAR(10)) END + ')' \
                    ELSE c.DATA_TYPE END, c.IS_NULLABLE, c.COLUMN_DEFAULT, \
                    CASE WHEN kcu.COLUMN_NAME IS NOT NULL THEN 1 ELSE 0 END, \
                    CAST(ep.value AS NVARCHAR(4000)), \
                    CASE WHEN sc.is_computed = 1 THEN 'generated computed' \
                         WHEN sc.is_identity = 1 THEN 'identity' ELSE NULL END \
             FROM INFORMATION_SCHEMA.COLUMNS c \
             LEFT JOIN INFORMATION_SCHEMA.TABLE_CONSTRAINTS tc \
               ON tc.TABLE_NAME = c.TABLE_NAME AND tc.TABLE_SCHEMA = c.TABLE_SCHEMA \
              AND tc.CONSTRAINT_TYPE = 'PRIMARY KEY' \
             LEFT JOIN INFORMATION_SCHEMA.KEY_COLUMN_USAGE kcu \
               ON kcu.CONSTRAINT_NAME = tc.CONSTRAINT_NAME AND kcu.CONSTRAINT_SCHEMA = tc.CONSTRAINT_SCHEMA \
              AND kcu.TABLE_NAME = c.TABLE_NAME AND kcu.TABLE_SCHEMA = c.TABLE_SCHEMA \
              AND kcu.COLUMN_NAME = c.COLUMN_NAME \
             LEFT JOIN sys.objects st ON st.name = c.TABLE_NAME AND st.schema_id = SCHEMA_ID(c.TABLE_SCHEMA) \
             LEFT JOIN sys.columns sc ON sc.object_id = st.object_id AND sc.name = c.COLUMN_NAME \
             LEFT JOIN sys.extended_properties ep \
               ON ep.major_id = st.object_id AND ep.minor_id = sc.column_id AND ep.name = 'MS_Description' \
             WHERE c.TABLE_NAME = @P1 AND c.TABLE_SCHEMA = @P2 \
             ORDER BY c.ORDINAL_POSITION", &[&target.table, &schema])
                .await.map_err(map_query_err)?.into_first_result().await.map_err(map_query_err)?;
            let columns: Vec<ColumnInfo> = column_rows.iter().map(|r| ColumnInfo {
                name: r.get::<&str, _>(0).unwrap_or("").into(),
                data_type: r.get::<&str, _>(1).unwrap_or("").into(),
                nullable: r.get::<&str, _>(2).unwrap_or("YES") == "YES",
                default_value: r.get::<&str, _>(3).map(str::to_owned),
                is_primary_key: r.get::<i32, _>(4) == Some(1),
                comment: r.get::<&str, _>(5).map(str::to_owned),
                extra: r.get::<&str, _>(6).map(str::to_owned),
            }).collect();
            if columns.is_empty() || columns.iter().any(|c| c.name.is_empty() || c.data_type.is_empty()) {
                return Err(DatabaseError::QueryError("OBJECT_IDENTITY_UNSUPPORTED: 完整列目录不可用".into()));
            }
            Ok(TargetSnapshot { table: ResolvedTable { database, schema: Some(string(2)?),
                table: string(3)?, columns, object_kind: Some("table".into()) }, identity, write_reasons })
        }.await;
        self.finish_operation(client, result).await
    }
    async fn begin(&mut self) -> Result<(), DatabaseError> {
        self.execute("BEGIN TRAN").await.map(|_| ())
    }
    async fn commit(&mut self) -> Result<(), DatabaseError> {
        // COMMIT 的传输/协议错误原样返回，关闭连接不会把未知结果宣称为已回滚。
        self.execute("COMMIT TRAN").await.map(|_| ())
    }
    async fn rollback(&mut self) -> Result<(), DatabaseError> {
        self.execute("IF @@TRANCOUNT > 0 ROLLBACK TRAN")
            .await
            .map(|_| ())
    }
    async fn release(mut self: Box<Self>) -> Result<(), DatabaseError> {
        // 预算截断或失败已隔离的会话没有可归还的 socket，释放应安全结束。
        let Some(mut client) = self.client.take() else {
            return Ok(());
        };
        // 覆盖通过任意 SQL 打开的事务。临时对象和其他会话状态由物理关闭清除。
        client.execute("IF @@TRANCOUNT > 0 ROLLBACK TRAN; SET IMPLICIT_TRANSACTIONS OFF; SET TRANSACTION ISOLATION LEVEL READ COMMITTED; SET LOCK_TIMEOUT -1; SET ROWCOUNT 0", &[])
            .await.map_err(map_exec_err)?;
        client
            .close()
            .await
            .map_err(|e| DatabaseError::ConnectionError(e.to_string()))
    }
}

async fn finish_lease<T>(
    lease: Box<dyn SessionLease>,
    result: Result<T, DatabaseError>,
) -> Result<T, DatabaseError> {
    let release = lease.release().await;
    match result {
        Err(error) => Err(error),
        Ok(value) => release.map(|_| value),
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

async fn connect_client(
    config: &ConnectionConfig,
    database: Option<&str>,
) -> Result<TdsClient, DatabaseError> {
    // 与 JDBC encrypt=false 语义一致：关闭时直接声明不支持加密，避免先进行一次
    // 注定失败或超时的 TLS 握手。需要加密的服务器可在连接配置中显式开启。
    let encryption = if config.encrypt.unwrap_or(false) {
        tiberius::EncryptionLevel::Required
    } else {
        tiberius::EncryptionLevel::NotSupported
    };
    try_connect(config, database, Some(encryption)).await
}

pub async fn test_connection(config: &ConnectionConfig) -> Result<(), DatabaseError> {
    let mut client =
        connect_client(config, config.database.as_deref().filter(|d| !d.is_empty())).await?;
    client
        .simple_query("SELECT 1")
        .await
        .map_err(|e| DatabaseError::ConnectionError(e.to_string()))?
        .into_results()
        .await
        .map_err(|e| DatabaseError::ConnectionError(e.to_string()))?;
    client
        .close()
        .await
        .map_err(|e| DatabaseError::ConnectionError(e.to_string()))
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

    fn session_capabilities(&self) -> SessionCapabilities {
        SessionCapabilities::without_cancellation(CANCELLATION_UNSUPPORTED)
    }

    async fn lease_session(
        &self,
        database: Option<&str>,
    ) -> Result<Box<dyn SessionLease>, DatabaseError> {
        let client = self.client_for(database).await?;
        Ok(Box::new(MsSqlSessionLease {
            id: uuid::Uuid::new_v4().to_string(),
            client: Some(client),
        }))
    }

    async fn query_typed(
        &self,
        sql: &str,
        params: &[TypedValue],
        database: Option<&str>,
    ) -> Result<TypedQueryResult, DatabaseError> {
        let mut lease = self.lease_session(database).await?;
        let result = lease.query_typed(sql, params).await;
        finish_lease(lease, result).await
    }

    async fn execute_bound(
        &self,
        sql: &str,
        params: &[TypedValue],
        database: Option<&str>,
    ) -> Result<u64, DatabaseError> {
        let mut lease = self.lease_session(database).await?;
        let result = lease.execute_bound(sql, params).await;
        finish_lease(lease, result).await
    }

    async fn query(
        &self,
        sql: &str,
        database: Option<&str>,
        _query_id: Option<u64>,
    ) -> Result<QueryResult, DatabaseError> {
        let mut lease = self.lease_session(database).await?;
        let result = lease.query(sql).await;
        finish_lease(lease, result).await
    }

    async fn execute(
        &self,
        sql: &str,
        database: Option<&str>,
        _query_id: Option<u64>,
    ) -> Result<QueryResult, DatabaseError> {
        let mut lease = self.lease_session(database).await?;
        let result = lease.execute(sql).await;
        finish_lease(lease, result).await
    }

    async fn cancel_query(&self, _query_id: u64) -> Result<(), DatabaseError> {
        Err(DatabaseError::Unknown(CANCELLATION_UNSUPPORTED.into()))
    }

    async fn get_databases(&self) -> Result<Vec<String>, DatabaseError> {
        // MCP 等只读调用只暴露实际可进入的数据库。
        let dbs = self
            .get_database_infos()
            .await?
            .into_iter()
            .filter(|database| database.accessible)
            .map(|database| database.name)
            .collect();
        Ok(dbs)
    }

    async fn get_database_infos(&self) -> Result<Vec<DatabaseInfo>, DatabaseError> {
        let mut client = self.client_for(None).await?;
        let stream = client
            .simple_query(
                "SELECT name, CASE WHEN HAS_DBACCESS(name) = 1 THEN 1 ELSE 0 END \
             FROM sys.databases WHERE state = 0 ORDER BY name",
            )
            .await
            .map_err(map_query_err)?;
        let rows = stream.into_first_result().await.map_err(map_query_err)?;

        Ok(rows
            .iter()
            .map(|row| DatabaseInfo {
                name: row.get::<&str, _>(0).unwrap_or("").to_string(),
                accessible: row.get::<i32, _>(1).unwrap_or(0) == 1,
            })
            .collect())
    }

    async fn get_tables(&self, database: Option<&str>) -> Result<Vec<TableInfo>, DatabaseError> {
        let mut client = self.client_for(database).await?;
        let stream = client
            .simple_query(
                "SELECT t.name, SCHEMA_NAME(t.schema_id), COALESCE(SUM(p.rows), 0) \
             FROM sys.tables t \
             LEFT JOIN sys.partitions p ON p.object_id = t.object_id AND p.index_id IN (0,1) \
             WHERE t.is_ms_shipped = 0 GROUP BY t.name, t.schema_id ORDER BY SCHEMA_NAME(t.schema_id), t.name",
            )
            .await
            .map_err(map_query_err)?;
        let rows = stream.into_first_result().await.map_err(map_query_err)?;

        let tables = rows
            .iter()
            .map(|row| {
                let name = mssql_metadata_string(row, 0)?;
                let schema = mssql_metadata_string(row, 1)?;
                Ok(TableInfo {
                    name,
                    schema: Some(schema),
                    row_count: row.try_get::<i64, _>(2).map_err(map_query_err)?,
                    engine: None,
                    comment: None,
                })
            })
            .collect::<Result<Vec<_>, DatabaseError>>()?;
        Ok(tables)
    }

    async fn get_views(&self, database: Option<&str>) -> Result<Vec<TableInfo>, DatabaseError> {
        let mut client = self.client_for(database).await?;
        let stream = client
            .simple_query(
                "SELECT v.name, SCHEMA_NAME(v.schema_id) \
             FROM sys.views v WHERE v.is_ms_shipped = 0 ORDER BY SCHEMA_NAME(v.schema_id), v.name",
            )
            .await
            .map_err(map_query_err)?;
        let rows = stream.into_first_result().await.map_err(map_query_err)?;

        Ok(rows
            .iter()
            .map(|row| {
                let name = mssql_metadata_string(row, 0)?;
                let schema = mssql_metadata_string(row, 1)?;
                Ok(TableInfo {
                    name,
                    schema: Some(schema),
                    row_count: None,
                    engine: None,
                    comment: None,
                })
            })
            .collect::<Result<Vec<_>, DatabaseError>>()?)
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
        let schema = target
            .schema
            .as_deref()
            .ok_or_else(|| DatabaseError::QueryError("视图目标缺少 schema".into()))?;
        let relation = format!(
            "[{}].[{}]",
            self.escape_identifier(schema),
            self.escape_identifier(&target.table)
        );
        let mut client = self.client_for(Some(&target.database)).await?;
        let rows = client
            .query(
                "SELECT OBJECT_DEFINITION(OBJECT_ID(@P1, 'V'))",
                &[&relation],
            )
            .await
            .map_err(map_query_err)?
            .into_first_result()
            .await
            .map_err(map_query_err)?;
        let row = rows
            .first()
            .ok_or_else(|| DatabaseError::QueryError("未找到目标视图".into()))?;
        mssql_metadata_string(row, 0)
    }

    async fn get_triggers(&self, database: Option<&str>) -> Result<Vec<TableInfo>, DatabaseError> {
        let mut client = self.client_for(database).await?;
        // parent_id = 0 的是数据库级 DDL 触发器，其余挂在表/视图上
        let stream = client
            .simple_query(
                "SELECT t.name, SCHEMA_NAME(o.schema_id), COALESCE(o.name, 'DATABASE') \
             FROM sys.triggers t LEFT JOIN sys.objects o ON t.parent_id = o.object_id \
             ORDER BY t.name",
            )
            .await
            .map_err(map_query_err)?;
        let rows = stream.into_first_result().await.map_err(map_query_err)?;

        Ok(rows
            .iter()
            .map(|row| {
                let name: &str = row.get(0).unwrap_or("");
                let schema: Option<&str> = row.get(1);
                let parent: &str = row.get(2).unwrap_or("");
                TableInfo {
                    name: name.to_string(),
                    schema: schema.map(|s| s.to_string()),
                    row_count: None,
                    engine: None,
                    comment: Some(format!("ON {}", parent)),
                }
            })
            .collect())
    }

    async fn get_trigger_definition(
        &self,
        database: Option<&str>,
        trigger: &str,
    ) -> Result<String, DatabaseError> {
        let mut client = self.client_for(database).await?;
        let stream = client.query(
            "SELECT TOP 1 OBJECT_DEFINITION(t.object_id) FROM sys.triggers t WHERE t.name = @P1",
            &[&trigger],
        ).await.map_err(map_query_err)?;
        let rows = stream.into_first_result().await.map_err(map_query_err)?;
        rows.first()
            .and_then(|r| r.get::<&str, _>(0).map(|s| s.to_string()))
            .ok_or_else(|| DatabaseError::Unknown(format!("触发器 {} 定义为空", trigger)))
    }

    async fn get_procedures(
        &self,
        database: Option<&str>,
    ) -> Result<Vec<TableInfo>, DatabaseError> {
        let mut client = self.client_for(database).await?;
        let stream = client
            .simple_query(
                "SELECT p.name, SCHEMA_NAME(p.schema_id) FROM sys.procedures p ORDER BY p.name",
            )
            .await
            .map_err(map_query_err)?;
        let rows = stream.into_first_result().await.map_err(map_query_err)?;

        Ok(rows
            .iter()
            .map(|row| {
                let name: &str = row.get(0).unwrap_or("");
                let schema: Option<&str> = row.get(1);
                TableInfo {
                    name: name.to_string(),
                    schema: schema.map(|s| s.to_string()),
                    row_count: None,
                    engine: None,
                    comment: None,
                }
            })
            .collect())
    }

    async fn get_procedure_definition(
        &self,
        database: Option<&str>,
        procedure: &str,
    ) -> Result<String, DatabaseError> {
        let mut client = self.client_for(database).await?;
        let stream = client.query(
            "SELECT TOP 1 OBJECT_DEFINITION(p.object_id) FROM sys.procedures p WHERE p.name = @P1",
            &[&procedure],
        ).await.map_err(map_query_err)?;
        let rows = stream.into_first_result().await.map_err(map_query_err)?;
        rows.first()
            .and_then(|r| r.get::<&str, _>(0).map(|s| s.to_string()))
            .ok_or_else(|| DatabaseError::Unknown(format!("存储过程 {} 定义为空", procedure)))
    }

    async fn get_functions(&self, database: Option<&str>) -> Result<Vec<TableInfo>, DatabaseError> {
        let mut client = self.client_for(database).await?;
        // FN = 标量函数, IF = 内联表值函数, TF = 多语句表值函数
        let stream = client
            .simple_query(
                "SELECT o.name, SCHEMA_NAME(o.schema_id), o.type \
             FROM sys.objects o WHERE o.type IN ('FN', 'IF', 'TF') ORDER BY o.name",
            )
            .await
            .map_err(map_query_err)?;
        let rows = stream.into_first_result().await.map_err(map_query_err)?;

        Ok(rows
            .iter()
            .map(|row| {
                let name: &str = row.get(0).unwrap_or("");
                let schema: Option<&str> = row.get(1);
                let kind: &str = row.get(2).unwrap_or("");
                TableInfo {
                    name: name.to_string(),
                    schema: schema.map(|s| s.to_string()),
                    row_count: None,
                    engine: Some(kind.to_string()),
                    comment: None,
                }
            })
            .collect())
    }

    async fn get_function_definition(
        &self,
        database: Option<&str>,
        function: &str,
    ) -> Result<String, DatabaseError> {
        let mut client = self.client_for(database).await?;
        let stream = client
            .query(
                "SELECT TOP 1 OBJECT_DEFINITION(o.object_id) FROM sys.objects o \
             WHERE o.name = @P1 AND o.type IN ('FN', 'IF', 'TF')",
                &[&function],
            )
            .await
            .map_err(map_query_err)?;
        let rows = stream.into_first_result().await.map_err(map_query_err)?;
        rows.first()
            .and_then(|r| r.get::<&str, _>(0).map(|s| s.to_string()))
            .ok_or_else(|| DatabaseError::Unknown(format!("函数 {} 定义为空", function)))
    }

    async fn get_users(&self, database: Option<&str>) -> Result<Vec<DbUserInfo>, DatabaseError> {
        let mut client = self.client_for(database).await?;
        // 服务器级登录名：SQL 登录 + Windows 登录，排除系统内置
        let stream = client
            .simple_query(
                "SELECT name, type_desc FROM sys.server_principals \
             WHERE type IN ('S', 'U') AND name NOT LIKE '##%' AND name <> 'sa' \
             ORDER BY name",
            )
            .await
            .map_err(map_query_err)?;
        let rows = stream.into_first_result().await.map_err(map_query_err)?;
        Ok(rows
            .iter()
            .map(|row| {
                let name: &str = row.get(0).unwrap_or("");
                let type_desc: &str = row.get(1).unwrap_or("");
                DbUserInfo {
                    name: name.to_string(),
                    host: None,
                    note: Some(if type_desc.contains("SQL") {
                        "SQL 登录".to_string()
                    } else {
                        "Windows 登录".to_string()
                    }),
                }
            })
            .collect())
    }

    async fn get_current_user(
        &self,
        database: Option<&str>,
    ) -> Result<CurrentUserInfo, DatabaseError> {
        let mut client = self.client_for(database).await?;
        let stream = client
            .simple_query("SELECT SUSER_NAME()")
            .await
            .map_err(map_query_err)?;
        let rows = stream.into_first_result().await.map_err(map_query_err)?;
        let user = rows
            .first()
            .and_then(|r| r.get::<&str, _>(0).map(|s| s.to_string()))
            .unwrap_or_else(|| "unknown".to_string());

        // 服务器级权限 + 服务器角色成员
        let stream = client
            .simple_query(
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
            )
            .await
            .map_err(map_query_err)?;
        let rows = stream.into_first_result().await.map_err(map_query_err)?;
        let grants = rows
            .iter()
            .filter_map(|r| r.get::<&str, _>(0).map(|s| s.to_string()))
            .collect();
        Ok(CurrentUserInfo { user, grants })
    }

    async fn resolve_table(
        &self,
        database: &str,
        table_parts: &[String],
    ) -> Result<ResolvedTable, DatabaseError> {
        let (target_database, relation_parts) = match table_parts {
            [_] | [_, _] => (database, table_parts),
            [db, _, _] => (db.as_str(), &table_parts[1..]),
            _ => {
                return Err(DatabaseError::QueryError(
                    "SQL Server 表名需要一至三段标识符".into(),
                ))
            }
        };
        if table_parts.iter().any(String::is_empty) {
            return Err(DatabaseError::QueryError("表名各段不能为空".into()));
        }
        let relation = relation_parts
            .iter()
            .map(|part| format!("[{}]", self.escape_identifier(part)))
            .collect::<Vec<_>>()
            .join(".");
        let (database, schema, table, object_kind) = {
            let mut client = self.client_for(Some(target_database)).await?;
            let rows = client
                .query(
                    "SELECT DB_NAME(), s.name, t.name, t.type FROM sys.objects t \
                 JOIN sys.schemas s ON s.schema_id = t.schema_id \
                 WHERE t.object_id = OBJECT_ID(@P1) AND t.type IN ('U', 'V')",
                    &[&relation],
                )
                .await
                .map_err(map_query_err)?
                .into_first_result()
                .await
                .map_err(map_query_err)?;
            let row = rows
                .first()
                .ok_or_else(|| DatabaseError::QueryError("未找到目标表".into()))?;
            let database = row
                .get::<&str, _>(0)
                .ok_or_else(|| DatabaseError::QueryError("数据库身份不可用".into()))?
                .to_string();
            let schema = row
                .get::<&str, _>(1)
                .ok_or_else(|| DatabaseError::QueryError("表 schema 不可用".into()))?
                .to_string();
            let table = row
                .get::<&str, _>(2)
                .ok_or_else(|| DatabaseError::QueryError("表名不可用".into()))?
                .to_string();
            let object_kind = match row.get::<&str, _>(3).map(str::trim) {
                Some("U") => "table",
                Some("V") => "view",
                _ => "unknown",
            };
            (database, schema, table, object_kind)
        };
        // 身份查询完成后关闭该短连接；列信息通过另一个独立元数据连接读取。
        let columns = self
            .columns_for_schema(Some(&database), &schema, &table)
            .await?;
        if columns.is_empty() {
            return Err(DatabaseError::QueryError("目标表列信息不可用".into()));
        }
        Ok(ResolvedTable {
            database,
            schema: Some(schema),
            table,
            columns,
            object_kind: Some(object_kind.into()),
        })
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
        let schema = target
            .schema
            .as_deref()
            .ok_or_else(|| DatabaseError::QueryError("SQL Server 元数据目标缺少 schema".into()))?;
        let relation = format!(
            "[{}].[{}]",
            self.escape_identifier(schema),
            self.escape_identifier(&target.table)
        );
        let mut client = self.client_for(Some(&target.database)).await?;
        let rows = client.query(
            "SELECT i.name, i.is_unique, i.is_primary_key, c.name, \
                    ic.is_included_column, ic.is_descending_key, i.filter_definition \
             FROM sys.indexes i \
             LEFT JOIN sys.index_columns ic ON ic.object_id = i.object_id AND ic.index_id = i.index_id \
               AND (ic.key_ordinal > 0 OR ic.is_included_column = 1) \
             LEFT JOIN sys.columns c ON c.object_id = ic.object_id AND c.column_id = ic.column_id \
             WHERE i.object_id = OBJECT_ID(@P1) AND i.type > 0 AND i.is_hypothetical = 0 \
             ORDER BY i.index_id, ic.is_included_column, ic.key_ordinal, ic.index_column_id",
            &[&relation],
        ).await.map_err(map_query_err)?.into_first_result().await.map_err(map_query_err)?;
        let mut indexes: Vec<IndexInfo> = Vec::new();
        for row in rows {
            let name = mssql_metadata_string(&row, 0)?;
            if indexes.last().is_none_or(|index| index.name != name) {
                indexes.push(IndexInfo {
                    name,
                    columns: Vec::new(),
                    is_unique: row
                        .try_get::<bool, _>(1)
                        .map_err(map_query_err)?
                        .ok_or_else(|| DatabaseError::QueryError("索引唯一标记缺失".into()))?,
                    is_primary: row
                        .try_get::<bool, _>(2)
                        .map_err(map_query_err)?
                        .ok_or_else(|| DatabaseError::QueryError("索引主键标记缺失".into()))?,
                    key_parts: Vec::new(),
                    included_columns: Vec::new(),
                    definition: None,
                    predicate: row
                        .try_get::<&str, _>(6)
                        .map_err(map_query_err)?
                        .map(str::to_owned),
                });
            }
            let index = indexes
                .last_mut()
                .ok_or_else(|| DatabaseError::QueryError("索引分组状态无效".into()))?;
            if let Some(column) = row.try_get::<&str, _>(3).map_err(map_query_err)? {
                let included = row
                    .try_get::<bool, _>(4)
                    .map_err(map_query_err)?
                    .ok_or_else(|| DatabaseError::QueryError("索引包含列标记缺失".into()))?;
                if included {
                    index.included_columns.push(column.into());
                } else {
                    index.columns.push(column.into());
                    index.key_parts.push(IndexKeyPart {
                        column: Some(column.into()),
                        expression: None,
                        descending: row
                            .try_get::<bool, _>(5)
                            .map_err(map_query_err)?
                            .ok_or_else(|| DatabaseError::QueryError("索引排序标记缺失".into()))?,
                    });
                }
            }
        }
        Ok(indexes)
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
        let schema = target
            .schema
            .as_deref()
            .ok_or_else(|| DatabaseError::QueryError("SQL Server 元数据目标缺少 schema".into()))?;
        let relation = format!(
            "[{}].[{}]",
            self.escape_identifier(schema),
            self.escape_identifier(&target.table)
        );
        let mut client = self.client_for(Some(&target.database)).await?;
        let rows = client.query(
            "SELECT fk.name, pc.name, rt.name, rc.name, \
                    fk.update_referential_action_desc, fk.delete_referential_action_desc, rs.name \
             FROM sys.foreign_keys fk \
             JOIN sys.foreign_key_columns fkc ON fkc.constraint_object_id = fk.object_id \
             JOIN sys.columns pc ON pc.object_id = fk.parent_object_id AND pc.column_id = fkc.parent_column_id \
             JOIN sys.tables rt ON rt.object_id = fk.referenced_object_id \
             JOIN sys.schemas rs ON rs.schema_id = rt.schema_id \
             JOIN sys.columns rc ON rc.object_id = rt.object_id AND rc.column_id = fkc.referenced_column_id \
             WHERE fk.parent_object_id = OBJECT_ID(@P1) \
             ORDER BY fk.object_id, fkc.constraint_column_id",
            &[&relation],
        ).await.map_err(map_query_err)?.into_first_result().await.map_err(map_query_err)?;
        let mut foreign_keys: Vec<ForeignKeyInfo> = Vec::new();
        for row in rows {
            let name = mssql_metadata_string(&row, 0)?;
            if foreign_keys.last().is_none_or(|key| key.name != name) {
                foreign_keys.push(ForeignKeyInfo {
                    name,
                    ref_database: Some(target.database.clone()),
                    columns: Vec::new(),
                    ref_table: mssql_metadata_string(&row, 2)?,
                    ref_schema: Some(mssql_metadata_string(&row, 6)?),
                    ref_columns: Vec::new(),
                    on_update: Some(mssql_metadata_string(&row, 4)?.replace('_', " ")),
                    on_delete: Some(mssql_metadata_string(&row, 5)?.replace('_', " ")),
                });
            }
            let key = foreign_keys
                .last_mut()
                .ok_or_else(|| DatabaseError::QueryError("外键分组状态无效".into()))?;
            key.columns.push(mssql_metadata_string(&row, 1)?);
            key.ref_columns.push(mssql_metadata_string(&row, 3)?);
        }
        Ok(foreign_keys)
    }

    async fn execute_batch(
        &self,
        sqls: Vec<String>,
        database: Option<&str>,
    ) -> Result<u64, DatabaseError> {
        let mut lease = self.lease_session(database).await?;
        let result = async {
            lease.begin().await?;
            let mut total = 0;
            for sql in &sqls {
                match lease.execute(sql).await {
                    Ok(result) => total += result.rows_affected.unwrap_or(0),
                    Err(error) => {
                        // 连接已隔离时 rollback 会失败，原始错误仍保留；关闭 socket 不代表已确认回滚。
                        let _ = lease.rollback().await;
                        return Err(error);
                    }
                }
            }
            lease.commit().await?;
            Ok(total)
        }
        .await;
        finish_lease(lease, result).await
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
        let schema = target.schema.as_deref().ok_or_else(|| {
            DatabaseError::QueryError("EXPORT_OBJECT_UNSUPPORTED: 导出表缺少明确 schema".into())
        })?;
        let quoted = format!(
            "[{}].[{}]",
            self.escape_identifier(schema),
            self.escape_identifier(&target.table)
        );
        let mut out = String::from("-- SQLink 表定义\n\n");

        if include_drop {
            out.push_str(&format!("DROP TABLE IF EXISTS {quoted};\n\n"));
        }

        // SQL Server 没有 SHOW CREATE TABLE，用列信息重建近似定义
        let cols = &target.columns;
        if cols.is_empty() {
            return Err(DatabaseError::Unknown(format!(
                "表 {} 不存在或无列",
                target.table
            )));
        }
        let mut defs: Vec<String> = cols
            .iter()
            .map(|c| {
                let mut d = format!("[{}] {}", self.escape_identifier(&c.name), c.data_type);
                if let Some(def) = &c.default_value {
                    d.push_str(&format!(" DEFAULT {}", def));
                }
                d.push_str(if c.nullable { " NULL" } else { " NOT NULL" });
                d
            })
            .collect();
        let pk: Vec<String> = cols
            .iter()
            .filter(|c| c.is_primary_key)
            .map(|c| format!("[{}]", self.escape_identifier(&c.name)))
            .collect();
        if !pk.is_empty() {
            defs.push(format!("PRIMARY KEY ({})", pk.join(", ")));
        }
        out.push_str(&format!(
            "CREATE TABLE {} (\n  {}\n);\n\n",
            quoted,
            defs.join(",\n  ")
        ));

        let mut rows_written = 0u64;
        if include_data {
            rows_written = crate::export::append_dump_data_for(self, target, &mut out).await?;
        }

        Ok((out, rows_written))
    }

    async fn disconnect(&self) -> Result<(), DatabaseError> {
        self.connected.store(false, Ordering::Relaxed);
        Ok(())
    }

    fn escape_identifier(&self, identifier: &str) -> String {
        identifier.replace(']', "]]")
    }
}

async fn mssql_query_typed(
    client: &mut TdsClient,
    query: tiberius::Query<'_>,
) -> Result<MsSqlQueryOutput<TypedQueryResult>, DatabaseError> {
    let start = std::time::Instant::now();
    let stream = query.query(client).await.map_err(map_query_err)?;
    mssql_query_stream(stream, start, true).await
}

async fn mssql_query(
    client: &mut TdsClient,
    sql: &str,
) -> Result<MsSqlQueryOutput<QueryResult>, DatabaseError> {
    let start = std::time::Instant::now();
    let stream = client.simple_query(sql).await.map_err(map_query_err)?;
    let output = mssql_query_stream(stream, start, false).await?;
    Ok(MsSqlQueryOutput {
        value: output.value.result,
        stream_complete: output.stream_complete,
    })
}

struct MsSqlQueryOutput<T> {
    value: T,
    stream_complete: bool,
}

enum MsSqlResultItem<R> {
    Metadata {
        columns: Vec<ColumnMetadata>,
        result_index: usize,
    },
    Row {
        row: R,
        result_index: usize,
    },
}

async fn mssql_query_stream(
    stream: tiberius::QueryStream<'_>,
    start: std::time::Instant,
    typed: bool,
) -> Result<MsSqlQueryOutput<TypedQueryResult>, DatabaseError> {
    let stream = stream.map(|item| {
        item.map_err(map_query_err).map(|item| match item {
            tiberius::QueryItem::Metadata(metadata) => MsSqlResultItem::Metadata {
                result_index: metadata.result_index(),
                columns: metadata
                    .columns()
                    .iter()
                    .map(|column| ColumnMetadata {
                        name: column.name().to_string(),
                        data_type: format!("{:?}", column.column_type()),
                        unified_type: "Unknown".into(),
                        nullable: true,
                        length: None,
                        precision: None,
                        scale: None,
                    })
                    .collect(),
            },
            tiberius::QueryItem::Row(row) => MsSqlResultItem::Row {
                result_index: row.result_index(),
                row,
            },
        })
    });
    mssql_collect_first_result(stream, start, typed, mssql_decode_budgeted_row).await
}

async fn mssql_collect_first_result<S, R, F>(
    mut stream: S,
    start: std::time::Instant,
    typed: bool,
    mut decode: F,
) -> Result<MsSqlQueryOutput<TypedQueryResult>, DatabaseError>
where
    S: Stream<Item = Result<MsSqlResultItem<R>, DatabaseError>> + Unpin,
    F: FnMut(
        R,
        &QueryBudget,
        bool,
    ) -> Result<Option<(RowData, Vec<Option<TypedValue>>)>, DatabaseError>,
{
    let mut columns = Vec::new();
    let mut budget = QueryBudget::new(&columns)?;
    let mut rows = Vec::new();
    let mut typed_rows = Vec::new();
    let mut truncated = false;
    let mut stream_complete = false;

    loop {
        match stream.try_next().await? {
            None => {
                stream_complete = true;
                break;
            }
            Some(MsSqlResultItem::Metadata {
                columns: first_columns,
                result_index: 0,
            }) => {
                // 从 Metadata 保留列定义，零行 SELECT 仍有完整网格结构。
                budget = QueryBudget::new(&first_columns)?;
                columns = first_columns;
            }
            Some(MsSqlResultItem::Metadata { .. })
            | Some(MsSqlResultItem::Row {
                result_index: 1.., ..
            }) => {
                // 只展示首结果集；后续结果不 drain，明确标记提前结束并隔离连接。
                truncated = true;
                break;
            }
            Some(MsSqlResultItem::Row { row, .. }) => {
                // 上限后最多探测一行，不转换、不继续读取剩余响应。
                if budget.row_limit_reached() {
                    truncated = true;
                    break;
                }
                let Some((row, typed_row)) = decode(row, &budget, typed)? else {
                    truncated = true;
                    break;
                };
                if !budget.try_add_row(&row, typed.then_some(typed_row.as_slice()))? {
                    truncated = true;
                    break;
                }
                rows.push(row);
                if typed {
                    typed_rows.push(typed_row);
                }
            }
        }
    }
    Ok(MsSqlQueryOutput {
        value: TypedQueryResult {
            result: QueryResult {
                columns,
                total_count: Some(rows.len()),
                rows,
                execution_time_ms: start.elapsed().as_millis() as u64,
                rows_affected: None,
                last_insert_id: None,
                truncated,
            },
            typed_rows,
        },
        stream_complete,
    })
}

fn mssql_raw_cell_length(data: &tiberius::ColumnData<'_>) -> usize {
    use tiberius::ColumnData as C;
    match data {
        C::String(value) => value.as_ref().map_or(0, |value| value.len()),
        C::Binary(value) => value.as_ref().map_or(0, |value| value.len()),
        C::Xml(value) => value.as_ref().map_or(0, |value| {
            let text: &str = value.as_ref().as_ref();
            text.len()
        }),
        C::U8(value) => value.as_ref().map_or(0, |_| 1),
        C::Bit(value) => value.as_ref().map_or(0, |_| 1),
        C::I16(value) => value.as_ref().map_or(0, |_| 2),
        C::I32(value) => value.as_ref().map_or(0, |_| 4),
        C::I64(value) => value.as_ref().map_or(0, |_| 8),
        C::F32(value) => value.as_ref().map_or(0, |_| 4),
        C::F64(value) => value.as_ref().map_or(0, |_| 8),
        C::Guid(value) => value.as_ref().map_or(0, |_| 16),
        C::Numeric(value) => value.as_ref().map_or(0, |_| 17),
        C::DateTime(value) => value.as_ref().map_or(0, |_| 8),
        C::SmallDateTime(value) => value.as_ref().map_or(0, |_| 4),
        C::Date(value) => value.as_ref().map_or(0, |_| 3),
        C::Time(value) => value.as_ref().map_or(0, |_| 5),
        C::DateTime2(value) => value.as_ref().map_or(0, |_| 8),
        C::DateTimeOffset(value) => value.as_ref().map_or(0, |_| 10),
    }
}

fn mssql_decode_budgeted_row(
    row: tiberius::Row,
    budget: &QueryBudget,
    typed: bool,
) -> Result<Option<(RowData, Vec<Option<TypedValue>>)>, DatabaseError> {
    // Tiberius 在交付 Row 前已解码整行；公开 API 无法限制单行 wire 解码的瞬时分配。
    // 在 JSON、十六进制和 base64 转换前借用检查，避免超大值再次分配和膨胀。
    if !budget.allows_raw_row(row.cells().map(|(_, value)| mssql_raw_cell_length(value))) {
        return Ok(None);
    }
    let fallback: Vec<_> = row
        .columns()
        .iter()
        .enumerate()
        .map(|(index, column)| mssql_value_to_json(&row, index, &column.column_type()))
        .collect();
    if !typed {
        return Ok(Some((
            RowData {
                values: fallback.into_iter().map(Some).collect(),
            },
            Vec::new(),
        )));
    }
    let types: Vec<_> = row
        .columns()
        .iter()
        .map(|column| column.column_type())
        .collect();
    let typed_row: Vec<_> = row
        .into_iter()
        .zip(types)
        .map(|(value, ty)| mssql_typed_value(value, ty))
        .collect();
    let values = typed_row
        .iter()
        .zip(fallback)
        .map(|(value, fallback)| {
            Some(
                value
                    .as_ref()
                    .map(TypedValue::display_json)
                    .unwrap_or(fallback),
            )
        })
        .collect();
    Ok(Some((RowData { values }, typed_row)))
}

async fn mssql_execute(client: &mut TdsClient, sql: &str) -> Result<QueryResult, DatabaseError> {
    let start = std::time::Instant::now();
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

fn mssql_type_error(message: &str) -> DatabaseError {
    DatabaseError::TypeError(message.into())
}

/// 仅为可信后端调用拒绝显式分号批语句；单条 DML 由调用方构造保证。
/// SQL Server 允许省略分号，本检查不充当任意 SQL 的语法或权限沙箱。
fn mssql_single_statement(sql: &str) -> Result<(), DatabaseError> {
    let mut splitter = crate::database::dump::SqlSplitter::new(false);
    let mut input = std::io::BufReader::new(sql.as_bytes());
    let mut count = 0;
    while splitter
        .next_statement(&mut input)
        .map_err(|_| mssql_type_error("不能检查 SQL Server 单条语句"))?
        .is_some()
    {
        count += 1;
        if count > 1 {
            return Err(mssql_type_error("类型化执行只接受单条 SQL 语句"));
        }
    }
    if count != 1 {
        return Err(mssql_type_error("类型化执行需要一条 SQL 语句"));
    }
    Ok(())
}

struct MsParam(tiberius::ColumnData<'static>);
impl<'a> tiberius::IntoSql<'a> for MsParam {
    fn into_sql(self) -> tiberius::ColumnData<'a> {
        self.0
    }
}

fn mssql_numeric(value: &str) -> Result<tiberius::numeric::Numeric, DatabaseError> {
    let scale = value.split_once('.').map(|(_, f)| f.len()).unwrap_or(0);
    let coefficient = value
        .replace('.', "")
        .parse::<i128>()
        .map_err(|_| mssql_type_error("SQL Server decimal 超出原生 i128 范围"))?;
    // Numeric::new_with_scale 在 scale>=38 时会 panic，且 TDS precision 最大为 38。
    if scale >= 38 {
        return Err(mssql_type_error(
            "SQL Server 原生 decimal 仅支持 scale 0..37",
        ));
    }
    let numeric = tiberius::numeric::Numeric::new_with_scale(coefficient, scale as u8);
    if numeric.precision() > 38 {
        return Err(mssql_type_error("SQL Server decimal 精度不能超过 38"));
    }
    Ok(numeric)
}

fn mssql_numeric_string(value: tiberius::numeric::Numeric) -> String {
    let scale = value.scale() as usize;
    let coefficient = value.value();
    let digits = coefficient.unsigned_abs().to_string();
    let sign = if coefficient < 0 { "-" } else { "" };
    if scale == 0 {
        return format!("{sign}{digits}");
    }
    let padded = format!(
        "{}{}",
        "0".repeat((scale + 1).saturating_sub(digits.len())),
        digits
    );
    let split = padded.len() - scale;
    format!("{sign}{}.{}", &padded[..split], &padded[split..])
}

fn mssql_date(value: chrono::NaiveDate) -> tiberius::time::Date {
    let epoch = chrono::NaiveDate::from_ymd_opt(1, 1, 1).unwrap();
    tiberius::time::Date::new(value.signed_duration_since(epoch).num_days() as u32)
}

fn mssql_time(value: chrono::NaiveTime, precision: u8) -> tiberius::time::Time {
    use chrono::Timelike;
    let units = 10u64.pow(precision as u32);
    tiberius::time::Time::new(
        value.num_seconds_from_midnight() as u64 * units
            + value.nanosecond() as u64 / 10u64.pow(9 - precision as u32),
        precision,
    )
}

fn mssql_bound_query<'a>(
    sql: &'a str,
    params: &[TypedValue],
) -> Result<tiberius::Query<'a>, DatabaseError> {
    let mut query = tiberius::Query::new(sql);
    for data in mssql_bound_params(params)? {
        query.bind(MsParam(data));
    }
    Ok(query)
}

fn mssql_bound_params(
    params: &[TypedValue],
) -> Result<Vec<tiberius::ColumnData<'static>>, DatabaseError> {
    use tiberius::ColumnData as C;
    let mut bound = Vec::with_capacity(params.len());
    for value in params {
        let data = match value {
            // TDS 仍发送原生 NULL 标记。NVARCHAR NULL 可向白名单标量转换，
            // 避免 INT NULL 在 date/time/datetime2 目标上的不兼容类型错误。
            TypedValue::Null => C::String(None),
            TypedValue::Text(v) => C::String(Some(v.clone().into())),
            TypedValue::Boolean(v) => C::Bit(Some(*v)),
            TypedValue::Integer(v) => C::I64(Some(
                v.parse().map_err(|_| mssql_type_error("int64 格式无效"))?,
            )),
            TypedValue::UnsignedInteger(v) => {
                let n: u64 = v.parse().map_err(|_| mssql_type_error("uint64 格式无效"))?;
                if let Ok(n) = i64::try_from(n) {
                    C::I64(Some(n))
                } else {
                    C::Numeric(Some(mssql_numeric(v)?))
                }
            }
            TypedValue::Decimal(v) => C::Numeric(Some(mssql_numeric(v)?)),
            TypedValue::Float(v) => C::F64(Some(*v)),
            TypedValue::Binary(_) => C::Binary(Some(value.binary_bytes()?.into())),
            TypedValue::Date(v) => C::Date(Some(mssql_date(
                chrono::NaiveDate::parse_from_str(v, "%Y-%m-%d")
                    .map_err(|_| mssql_type_error("日期无效"))?,
            ))),
            TypedValue::Time(v) if v.precision <= 7 => C::Time(Some(mssql_time(
                chrono::NaiveTime::parse_from_str(&v.value, "%H:%M:%S%.f")
                    .map_err(|_| mssql_type_error("时间无效"))?,
                v.precision,
            ))),
            TypedValue::Timestamp(v) if v.precision <= 7 && v.timezone == TimezoneKind::None => {
                let dt = chrono::NaiveDateTime::parse_from_str(&v.value, "%Y-%m-%dT%H:%M:%S%.f")
                    .map_err(|_| mssql_type_error("时间戳无效"))?;
                C::DateTime2(Some(tiberius::time::DateTime2::new(
                    mssql_date(dt.date()),
                    mssql_time(dt.time(), v.precision),
                )))
            }
            // 当前驱动 chrono 的 offset 编码与解码约定不一致，禁止冒充无损。
            TypedValue::Timestamp(v) if v.timezone == TimezoneKind::Offset => {
                return Err(mssql_type_error(
                    "SQL Server 驱动暂不支持无损绑定带时区时间戳",
                ))
            }
            _ => return Err(mssql_type_error("SQL Server 参数精度不能无损编码")),
        };
        bound.push(data);
    }
    Ok(bound)
}

fn mssql_time_string(value: tiberius::time::Time) -> Option<String> {
    let precision = value.scale();
    if precision > 7 {
        return None;
    }
    let units = 10u64.pow(precision as u32);
    let seconds = value.increments() / units;
    if seconds >= 86400 {
        return None;
    }
    let mut text = format!(
        "{:02}:{:02}:{:02}",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60
    );
    if precision > 0 {
        text.push_str(&format!(
            ".{:0width$}",
            value.increments() % units,
            width = precision as usize
        ));
    }
    Some(text)
}

fn mssql_date_string(value: tiberius::time::Date) -> Option<String> {
    chrono::NaiveDate::from_ymd_opt(1, 1, 1)?
        .checked_add_signed(chrono::Duration::days(value.days() as i64))
        .map(|v| v.format("%Y-%m-%d").to_string())
}

fn mssql_typed_value(data: tiberius::ColumnData<'static>, ty: ColumnType) -> Option<TypedValue> {
    use tiberius::ColumnData as C;
    macro_rules! nullable {
        ($v:expr, $f:expr) => {
            match $v {
                Some(v) => ($f)(v),
                None => Some(TypedValue::Null),
            }
        };
    }
    let value = match data {
        C::U8(v) => nullable!(v, |v: u8| Some(TypedValue::UnsignedInteger(v.to_string()))),
        C::I16(v) => nullable!(v, |v: i16| Some(TypedValue::Integer(v.to_string()))),
        C::I32(v) => nullable!(v, |v: i32| Some(TypedValue::Integer(v.to_string()))),
        C::I64(v) => nullable!(v, |v: i64| Some(TypedValue::Integer(v.to_string()))),
        C::Bit(v) => nullable!(v, |v| Some(TypedValue::Boolean(v))),
        C::F32(v) => nullable!(v, |v| Some(TypedValue::Float(v as f64))),
        C::F64(v) => nullable!(
            v,
            |v| if matches!(ty, ColumnType::Money | ColumnType::Money4) {
                None
            } else {
                Some(TypedValue::Float(v))
            }
        ),
        C::String(v) => nullable!(v, |v: std::borrow::Cow<'_, str>| if matches!(
            ty,
            ColumnType::NVarchar
                | ColumnType::NChar
                | ColumnType::NText
                | ColumnType::BigVarChar
                | ColumnType::BigChar
                | ColumnType::Text
        ) {
            Some(TypedValue::Text(v.into_owned()))
        } else {
            None
        }),
        C::Numeric(v) => nullable!(v, |v| Some(TypedValue::Decimal(mssql_numeric_string(v)))),
        C::Binary(v) => nullable!(v, |v: std::borrow::Cow<'_, [u8]>| Some(TypedValue::binary(
            &v
        ))),
        C::Date(v) => nullable!(v, |v| Some(TypedValue::Date(mssql_date_string(v)?))),
        C::Time(v) => nullable!(v, |v: tiberius::time::Time| Some(TypedValue::Time(
            TimeValue {
                value: mssql_time_string(v)?,
                precision: v.scale()
            }
        ))),
        C::DateTime2(v) => nullable!(v, |v: tiberius::time::DateTime2| Some(
            TypedValue::Timestamp(TimestampValue {
                value: format!(
                    "{}T{}",
                    mssql_date_string(v.date())?,
                    mssql_time_string(v.time())?
                ),
                precision: v.time().scale(),
                timezone: TimezoneKind::None
            })
        )),
        // datetime 的 1/300 秒不能用有限小数秒精确表达；offset 保留为只读。
        C::DateTime(v) => nullable!(v, |_| None),
        C::SmallDateTime(v) => nullable!(v, |_| None),
        C::DateTimeOffset(v) => nullable!(v, |_| None),
        C::Guid(v) => nullable!(v, |_| None),
        C::Xml(v) => nullable!(v, |_| None),
    }?;
    value.validate().ok().map(|_| value)
}

#[cfg(test)]
mod budget_tests {
    use super::*;
    use crate::database::result_budget::{MAX_QUERY_BYTES, MAX_QUERY_CELL_BYTES, MAX_QUERY_ROWS};
    use futures_util::stream;
    use std::cell::Cell;

    fn columns() -> Vec<ColumnMetadata> {
        vec![ColumnMetadata {
            name: "保留的列".into(),
            data_type: "NVarchar".into(),
            unified_type: "Unknown".into(),
            nullable: true,
            length: None,
            precision: None,
            scale: None,
        }]
    }

    fn metadata(index: usize) -> Result<MsSqlResultItem<String>, DatabaseError> {
        Ok(MsSqlResultItem::Metadata {
            columns: columns(),
            result_index: index,
        })
    }

    fn mock_row(text: String) -> Result<MsSqlResultItem<String>, DatabaseError> {
        Ok(MsSqlResultItem::Row {
            row: text,
            result_index: 0,
        })
    }

    fn decode_mock(
        text: String,
        budget: &QueryBudget,
        typed: bool,
    ) -> Result<Option<(RowData, Vec<Option<TypedValue>>)>, DatabaseError> {
        if !budget.allows_raw_row([text.len()]) {
            return Ok(None);
        }
        let row = RowData {
            values: vec![Some(serde_json::Value::String(text.clone()))],
        };
        let typed_row = if typed {
            vec![Some(TypedValue::Text(text))]
        } else {
            Vec::new()
        };
        Ok(Some((row, typed_row)))
    }

    #[tokio::test]
    async fn zero_rows_keep_metadata_and_complete_stream() {
        let output = mssql_collect_first_result(
            stream::iter([metadata(0)]),
            std::time::Instant::now(),
            true,
            decode_mock,
        )
        .await
        .unwrap();
        assert_eq!(output.value.result.columns[0].name, "保留的列");
        assert!(output.value.result.rows.is_empty());
        assert!(output.value.typed_rows.is_empty());
        assert!(!output.value.result.truncated);
        assert!(output.stream_complete);
    }

    #[tokio::test]
    async fn exact_row_limit_reads_end_without_marking_truncated() {
        let source = stream::iter([metadata(0)]).chain(stream::iter(
            (0..MAX_QUERY_ROWS).map(|_| mock_row("小值".into())),
        ));
        let output =
            mssql_collect_first_result(source, std::time::Instant::now(), true, decode_mock)
                .await
                .unwrap();
        assert_eq!(output.value.result.rows.len(), MAX_QUERY_ROWS);
        assert_eq!(output.value.typed_rows.len(), MAX_QUERY_ROWS);
        assert!(!output.value.result.truncated);
        assert!(output.stream_complete);
    }

    #[tokio::test]
    async fn unending_stream_stops_after_one_probe_and_never_drains() {
        let polled = Cell::new(0);
        let decoded = Cell::new(0);
        let source = stream::iter([metadata(0)])
            .chain(stream::repeat_with(|| mock_row("小值".into())))
            .inspect(|_| polled.set(polled.get() + 1));
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            mssql_collect_first_result(
                source,
                std::time::Instant::now(),
                true,
                |row, budget, typed| {
                    decoded.set(decoded.get() + 1);
                    decode_mock(row, budget, typed)
                },
            ),
        )
        .await
        .expect("预算截断不能继续 drain 无限流")
        .unwrap();
        assert_eq!(polled.get(), MAX_QUERY_ROWS + 2);
        assert_eq!(decoded.get(), MAX_QUERY_ROWS);
        assert_eq!(output.value.result.rows.len(), MAX_QUERY_ROWS);
        assert_eq!(output.value.typed_rows.len(), MAX_QUERY_ROWS);
        assert!(output.value.result.truncated);
        assert!(!output.stream_complete);
    }

    #[tokio::test]
    async fn second_metadata_stops_before_any_later_rows() {
        let polled = Cell::new(0);
        let source = stream::iter([
            metadata(0),
            mock_row("首结果".into()),
            metadata(1),
            Err(DatabaseError::QueryError("后续响应不得读取".into())),
        ])
        .inspect(|_| polled.set(polled.get() + 1));
        let output =
            mssql_collect_first_result(source, std::time::Instant::now(), false, decode_mock)
                .await
                .unwrap();
        assert_eq!(polled.get(), 3);
        assert_eq!(output.value.result.rows.len(), 1);
        assert!(output.value.result.truncated);
        assert!(!output.stream_complete);
    }

    #[tokio::test]
    async fn oversized_raw_cell_stops_without_following_the_stream() {
        let polled = Cell::new(0);
        let source = stream::iter([
            metadata(0),
            mock_row("x".repeat(MAX_QUERY_CELL_BYTES + 1)),
            Err(DatabaseError::QueryError("超限后不得读取".into())),
        ])
        .inspect(|_| polled.set(polled.get() + 1));
        let output =
            mssql_collect_first_result(source, std::time::Instant::now(), true, decode_mock)
                .await
                .unwrap();
        assert_eq!(polled.get(), 2);
        assert!(output.value.result.rows.is_empty());
        assert!(output.value.typed_rows.is_empty());
        assert!(output.value.result.truncated);
        assert!(!output.stream_complete);
    }

    #[tokio::test]
    async fn byte_budget_keeps_display_and_typed_rows_in_sync() {
        for typed in [false, true] {
            let source = stream::iter([metadata(0)])
                .chain(stream::repeat_with(|| mock_row("\"\\中文".repeat(20_000))));
            let output =
                mssql_collect_first_result(source, std::time::Instant::now(), typed, decode_mock)
                    .await
                    .unwrap();
            let result = output.value;
            assert!(result.result.truncated);
            assert!(!output.stream_complete);
            assert!(!result.result.rows.is_empty());
            assert!(result.result.rows.len() < MAX_QUERY_ROWS);
            assert_eq!(
                result.typed_rows.len(),
                if typed { result.result.rows.len() } else { 0 }
            );
            assert!(
                serde_json::to_vec(&(&result.result, &result.typed_rows))
                    .unwrap()
                    .len()
                    < MAX_QUERY_BYTES
            );
        }
    }

    #[test]
    fn raw_variable_payload_lengths_are_checked_without_conversion() {
        use tiberius::ColumnData as C;
        let text = "x".repeat(MAX_QUERY_CELL_BYTES + 1);
        assert_eq!(
            mssql_raw_cell_length(&C::String(Some(text.clone().into()))),
            text.len()
        );
        assert_eq!(
            mssql_raw_cell_length(&C::Binary(Some(vec![0; text.len()].into()))),
            text.len()
        );
        assert_eq!(
            mssql_raw_cell_length(&C::Xml(Some(std::borrow::Cow::Owned(
                tiberius::xml::XmlData::new(&text)
            )))),
            text.len()
        );
        assert_eq!(mssql_raw_cell_length(&C::String(None)), 0);
    }

    #[tokio::test]
    async fn isolated_lease_releases_safely_without_a_socket() {
        let lease = MsSqlSessionLease {
            id: uuid::Uuid::new_v4().to_string(),
            client: None,
        };
        assert!(Box::new(lease).release().await.is_ok());
    }
}

#[cfg(test)]
mod typed_tests {
    use super::*;

    #[test]
    fn isolated_lease_has_no_cancellation_handle_or_reusable_client() {
        let mut lease = MsSqlSessionLease {
            id: uuid::Uuid::new_v4().to_string(),
            client: None,
        };
        let capabilities = lease.capabilities();
        assert!(capabilities.exclusive_session);
        assert!(!capabilities.cancellation);
        assert!(capabilities
            .cancellation_reason
            .unwrap()
            .contains("不代表服务端查询已经停止"));
        assert!(lease.cancel_handle().is_none());
        assert!(lease
            .take_client()
            .err()
            .unwrap()
            .to_string()
            .contains("事务结果可能未知"));
    }

    #[tokio::test]
    async fn disconnected_driver_rejects_leases_without_opening_a_socket() {
        let config: ConnectionConfig = serde_json::from_value(serde_json::json!({
            "name": "离线断开验证", "driverType": "mssql", "host": "不可访问测试主机.invalid",
            "port": 1433, "user": "", "password": ""
        }))
        .unwrap();
        let driver = MsSqlDriver {
            connected: AtomicBool::new(false),
            config,
        };
        assert!(driver
            .lease_session(None)
            .await
            .err()
            .unwrap()
            .to_string()
            .contains("连接已断开"));
        assert!(!driver.session_capabilities().cancellation);
        assert!(driver
            .cancel_query(1)
            .await
            .unwrap_err()
            .to_string()
            .contains("未实现可靠的服务端取消"));
    }

    #[test]
    fn null_binds_as_native_nullable_unicode_instead_of_int() {
        let bound = mssql_bound_params(&[TypedValue::Null]).unwrap();
        assert!(matches!(
            bound.as_slice(),
            [tiberius::ColumnData::String(None)]
        ));
    }

    #[test]
    fn decimal_native_binding_preserves_negative_fraction_and_38_digits() {
        for value in [
            "0",
            "0.000",
            "-0.001",
            "-12.3456",
            "123456789012345678901.12345678901234567",
            "99999999999999999999999999999999999999",
        ] {
            assert_eq!(mssql_numeric_string(mssql_numeric(value).unwrap()), value);
        }
        assert!(mssql_numeric("0.00000000000000000000000000000000000001").is_err());
        assert!(mssql_numeric("123456789012345678901234567890123456789").is_err());
    }

    #[test]
    fn temporal_native_decode_keeps_precision_and_legacy_types_read_only() {
        let time = tiberius::time::Time::new(452961234567, 7);
        assert_eq!(mssql_time_string(time).as_deref(), Some("12:34:56.1234567"));
        assert_eq!(
            mssql_typed_value(tiberius::ColumnData::Time(Some(time)), ColumnType::Timen),
            Some(TypedValue::Time(TimeValue {
                value: "12:34:56.1234567".into(),
                precision: 7
            }))
        );
        assert!(mssql_typed_value(
            tiberius::ColumnData::DateTime(Some(tiberius::time::DateTime::new(0, 1))),
            ColumnType::Datetime
        )
        .is_none());
        assert_eq!(
            mssql_typed_value(tiberius::ColumnData::DateTime(None), ColumnType::Datetime),
            Some(TypedValue::Null)
        );
        assert!(mssql_bound_query(
            "SELECT @P1",
            &[TypedValue::Time(TimeValue {
                value: "12:00:00.12345678".into(),
                precision: 8
            })]
        )
        .is_err());
    }

    #[test]
    fn bound_execution_rejects_explicit_batches() {
        assert!(mssql_single_statement("INSERT INTO t VALUES (@P1); DELETE FROM t").is_err());
        assert!(mssql_single_statement("INSERT INTO t VALUES (@P1)").is_ok());
        assert!(mssql_single_statement("SELECT ';'").is_ok());
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
    get_str!(
        tiberius::numeric::Numeric,
        |v: tiberius::numeric::Numeric| v.to_string()
    );
    get_str!(uuid::Uuid, |v: uuid::Uuid| v.to_string());
    get_str!(chrono::NaiveDateTime, |v: chrono::NaiveDateTime| v
        .format("%Y-%m-%d %H:%M:%S")
        .to_string());
    get_str!(chrono::NaiveDate, |v: chrono::NaiveDate| v
        .format("%Y-%m-%d")
        .to_string());
    get_str!(chrono::NaiveTime, |v: chrono::NaiveTime| v
        .format("%H:%M:%S")
        .to_string());
    get_str!(chrono::DateTime<chrono::Utc>, |v: chrono::DateTime<
        chrono::Utc,
    >| v
        .format("%Y-%m-%d %H:%M:%S %z")
        .to_string());
    get_str!(&[u8], |v: &[u8]| format!(
        "\\x{}",
        v.iter().map(|b| format!("{:02x}", b)).collect::<String>()
    ));
    get_str!(&tiberius::xml::XmlData, |v: &tiberius::xml::XmlData| v
        .to_string());

    J::String(format!("‹{:?}›", ty))
}

impl MsSqlDriver {
    async fn columns_for_schema(
        &self,
        database: Option<&str>,
        schema: &str,
        table: &str,
    ) -> Result<Vec<ColumnInfo>, DatabaseError> {
        let mut client = self.client_for(database).await?;
        let stream = client.query(
            "SELECT c.COLUMN_NAME, CASE \
                    WHEN c.DATA_TYPE IN ('decimal', 'numeric') THEN c.DATA_TYPE + '(' + CAST(c.NUMERIC_PRECISION AS VARCHAR(3)) + ',' + CAST(c.NUMERIC_SCALE AS VARCHAR(3)) + ')' \
                    WHEN c.DATA_TYPE IN ('time', 'datetime2', 'datetimeoffset') THEN c.DATA_TYPE + '(' + CAST(c.DATETIME_PRECISION AS VARCHAR(3)) + ')' \
                    WHEN c.DATA_TYPE IN ('binary', 'varbinary', 'varchar', 'nvarchar', 'char', 'nchar') THEN c.DATA_TYPE + '(' + CASE WHEN c.CHARACTER_MAXIMUM_LENGTH = -1 THEN 'max' ELSE CAST(c.CHARACTER_MAXIMUM_LENGTH AS VARCHAR(10)) END + ')' \
                    ELSE c.DATA_TYPE END, c.IS_NULLABLE, c.COLUMN_DEFAULT, \
                    CASE WHEN kcu.COLUMN_NAME IS NOT NULL THEN 1 ELSE 0 END, \
                    CAST(ep.value AS NVARCHAR(4000)), \
                    CASE WHEN sc.is_computed = 1 THEN 'generated computed' \
                         WHEN sc.is_identity = 1 THEN 'identity' ELSE NULL END \
             FROM INFORMATION_SCHEMA.COLUMNS c \
             LEFT JOIN INFORMATION_SCHEMA.TABLE_CONSTRAINTS tc \
               ON tc.TABLE_NAME = c.TABLE_NAME AND tc.TABLE_SCHEMA = c.TABLE_SCHEMA \
              AND tc.CONSTRAINT_TYPE = 'PRIMARY KEY' \
             LEFT JOIN INFORMATION_SCHEMA.KEY_COLUMN_USAGE kcu \
               ON kcu.CONSTRAINT_NAME = tc.CONSTRAINT_NAME AND kcu.CONSTRAINT_SCHEMA = tc.CONSTRAINT_SCHEMA \
              AND kcu.TABLE_NAME = c.TABLE_NAME AND kcu.TABLE_SCHEMA = c.TABLE_SCHEMA \
              AND kcu.COLUMN_NAME = c.COLUMN_NAME \
             LEFT JOIN sys.objects st ON st.name = c.TABLE_NAME AND st.schema_id = SCHEMA_ID(c.TABLE_SCHEMA) \
             LEFT JOIN sys.columns sc ON sc.object_id = st.object_id AND sc.name = c.COLUMN_NAME \
             LEFT JOIN sys.extended_properties ep \
               ON ep.major_id = st.object_id AND ep.minor_id = sc.column_id AND ep.name = 'MS_Description' \
             WHERE c.TABLE_NAME = @P1 AND c.TABLE_SCHEMA = @P2 \
             ORDER BY c.ORDINAL_POSITION",
            &[&table, &schema],
        ).await.map_err(map_query_err)?;
        let rows = stream.into_first_result().await.map_err(map_query_err)?;

        let columns = rows
            .iter()
            .map(|row| {
                let nullable = mssql_metadata_string(row, 2)?;
                let is_pk = row
                    .try_get::<i32, _>(4)
                    .map_err(map_query_err)?
                    .ok_or_else(|| DatabaseError::QueryError("主键列标记缺失".into()))?;
                Ok(ColumnInfo {
                    name: mssql_metadata_string(row, 0)?,
                    data_type: mssql_metadata_string(row, 1)?,
                    nullable: nullable == "YES",
                    is_primary_key: is_pk == 1,
                    default_value: row
                        .try_get::<&str, _>(3)
                        .map_err(map_query_err)?
                        .map(str::to_owned),
                    comment: row
                        .try_get::<&str, _>(5)
                        .map_err(map_query_err)?
                        .map(str::to_owned),
                    extra: row
                        .try_get::<&str, _>(6)
                        .map_err(map_query_err)?
                        .map(str::to_owned),
                })
            })
            .collect::<Result<Vec<_>, DatabaseError>>()?;
        Ok(columns)
    }
}

/// 系统目录必填文本；NULL 和协议类型错误均使整个元数据请求失败。
fn mssql_metadata_string(row: &tiberius::Row, index: usize) -> Result<String, DatabaseError> {
    row.try_get::<&str, _>(index)
        .map_err(map_query_err)?
        .map(str::to_owned)
        .ok_or_else(|| {
            DatabaseError::QueryError(format!("系统目录第 {} 列缺少必填文本", index + 1))
        })
}
