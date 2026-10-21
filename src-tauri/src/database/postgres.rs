use crate::database::result_budget::QueryBudget;
use crate::database::DatabaseDriver;
use crate::models::*;
use crate::session::{SessionCancellation, SessionCapabilities, SessionLease, TargetSnapshot};
use crate::typed_value::{
    validate_values, TimeValue, TimestampValue, TimezoneKind, TypedQueryResult, TypedValue,
};
use async_trait::async_trait;
use futures_util::TryStreamExt;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex, MutexGuard};
use tokio::sync::Mutex;
use tokio_postgres::types::Type;
use tokio_postgres::{Client, Config, NoTls};

/// 这里只保护内存登记表，使用同步锁以便 future 被丢弃时也能立即清理。
struct CancelRegistry<T> {
    entries: StdMutex<HashMap<u64, Arc<T>>>,
}

impl<T> CancelRegistry<T> {
    fn new() -> Self {
        Self {
            entries: StdMutex::new(HashMap::new()),
        }
    }

    fn entries(&self) -> MutexGuard<'_, HashMap<u64, Arc<T>>> {
        // 清理可能发生在 panic 展开期间，锁中毒不能再次触发 panic。
        self.entries.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn register(&self, query_id: u64, token: T) -> CancelRegistration<'_, T> {
        let token = Arc::new(token);
        self.entries().insert(query_id, Arc::clone(&token));
        CancelRegistration {
            registry: self,
            query_id,
            token,
        }
    }

    fn get(&self, query_id: u64) -> Option<Arc<T>> {
        self.entries().get(&query_id).cloned()
    }
}

struct CancelRegistration<'a, T> {
    registry: &'a CancelRegistry<T>,
    query_id: u64,
    token: Arc<T>,
}

impl<T> Drop for CancelRegistration<'_, T> {
    fn drop(&mut self) {
        let mut entries = self.registry.entries();
        // 同一 ID 若已被新查询覆盖，旧查询结束时不能删掉新登记。
        if entries
            .get(&self.query_id)
            .is_some_and(|token| Arc::ptr_eq(token, &self.token))
        {
            entries.remove(&self.query_id);
        }
    }
}

/// PostgreSQL 每个租约独立建连；正常归还和异常析构均关闭，不与元数据连接交错。
struct PgLease {
    id: String,
    client: Client,
    task: tokio::task::JoinHandle<()>,
    connected: Arc<AtomicBool>,
    cancellation: Arc<PgCancellation>,
    dirty: bool,
    /// 操作future未正常返回时保留，禁止在未知会话状态下继续发SQL。
    in_flight: bool,
}

struct PgCancellation {
    token: tokio_postgres::CancelToken,
    active: AtomicBool,
    cancelled: AtomicBool,
    connected: Arc<AtomicBool>,
    gate: Mutex<()>,
}

#[async_trait]
impl SessionCancellation for PgCancellation {
    async fn cancel(&self) -> Result<(), DatabaseError> {
        let _guard = self.gate.lock().await;
        if !self.active.load(Ordering::SeqCst) || !self.connected.load(Ordering::SeqCst) {
            return Err(DatabaseError::Unknown("租约已归还或连接已断开".into()));
        }
        self.cancelled.store(true, Ordering::SeqCst);
        self.token
            .cancel_query(NoTls)
            .await
            .map_err(map_pg_query_error)
    }
}

impl PgDriver {
    async fn new_lease(&self, database: Option<&str>) -> Result<PgLease, DatabaseError> {
        if !self.connected.load(Ordering::SeqCst) {
            return Err(DatabaseError::ConnectionError("连接已断开".into()));
        }
        let database = database
            .filter(|d| !d.is_empty())
            .or(self.config.database.as_deref().filter(|d| !d.is_empty()))
            .unwrap_or("postgres");
        let (client, connection) = self
            .pg_config(database)
            .connect(NoTls)
            .await
            .map_err(|e| DatabaseError::ConnectionError(e.to_string()))?;
        let cancellation = Arc::new(PgCancellation {
            token: client.cancel_token(),
            active: AtomicBool::new(true),
            cancelled: AtomicBool::new(false),
            connected: self.connected.clone(),
            gate: Mutex::new(()),
        });
        let task = tokio::spawn(async move {
            let _ = connection.await;
        });
        let lease = PgLease {
            id: uuid::Uuid::new_v4().to_string(),
            client,
            task,
            connected: self.connected.clone(),
            cancellation,
            dirty: false,
            in_flight: false,
        };
        lease.ensure_live()?;
        Ok(lease)
    }
}

impl PgLease {
    /// 未读完的响应不能留给下一条 SQL；关闭本租约的物理连接，不排空结果。
    fn quarantine_stream(&mut self) {
        self.dirty = true;
        self.cancellation.active.store(false, Ordering::SeqCst);
        self.task.abort();
    }

    fn begin_operation(&mut self) -> Result<bool, DatabaseError> {
        self.ensure_live()?;
        if self.in_flight {
            return Err(DatabaseError::ConnectionError(
                "上次操作尚未完成或已中断，租约状态未知，必须归还隔离".into(),
            ));
        }
        self.in_flight = true;
        let was_dirty = self.dirty;
        self.dirty = true;
        Ok(was_dirty)
    }

    fn ensure_live(&self) -> Result<(), DatabaseError> {
        if !self.connected.load(Ordering::SeqCst)
            || !self.cancellation.active.load(Ordering::SeqCst)
        {
            Err(DatabaseError::ConnectionError(
                "连接已断开或租约已归还".into(),
            ))
        } else {
            Ok(())
        }
    }
}

impl Drop for PgLease {
    fn drop(&mut self) {
        self.cancellation.active.store(false, Ordering::SeqCst);
        // 强制终止连接任务，服务端断连自动回滚未提交事务。
        self.task.abort();
    }
}

#[async_trait]
impl SessionLease for PgLease {
    fn session_id(&self) -> &str {
        &self.id
    }
    fn capabilities(&self) -> SessionCapabilities {
        SessionCapabilities::supported()
    }
    fn cancel_handle(&self) -> Option<Arc<dyn SessionCancellation>> {
        Some(self.cancellation.clone())
    }
    async fn query_typed(
        &mut self,
        sql: &str,
        params: &[TypedValue],
    ) -> Result<TypedQueryResult, DatabaseError> {
        let was_dirty = self.begin_operation()?;
        let result: Result<TypedQueryResult, DatabaseError> = async {
            validate_values(params)?;
            let start = std::time::Instant::now();
            self.ensure_live()?;
            let client = &self.client;
            let statement = client.prepare(sql).await.map_err(map_pg_query_error)?;
            let bound = pg_bind_params(params, statement.params())?;
            let refs: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> =
                bound.iter().map(|v| v as _).collect();
            let columns: Vec<ColumnMetadata> = statement
                .columns()
                .iter()
                .map(|c| ColumnMetadata {
                    name: c.name().into(),
                    data_type: c.type_().to_string(),
                    unified_type: "Unknown".into(),
                    nullable: true,
                    length: None,
                    precision: None,
                    scale: None,
                })
                .collect();
            let mut budget = QueryBudget::new(&columns)?;
            // query_raw 的有界响应通道随消费推进，不先收集完整结果。
            let mut source = Box::pin(
                client
                    .query_raw(&statement, refs)
                    .await
                    .map_err(map_pg_query_error)?,
            );
            let mut truncated = false;
            let mut rows = Vec::new();
            let mut typed_rows = Vec::new();
            while let Some(row) = source.try_next().await.map_err(|error| {
                self.quarantine_stream();
                map_pg_query_error(error)
            })? {
                if budget.row_limit_reached()
                    || !pg_raw_row_fits(&budget, &row).map_err(|error| {
                        self.quarantine_stream();
                        error
                    })?
                {
                    truncated = true;
                    self.quarantine_stream();
                    break;
                }
                let typed: Vec<_> = row
                    .columns()
                    .iter()
                    .enumerate()
                    .map(|(i, c)| pg_typed_value(&row, i, c.type_()))
                    .collect();
                let values =
                    typed
                        .iter()
                        .enumerate()
                        .map(|(i, v)| {
                            Some(v.as_ref().map(TypedValue::display_json).unwrap_or_else(|| {
                                pg_value_to_json(&row, i, row.columns()[i].type_())
                            }))
                        })
                        .collect();
                let row = RowData { values };
                if !budget.try_add_row(&row, Some(&typed)).map_err(|error| {
                    self.quarantine_stream();
                    error
                })? {
                    truncated = true;
                    self.quarantine_stream();
                    break;
                }
                rows.push(row);
                typed_rows.push(typed);
            }
            Ok(TypedQueryResult {
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
            })
        }
        .await;
        // 已完整收到正常返回（包括SQL错误）；future中断时不会执行这里。
        self.in_flight = false;
        // 截断成功已经主动关闭物理连接，仍须交付已接受的行；后续 SQL 由 ensure_live 拒绝。
        if result.as_ref().is_ok_and(|value| value.result.truncated) {
            return result;
        }
        // 流中错误同样会隔离连接，错误信息保留原始原因。
        if result.is_err() && !self.cancellation.active.load(Ordering::SeqCst) {
            return result;
        }
        if result.is_ok() {
            self.dirty = was_dirty;
        }
        self.ensure_live()?;
        result
    }

    async fn stream_export(
        &mut self,
        sql: &str,
        sink: Arc<dyn crate::export::ExportSink>,
    ) -> Result<u64, DatabaseError> {
        let was_dirty = self.begin_operation()?;
        let result = async {
            sink.check_cancelled()?;
            self.ensure_live()?;
            // 扩展查询协议只允许一条语句，prepare 同时保留空结果的列名。
            let statement = self.client.prepare(sql).await.map_err(map_pg_query_error)?;
            if statement.columns().is_empty() {
                return Err(DatabaseError::QueryError(
                    "EXPORT_RESULT_UNSUPPORTED: 导出需要一个 SELECT 结果集".into(),
                ));
            }
            let columns: Vec<_> = statement
                .columns()
                .iter()
                .map(|column| crate::export::ExportColumn {
                    name: column.name().into(),
                })
                .collect();
            sink.columns(&columns)?;
            let mut source = Box::pin(
                self.client
                    .query_raw(
                        &statement,
                        std::iter::empty::<&(dyn tokio_postgres::types::ToSql + Sync)>(),
                    )
                    .await
                    .map_err(map_pg_query_error)?,
            );
            let mut count = 0u64;
            while let Some(row) = source.try_next().await.map_err(map_pg_query_error)? {
                sink.check_cancelled()?;
                // 只借用原始字节检查长度，超限时不复制、转换或继续读取。
                let lengths = (0..row.len())
                    .map(|index| {
                        row.try_get::<_, Option<PgRawLength>>(index)
                            .map(|value| value.map_or(0, |value| value.0))
                            .map_err(map_pg_query_error)
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                crate::export::check_raw_lengths(lengths)?;
                let typed =
                    row.columns()
                        .iter()
                        .enumerate()
                        .map(|(index, column)| {
                            pg_typed_value(&row, index, column.type_()).ok_or_else(|| {
                                DatabaseError::TypeError(format!(
                            "EXPORT_TYPE_UNSUPPORTED: PostgreSQL 列 {} 的类型 {} 无法无损导出",
                            column.name(), column.type_(),
                        ))
                            })
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                sink.row(&typed)?;
                count = count.checked_add(1).ok_or_else(|| {
                    DatabaseError::QueryError("EXPORT_ROW_COUNT_OVERFLOW: 导出行数溢出".into())
                })?;
            }
            sink.check_cancelled()?;
            Ok(count)
        }
        .await;
        self.in_flight = false;
        if result.is_err() {
            // 包含 sink 失败、取消和流错误：不 drain，不把未知状态租约交回调用方。
            self.quarantine_stream();
            return result;
        }
        self.dirty = was_dirty;
        self.ensure_live()?;
        result
    }

    async fn execute_bound(
        &mut self,
        sql: &str,
        params: &[TypedValue],
    ) -> Result<u64, DatabaseError> {
        let was_dirty = self.begin_operation()?;
        let result = async {
            validate_values(params)?;
            self.ensure_live()?;
            let client = &self.client;
            let statement = client.prepare(sql).await.map_err(map_pg_execute_error)?;
            let bound = pg_bind_params(params, statement.params())?;
            let refs: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> =
                bound.iter().map(|v| v as _).collect();
            client
                .execute(&statement, &refs)
                .await
                .map_err(map_pg_execute_error)
        }
        .await;
        // 已完整收到正常返回（包括SQL错误）；future中断时不会执行这里。
        self.in_flight = false;
        if result.is_ok() {
            self.dirty = was_dirty;
        }
        self.ensure_live()?;
        result
    }

    async fn query(&mut self, sql: &str) -> Result<QueryResult, DatabaseError> {
        let was_dirty = self.begin_operation()?;
        let result: Result<QueryResult, DatabaseError> = async {
            let start = std::time::Instant::now();
            self.ensure_live()?;
            let client = &self.client;

            // 列元数据从预编译语句上取：空结果集没有任何 Row，从首行取列
            // 会拿到空 columns，前端会把空结果的 SELECT 当成 DML 展示
            let statement = client.prepare(sql).await.map_err(map_pg_query_error)?;
            let columns: Vec<ColumnMetadata> = statement
                .columns()
                .iter()
                .map(|col| ColumnMetadata {
                    name: col.name().to_string(),
                    data_type: format!("{}", col.type_()),
                    unified_type: "Unknown".to_string(),
                    nullable: true, // information_schema 才有准确值，结果集层面默认为 true
                    length: None,
                    precision: None,
                    scale: None,
                })
                .collect();

            let mut budget = QueryBudget::new(&columns)?;
            let mut source = Box::pin(
                client
                    .query_raw(
                        &statement,
                        std::iter::empty::<&(dyn tokio_postgres::types::ToSql + Sync)>(),
                    )
                    .await
                    .map_err(map_pg_query_error)?,
            );

            let mut rows: Vec<RowData> = Vec::new();
            let mut truncated = false;

            while let Some(pg_row) = source.try_next().await.map_err(|error| {
                self.quarantine_stream();
                map_pg_query_error(error)
            })? {
                if budget.row_limit_reached()
                    || !pg_raw_row_fits(&budget, &pg_row).map_err(|error| {
                        self.quarantine_stream();
                        error
                    })?
                {
                    truncated = true;
                    self.quarantine_stream();
                    break;
                }
                let values = pg_row
                    .columns()
                    .iter()
                    .enumerate()
                    .map(|(idx, col)| Some(pg_value_to_json(&pg_row, idx, col.type_())))
                    .collect();
                let row = RowData { values };
                if !budget.try_add_row(&row, None).map_err(|error| {
                    self.quarantine_stream();
                    error
                })? {
                    truncated = true;
                    self.quarantine_stream();
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
        .await;
        // 已完整收到正常返回（包括SQL错误）；future中断时不会执行这里。
        self.in_flight = false;
        // 截断后不复用当前会话，但保留成功的部分结果。
        if result.as_ref().is_ok_and(|value| value.truncated) {
            return result;
        }
        if result.is_err() && !self.cancellation.active.load(Ordering::SeqCst) {
            return result;
        }
        if result.is_ok() {
            self.dirty = was_dirty;
        }
        self.ensure_live()?;
        result
    }

    async fn execute(&mut self, sql: &str) -> Result<QueryResult, DatabaseError> {
        let was_dirty = self.begin_operation()?;
        let result = async {
            let start = std::time::Instant::now();
            self.ensure_live()?;
            let client = &self.client;

            let affected = client
                .execute(sql, &[])
                .await
                .map_err(map_pg_execute_error)?;

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
        .await;
        // 已完整收到正常返回（包括SQL错误）；future中断时不会执行这里。
        self.in_flight = false;
        if result.is_ok() {
            self.dirty = was_dirty;
        }
        self.ensure_live()?;
        result
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
            "\"{}\".\"{}\"",
            schema.replace('"', "\"\""),
            target.table.replace('"', "\"\"")
        );
        let was_dirty = self.begin_operation()?;
        let result = async {
            // 锁和全部目录读取都在本租约、本事务内；普通目录连接不可替代。
            self.client.batch_execute(&format!("LOCK TABLE {quoted} IN {} MODE",
                if lock { "SHARE ROW EXCLUSIVE" } else { "ACCESS SHARE" }))
                .await.map_err(map_pg_query_error)?;
            let row = self.client.query_opt(
                "SELECT current_database(), d.oid::text, n.nspname, c.relname, c.oid::text, \
                 c.relfilenode::text, c.xmin::text, c.relkind::text, c.relpersistence::text, \
                 c.relrowsecurity OR c.relforcerowsecurity, c.relhasrules, \
                 EXISTS (SELECT 1 FROM pg_catalog.pg_trigger t WHERE t.tgrelid = c.oid), \
                 EXISTS (SELECT 1 FROM pg_catalog.pg_inherits i WHERE i.inhrelid = c.oid OR i.inhparent = c.oid), \
                 (SELECT count(*) FROM pg_catalog.pg_attribute a WHERE a.attrelid = c.oid AND a.attnum > 0 AND NOT a.attisdropped) \
                 FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
                 JOIN pg_catalog.pg_database d ON d.datname = current_database() \
                 WHERE n.nspname = $1 AND c.relname = $2",
                &[&schema, &target.table],
            ).await.map_err(map_pg_query_error)?.ok_or_else(||
                DatabaseError::QueryError("OBJECT_IDENTITY_UNSUPPORTED: 目标对象不存在".into()))?;
            let database: String = row.get(0);
            if database != target.database {
                return Err(DatabaseError::QueryError("OBJECT_IDENTITY_UNSUPPORTED: 当前会话数据库不符".into()));
            }
            let kind: String = row.get(7);
            let mut write_reasons: Vec<String> = Vec::new();
            let version: i32 = self.client.query_one("SELECT current_setting('server_version_num')::int", &[])
                .await.map_err(map_pg_query_error)?.get(0);
            if version / 10000 != 17 { write_reasons.push("DRIVER_VERSION_NOT_VALIDATED".into()); }
            if kind != "r" || row.get::<_, String>(8) != "p" || row.get::<_, bool>(12) {
                write_reasons.push("OBJECT_WRITE_SEMANTICS_UNSUPPORTED".into());
            }
            if row.get::<_, bool>(9) { write_reasons.push("ROW_SECURITY_UNSUPPORTED".into()); }
            if row.get::<_, bool>(10) { write_reasons.push("RULES_UNSUPPORTED".into()); }
            if row.get::<_, bool>(11) { write_reasons.push("TRIGGERS_UNSUPPORTED".into()); }
            let rows = self.client.query("SELECT c.column_name, pg_catalog.format_type(a.atttypid, a.atttypmod), c.is_nullable, c.column_default, \
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
             ) AS comment, \
             CASE \
               WHEN COALESCE(to_jsonb(a)->>'attgenerated', '') <> '' THEN \
                 'generated ' || CASE to_jsonb(a)->>'attgenerated' \
                   WHEN 's' THEN 'stored' WHEN 'v' THEN 'virtual' ELSE 'unknown' END \
               WHEN COALESCE(to_jsonb(a)->>'attidentity', '') <> '' THEN \
                 'identity ' || CASE to_jsonb(a)->>'attidentity' \
                   WHEN 'a' THEN 'always' WHEN 'd' THEN 'by default' ELSE 'unknown' END \
               ELSE NULL END AS extra \
             FROM information_schema.columns c \
             LEFT JOIN pg_catalog.pg_attribute a \
               ON a.attrelid = format('%I.%I', c.table_schema, c.table_name)::regclass \
              AND a.attname = c.column_name AND NOT a.attisdropped \
             WHERE c.table_schema = $2 AND c.table_name = $1 \
             ORDER BY c.ordinal_position", &[&target.table, &schema])
                .await.map_err(map_pg_query_error)?;
            let columns: Vec<ColumnInfo> = rows.iter().map(|r| ColumnInfo {
                name: r.get(0), data_type: r.get(1), nullable: r.get::<_, String>(2) == "YES",
                default_value: r.get(3), is_primary_key: r.get(4), comment: r.get(5), extra: r.get(6),
            }).collect();
            if columns.is_empty() || columns.len() as i64 != row.get::<_, i64>(13)
                || columns.iter().any(|c| c.name.is_empty() || c.data_type.is_empty()) {
                return Err(DatabaseError::QueryError("OBJECT_IDENTITY_UNSUPPORTED: 完整列目录不可用".into()));
            }
            let identity = serde_json::to_string(&(row.get::<_, String>(1), row.get::<_, String>(4),
                row.get::<_, String>(5), row.get::<_, String>(6)))
                .map_err(|_| DatabaseError::Unknown("对象身份编码失败".into()))?;
            Ok(TargetSnapshot { table: ResolvedTable { database,
                schema: Some(row.get(2)), table: row.get(3), columns,
                object_kind: Some(if matches!(kind.as_str(), "r" | "p") { "table" } else { "unknown" }.into()),
            }, identity, write_reasons })
        }.await;
        self.in_flight = false;
        if result.is_ok() {
            self.dirty = was_dirty;
        }
        self.ensure_live()?;
        result
    }
    async fn begin(&mut self) -> Result<(), DatabaseError> {
        self.execute("BEGIN").await.map(|_| ())
    }
    async fn commit(&mut self) -> Result<(), DatabaseError> {
        self.execute("COMMIT").await.map(|_| ())
    }
    async fn rollback(&mut self) -> Result<(), DatabaseError> {
        self.execute("ROLLBACK").await.map(|_| ())
    }
    async fn release(mut self: Box<Self>) -> Result<(), DatabaseError> {
        self.cancellation.active.store(false, Ordering::SeqCst);
        let cancellation = self.cancellation.clone();
        let _guard = cancellation.gate.lock().await;
        // PG 当前不复用租约物理连接；即使取消网络请求迟到也不会影响下一租约。
        self.task.abort();
        // 等待本地连接future实际析构，取消导致的JoinError属于正常清理。
        let _ = (&mut self.task).await;
        Ok(())
    }
}

/// 缓存项控制元数据socket生命周期；外部仍持有Client时，断开也能终止后台连接。
struct PgMetadataClient {
    client: Arc<Client>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for PgMetadataClient {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub struct PgDriver {
    config: ConnectionConfig,
    connected: Arc<AtomicBool>,
    /// database -> Client（PG 没有 USE 语句，跨库只能另建连接，这里做惰性缓存）
    clients: Mutex<HashMap<String, PgMetadataClient>>,
    /// query_id -> 本次租约独有取消句柄；归还后旧句柄同步失效。
    cancel_tokens: CancelRegistry<Arc<dyn SessionCancellation>>,
}

impl PgDriver {
    pub async fn connect(config: &ConnectionConfig) -> Result<Self, DatabaseError> {
        let driver = Self {
            config: config.clone(),
            connected: Arc::new(AtomicBool::new(true)),
            clients: Mutex::new(HashMap::new()),
            cancel_tokens: CancelRegistry::new(),
        };
        // 立即建立默认库连接验证可用性
        driver.client_for(None).await?;
        Ok(driver)
    }

    fn ensure_connected(&self) -> Result<(), DatabaseError> {
        if self.connected.load(Ordering::SeqCst) {
            Ok(())
        } else {
            Err(DatabaseError::ConnectionError("连接已断开".into()))
        }
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
        self.ensure_connected()?;
        let db_name = database
            .filter(|d| !d.is_empty())
            .or(self.config.database.as_deref().filter(|d| !d.is_empty()))
            .unwrap_or("postgres")
            .to_string();

        let mut guard = self.clients.lock().await;
        self.ensure_connected()?;
        if let Some(cached) = guard.get(&db_name) {
            if !cached.client.is_closed() {
                return Ok(Arc::clone(&cached.client));
            }
        }

        let (client, connection) = self
            .pg_config(&db_name)
            .connect(NoTls)
            .await
            .map_err(|e| DatabaseError::ConnectionError(e.to_string()))?;

        // 等待建连期间可能已断开；未启动后台任务、未写入缓存时直接丢弃。
        self.ensure_connected()?;
        let task = tokio::spawn(async move {
            if let Err(e) = connection.await {
                tracing::error!(error = %e, "PostgreSQL 连接后台任务出错");
            }
        });

        let client = Arc::new(client);
        guard.insert(
            db_name,
            PgMetadataClient {
                client: Arc::clone(&client),
                task,
            },
        );
        Ok(client)
    }

    /// pg_proc 列表（prokind: 'p' = 存储过程, 'f' = 普通函数；需 PG 11+）
    async fn get_routines(
        &self,
        database: Option<&str>,
        prokind: &str,
    ) -> Result<Vec<TableInfo>, DatabaseError> {
        let client = self.client_for(database).await?;
        let rows = client
            .query(
                "SELECT p.proname, pg_get_function_arguments(p.oid) \
             FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace \
             WHERE n.nspname = 'public' AND p.prokind = $1 ORDER BY p.proname",
                &[&prokind],
            )
            .await
            .map_err(map_pg_query_error)?;

        Ok(rows
            .iter()
            .map(|row| {
                let name: String = row.get(0);
                let args: String = row.get(1);
                TableInfo {
                    name,
                    schema: Some("public".to_string()),
                    row_count: None,
                    engine: None,
                    comment: if args.is_empty() {
                        None
                    } else {
                        Some(format!("({})", args))
                    },
                }
            })
            .collect())
    }

    /// pg_get_functiondef 对函数和存储过程都适用（重载时取第一个匹配）
    async fn get_routine_definition(
        &self,
        database: Option<&str>,
        prokind: &str,
        name: &str,
    ) -> Result<String, DatabaseError> {
        let client = self.client_for(database).await?;
        let row = client
            .query_opt(
                "SELECT pg_get_functiondef(p.oid) \
             FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace \
             WHERE n.nspname = 'public' AND p.proname = $1 AND p.prokind = $2 LIMIT 1",
                &[&name, &prokind],
            )
            .await
            .map_err(map_pg_query_error)?;
        let def: Option<String> = row.and_then(|r| r.get(0));
        def.ok_or_else(|| DatabaseError::Unknown(format!("过程/函数 {} 不存在", name)))
    }
}

pub async fn test_connection(config: &ConnectionConfig) -> Result<(), DatabaseError> {
    let db = config
        .database
        .as_deref()
        .filter(|d| !d.is_empty())
        .unwrap_or("postgres");

    let mut cfg = Config::new();
    cfg.host(&config.host)
        .port(config.port)
        .user(&config.user)
        .dbname(db);
    if !config.password.is_empty() {
        cfg.password(&config.password);
    }

    let (client, connection) = cfg
        .connect(NoTls)
        .await
        .map_err(|e| DatabaseError::ConnectionError(e.to_string()))?;

    tokio::spawn(async move {
        let _ = connection.await;
    });

    client
        .query_one("SELECT 1", &[])
        .await
        .map_err(|e| DatabaseError::ConnectionError(e.to_string()))?;

    Ok(())
}

#[async_trait]
impl DatabaseDriver for PgDriver {
    fn session_capabilities(&self) -> SessionCapabilities {
        SessionCapabilities::supported()
    }
    async fn lease_session(
        &self,
        database: Option<&str>,
    ) -> Result<Box<dyn SessionLease>, DatabaseError> {
        Ok(Box::new(self.new_lease(database).await?))
    }

    fn driver_type(&self) -> &'static str {
        "postgres"
    }

    fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    async fn query_typed(
        &self,
        sql: &str,
        params: &[TypedValue],
        database: Option<&str>,
    ) -> Result<TypedQueryResult, DatabaseError> {
        let mut lease = self.new_lease(database).await?;

        let result = lease.query_typed(sql, params).await;
        let cleanup = Box::new(lease).release().await;
        match result {
            Ok(value) => {
                cleanup?;
                Ok(value)
            }
            Err(error) => Err(error),
        }
    }

    async fn execute_bound(
        &self,
        sql: &str,
        params: &[TypedValue],
        database: Option<&str>,
    ) -> Result<u64, DatabaseError> {
        let mut lease = self.new_lease(database).await?;

        let result = lease.execute_bound(sql, params).await;
        let cleanup = Box::new(lease).release().await;
        match result {
            Ok(value) => {
                cleanup?;
                Ok(value)
            }
            Err(error) => Err(error),
        }
    }

    async fn query(
        &self,
        sql: &str,
        database: Option<&str>,
        query_id: Option<u64>,
    ) -> Result<QueryResult, DatabaseError> {
        let mut lease = self.new_lease(database).await?;
        let _registration = query_id.map(|qid| {
            self.cancel_tokens
                .register(qid, lease.cancel_handle().expect("租约支持取消"))
        });
        let result = lease.query(sql).await;
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
        database: Option<&str>,
        query_id: Option<u64>,
    ) -> Result<QueryResult, DatabaseError> {
        let mut lease = self.new_lease(database).await?;
        let _registration = query_id.map(|qid| {
            self.cancel_tokens
                .register(qid, lease.cancel_handle().expect("租约支持取消"))
        });
        let result = lease.execute(sql).await;
        let cleanup = Box::new(lease).release().await;
        match result {
            Ok(value) => {
                cleanup?;
                Ok(value)
            }
            Err(error) => Err(error),
        }
    }

    async fn cancel_query(&self, query_id: u64) -> Result<(), DatabaseError> {
        let handle = self
            .cancel_tokens
            .get(query_id)
            .ok_or_else(|| DatabaseError::Unknown("查询不存在或已结束".into()))?;
        handle.cancel().await
    }

    async fn get_databases(&self) -> Result<Vec<String>, DatabaseError> {
        let result = self
            .query(
                "SELECT datname FROM pg_database WHERE NOT datistemplate ORDER BY datname",
                None,
                None,
            )
            .await?;
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
        let client = self.client_for(database).await?;
        // reltuples 是优化器估算值，够用且快；obj_description 取表注释
        let rows = client
            .query(
                "SELECT c.relname, GREATEST(c.reltuples, 0)::bigint, obj_description(c.oid), n.nspname \
             FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname <> 'information_schema' AND n.nspname !~ '^pg_' \
               AND c.relkind IN ('r', 'p', 'f') AND has_schema_privilege(n.oid, 'USAGE') \
               AND has_table_privilege(c.oid, 'SELECT,INSERT,UPDATE,DELETE,TRUNCATE,REFERENCES,TRIGGER') \
             ORDER BY n.nspname, c.relname",
                &[],
            )
            .await
            .map_err(map_pg_query_error)?;

        let tables = rows
            .iter()
            .map(|row| {
                let name: String = row.try_get(0).map_err(map_pg_query_error)?;
                let row_count: i64 = row.try_get(1).map_err(map_pg_query_error)?;
                let comment: Option<String> = row.try_get(2).map_err(map_pg_query_error)?;
                Ok(TableInfo {
                    name,
                    schema: Some(row.try_get(3).map_err(map_pg_query_error)?),
                    row_count: Some(row_count),
                    engine: None,
                    comment,
                })
            })
            .collect::<Result<Vec<_>, DatabaseError>>()?;
        Ok(tables)
    }

    async fn get_views(&self, database: Option<&str>) -> Result<Vec<TableInfo>, DatabaseError> {
        let client = self.client_for(database).await?;
        // relkind: v = 普通视图, m = 物化视图
        let rows = client
            .query(
                "SELECT c.relname, CASE c.relkind WHEN 'm' THEN 'materialized' ELSE 'view' END, n.nspname \
             FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname <> 'information_schema' AND n.nspname !~ '^pg_' \
               AND c.relkind IN ('v', 'm') AND has_schema_privilege(n.oid, 'USAGE') \
               AND has_table_privilege(c.oid, 'SELECT,INSERT,UPDATE,DELETE,TRUNCATE,REFERENCES,TRIGGER') \
             ORDER BY n.nspname, c.relname",
                &[],
            )
            .await
            .map_err(map_pg_query_error)?;

        Ok(rows
            .iter()
            .map(|row| {
                let name: String = row.try_get(0).map_err(map_pg_query_error)?;
                let kind: &str = row.try_get(1).map_err(map_pg_query_error)?;
                Ok(TableInfo {
                    name,
                    schema: Some(row.try_get(2).map_err(map_pg_query_error)?),
                    row_count: None,
                    engine: Some(kind.to_string()),
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
        let client = self.client_for(Some(&target.database)).await?;
        let row = client
            .query_opt(
                "SELECT pg_catalog.pg_get_viewdef(c.oid, true) FROM pg_catalog.pg_class c \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = $1 AND c.relname = $2 AND c.relkind IN ('v', 'm')",
                &[&schema, &target.table],
            )
            .await
            .map_err(map_pg_query_error)?
            .ok_or_else(|| DatabaseError::QueryError("未找到目标视图".into()))?;
        row.try_get(0).map_err(map_pg_query_error)
    }

    async fn get_triggers(&self, database: Option<&str>) -> Result<Vec<TableInfo>, DatabaseError> {
        let client = self.client_for(database).await?;
        // tgisinternal = false 排除外键等约束内部触发器
        let rows = client
            .query(
                "SELECT t.tgname, c.relname \
             FROM pg_trigger t \
             JOIN pg_class c ON c.oid = t.tgrelid \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = 'public' AND NOT t.tgisinternal ORDER BY t.tgname",
                &[],
            )
            .await
            .map_err(map_pg_query_error)?;

        Ok(rows
            .iter()
            .map(|row| {
                let name: String = row.get(0);
                let table: String = row.get(1);
                TableInfo {
                    name,
                    schema: Some("public".to_string()),
                    row_count: None,
                    engine: None,
                    comment: Some(format!("ON {}", table)),
                }
            })
            .collect())
    }

    async fn get_trigger_definition(
        &self,
        database: Option<&str>,
        trigger: &str,
    ) -> Result<String, DatabaseError> {
        let client = self.client_for(database).await?;
        let row = client
            .query_opt(
                "SELECT pg_get_triggerdef(t.oid) \
             FROM pg_trigger t \
             JOIN pg_class c ON c.oid = t.tgrelid \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = 'public' AND t.tgname = $1 AND NOT t.tgisinternal LIMIT 1",
                &[&trigger],
            )
            .await
            .map_err(map_pg_query_error)?;
        let def: Option<String> = row.and_then(|r| r.get(0));
        def.ok_or_else(|| DatabaseError::Unknown(format!("触发器 {} 不存在", trigger)))
    }

    async fn get_procedures(
        &self,
        database: Option<&str>,
    ) -> Result<Vec<TableInfo>, DatabaseError> {
        self.get_routines(database, "p").await
    }

    async fn get_functions(&self, database: Option<&str>) -> Result<Vec<TableInfo>, DatabaseError> {
        self.get_routines(database, "f").await
    }

    async fn get_procedure_definition(
        &self,
        database: Option<&str>,
        procedure: &str,
    ) -> Result<String, DatabaseError> {
        self.get_routine_definition(database, "p", procedure).await
    }

    async fn get_function_definition(
        &self,
        database: Option<&str>,
        function: &str,
    ) -> Result<String, DatabaseError> {
        self.get_routine_definition(database, "f", function).await
    }

    async fn get_users(&self, database: Option<&str>) -> Result<Vec<DbUserInfo>, DatabaseError> {
        let client = self.client_for(database).await?;
        let rows = client
            .query(
                "SELECT rolname, rolsuper FROM pg_roles WHERE rolcanlogin ORDER BY rolname",
                &[],
            )
            .await
            .map_err(map_pg_query_error)?;
        Ok(rows
            .iter()
            .map(|row| {
                let name: String = row.get(0);
                let is_super: bool = row.get(1);
                DbUserInfo {
                    name,
                    host: None,
                    note: if is_super {
                        Some("超级用户".to_string())
                    } else {
                        None
                    },
                }
            })
            .collect())
    }

    async fn get_current_user(
        &self,
        database: Option<&str>,
    ) -> Result<CurrentUserInfo, DatabaseError> {
        let client = self.client_for(database).await?;
        let row = client
            .query_one("SELECT current_user", &[])
            .await
            .map_err(map_pg_query_error)?;
        let user: String = row.get(0);

        let mut grants: Vec<String> = Vec::new();
        // 角色成员关系
        let rows = client
            .query(
                "SELECT '成员角色: ' || r.rolname FROM pg_auth_members m \
             JOIN pg_roles r ON r.oid = m.roleid \
             WHERE m.member = (SELECT oid FROM pg_roles WHERE rolname = current_user)",
                &[],
            )
            .await
            .map_err(map_pg_query_error)?;
        for r in &rows {
            grants.push(r.get(0));
        }
        // 表级权限
        let rows = client
            .query(
                "SELECT 'GRANT ' || privilege_type || ' ON ' || table_schema || '.' || table_name \
             FROM information_schema.role_table_grants WHERE grantee = current_user \
             ORDER BY table_schema, table_name LIMIT 200",
                &[],
            )
            .await
            .map_err(map_pg_query_error)?;
        for r in &rows {
            grants.push(r.get(0));
        }
        Ok(CurrentUserInfo { user, grants })
    }

    async fn resolve_table(
        &self,
        database: &str,
        table_parts: &[String],
    ) -> Result<ResolvedTable, DatabaseError> {
        let (requested_database, relation_parts) = match table_parts {
            [_] | [_, _] => (None, table_parts),
            [db, _, _] => (Some(db.as_str()), &table_parts[1..]),
            _ => {
                return Err(DatabaseError::QueryError(
                    "PostgreSQL 表名需要一至三段标识符".into(),
                ))
            }
        };
        if database.is_empty() || table_parts.iter().any(String::is_empty) {
            return Err(DatabaseError::QueryError(
                "数据库名和表名各段不能为空".into(),
            ));
        }
        // 每段重新加引号，保留大小写、点号和引号本身；一段名称交由当前 search_path 解析。
        let relation = relation_parts
            .iter()
            .map(|part| format!("\"{}\"", self.escape_identifier(part)))
            .collect::<Vec<_>>()
            .join(".");
        let client = self.client_for(Some(database)).await?;
        let row = client
            .query_opt(
                "SELECT current_database(), n.nspname, c.relname, c.relkind::text \
             FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             WHERE c.oid = pg_catalog.to_regclass($1) AND c.relkind IN ('r', 'p', 'f', 'v', 'm') \
             AND ($2::text IS NULL OR $2 = current_database())",
                &[&relation, &requested_database],
            )
            .await
            .map_err(map_pg_query_error)?
            .ok_or_else(|| {
                DatabaseError::QueryError("未找到目标表，或不支持跨数据库限定名".into())
            })?;
        let database: String = row.try_get(0).map_err(map_pg_query_error)?;
        let schema: String = row.try_get(1).map_err(map_pg_query_error)?;
        let table: String = row.try_get(2).map_err(map_pg_query_error)?;
        let relkind: String = row.try_get(3).map_err(map_pg_query_error)?;
        let object_kind = match relkind.as_str() {
            "r" | "p" => "table",
            "v" | "m" => "view",
            _ => "unknown",
        };
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
            .ok_or_else(|| DatabaseError::QueryError("PostgreSQL 元数据目标缺少 schema".into()))?;
        let client = self.client_for(Some(&target.database)).await?;
        // 目录里的 relid 和序位是唯一依据；表达式通过服务端函数读取，不解析 SQL 文本。
        let rows = client
            .query(
                "SELECT ic.relname::text, i.indisunique, i.indisprimary, \
                    k.position::integer, k.attnum::integer, a.attname::text, \
                    COALESCE((to_jsonb(i)->>'indnkeyatts')::integer, i.indnatts::integer), \
                    pg_catalog.pg_get_indexdef(i.indexrelid, k.position::integer, true), \
                    (COALESCE(i.indoption[(k.position - 1)::integer], 0)::integer & 1) = 1, \
                    pg_catalog.pg_get_indexdef(i.indexrelid), \
                    pg_catalog.pg_get_expr(i.indpred, i.indrelid) \
             FROM pg_catalog.pg_index i \
             JOIN pg_catalog.pg_class tc ON tc.oid = i.indrelid \
             JOIN pg_catalog.pg_namespace ns ON ns.oid = tc.relnamespace \
             JOIN pg_catalog.pg_class ic ON ic.oid = i.indexrelid \
             CROSS JOIN LATERAL unnest(i.indkey) WITH ORDINALITY k(attnum, position) \
             LEFT JOIN pg_catalog.pg_attribute a ON a.attrelid = tc.oid AND a.attnum = k.attnum \
             WHERE ns.nspname = $1 AND tc.relname = $2 \
             ORDER BY i.indexrelid, k.position",
                &[&schema, &target.table],
            )
            .await
            .map_err(map_pg_query_error)?;
        let mut indexes: Vec<IndexInfo> = Vec::new();
        for row in rows {
            let name: String = row.try_get(0).map_err(map_pg_query_error)?;
            if indexes.last().is_none_or(|index| index.name != name) {
                indexes.push(IndexInfo {
                    name,
                    columns: Vec::new(),
                    is_unique: row.try_get(1).map_err(map_pg_query_error)?,
                    is_primary: row.try_get(2).map_err(map_pg_query_error)?,
                    key_parts: Vec::new(),
                    included_columns: Vec::new(),
                    definition: row.try_get(9).map_err(map_pg_query_error)?,
                    predicate: row.try_get(10).map_err(map_pg_query_error)?,
                });
            }
            let index = indexes
                .last_mut()
                .ok_or_else(|| DatabaseError::QueryError("索引分组状态无效".into()))?;
            let position: i32 = row.try_get(3).map_err(map_pg_query_error)?;
            let attnum: i32 = row.try_get(4).map_err(map_pg_query_error)?;
            let column: Option<String> = row.try_get(5).map_err(map_pg_query_error)?;
            let key_count: i32 = row.try_get(6).map_err(map_pg_query_error)?;
            if position > key_count {
                index.included_columns.push(
                    column.ok_or_else(|| DatabaseError::QueryError("包含列缺少真实列名".into()))?,
                );
            } else {
                let expression = if attnum == 0 {
                    Some(row.try_get::<_, String>(7).map_err(map_pg_query_error)?)
                } else {
                    let name = column
                        .as_ref()
                        .ok_or_else(|| DatabaseError::QueryError("索引键缺少真实列名".into()))?;
                    index.columns.push(name.clone());
                    None
                };
                index.key_parts.push(IndexKeyPart {
                    column,
                    expression,
                    descending: row.try_get(8).map_err(map_pg_query_error)?,
                });
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
            .ok_or_else(|| DatabaseError::QueryError("PostgreSQL 元数据目标缺少 schema".into()))?;
        let client = self.client_for(Some(&target.database)).await?;
        // conkey/confkey 按相同序号配对，不能把复合外键两端展开成笛卡尔积。
        let rows = client.query(
            "SELECT con.conname::text, pa.attname::text, rn.nspname::text, \
                    rc.relname::text, ra.attname::text, \
                    CASE con.confupdtype WHEN 'a' THEN 'NO ACTION' WHEN 'r' THEN 'RESTRICT' \
                      WHEN 'c' THEN 'CASCADE' WHEN 'n' THEN 'SET NULL' WHEN 'd' THEN 'SET DEFAULT' END, \
                    CASE con.confdeltype WHEN 'a' THEN 'NO ACTION' WHEN 'r' THEN 'RESTRICT' \
                      WHEN 'c' THEN 'CASCADE' WHEN 'n' THEN 'SET NULL' WHEN 'd' THEN 'SET DEFAULT' END \
             FROM pg_catalog.pg_constraint con \
             JOIN pg_catalog.pg_class pc ON pc.oid = con.conrelid \
             JOIN pg_catalog.pg_namespace pn ON pn.oid = pc.relnamespace \
             JOIN pg_catalog.pg_class rc ON rc.oid = con.confrelid \
             JOIN pg_catalog.pg_namespace rn ON rn.oid = rc.relnamespace \
             CROSS JOIN LATERAL unnest(con.conkey) WITH ORDINALITY pk(attnum, position) \
             JOIN LATERAL unnest(con.confkey) WITH ORDINALITY rk(attnum, position) ON rk.position = pk.position \
             JOIN pg_catalog.pg_attribute pa ON pa.attrelid = pc.oid AND pa.attnum = pk.attnum \
             JOIN pg_catalog.pg_attribute ra ON ra.attrelid = rc.oid AND ra.attnum = rk.attnum \
             WHERE pn.nspname = $1 AND pc.relname = $2 AND con.contype = 'f' \
             ORDER BY con.oid, pk.position",
            &[&schema, &target.table],
        ).await.map_err(map_pg_query_error)?;
        let mut foreign_keys: Vec<ForeignKeyInfo> = Vec::new();
        for row in rows {
            let name: String = row.try_get(0).map_err(map_pg_query_error)?;
            if foreign_keys.last().is_none_or(|key| key.name != name) {
                foreign_keys.push(ForeignKeyInfo {
                    name,
                    ref_database: Some(target.database.clone()),
                    columns: Vec::new(),
                    ref_schema: Some(row.try_get(2).map_err(map_pg_query_error)?),
                    ref_table: row.try_get(3).map_err(map_pg_query_error)?,
                    ref_columns: Vec::new(),
                    on_update: row.try_get(5).map_err(map_pg_query_error)?,
                    on_delete: row.try_get(6).map_err(map_pg_query_error)?,
                });
            }
            let key = foreign_keys
                .last_mut()
                .ok_or_else(|| DatabaseError::QueryError("外键分组状态无效".into()))?;
            key.columns
                .push(row.try_get(1).map_err(map_pg_query_error)?);
            key.ref_columns
                .push(row.try_get(4).map_err(map_pg_query_error)?);
        }
        Ok(foreign_keys)
    }

    async fn execute_batch(
        &self,
        sqls: Vec<String>,
        database: Option<&str>,
    ) -> Result<u64, DatabaseError> {
        let mut lease = self.lease_session(database).await?;
        lease.begin().await?;
        let mut total = 0;
        for sql in sqls {
            total += lease.execute(&sql).await?.rows_affected.unwrap_or(0);
        }
        lease.commit().await?;
        lease.release().await?;
        Ok(total)
    }

    async fn disconnect(&self) -> Result<(), DatabaseError> {
        self.connected.store(false, Ordering::SeqCst);
        let clients = std::mem::take(&mut *self.clients.lock().await);
        for (_, mut cached) in clients {
            cached.task.abort();
            let _ = (&mut cached.task).await;
        }
        Ok(())
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
            "\"{}\".\"{}\"",
            self.escape_identifier(schema),
            self.escape_identifier(&target.table)
        );
        let mut out = String::from("-- SQLink 表定义\n\n");

        if include_drop {
            out.push_str(&format!("DROP TABLE IF EXISTS {quoted};\n\n"));
        }

        // 从同一个已解析对象获取列定义，避免 DDL 与数据命中不同 schema。
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
                let mut d = format!("\"{}\" {}", self.escape_identifier(&c.name), c.data_type);
                if let Some(def) = &c.default_value {
                    d.push_str(&format!(" DEFAULT {}", def));
                }
                if !c.nullable {
                    d.push_str(" NOT NULL");
                }
                d
            })
            .collect();
        let pk: Vec<String> = cols
            .iter()
            .filter(|c| c.is_primary_key)
            .map(|c| format!("\"{}\"", self.escape_identifier(&c.name)))
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

    fn escape_identifier(&self, identifier: &str) -> String {
        identifier.replace('"', "\"\"")
    }
}

fn pg_type_error(message: impl Into<String>) -> DatabaseError {
    DatabaseError::TypeError(message.into())
}

/// 先核对服务器预编译的实际参数类型，再交给 PostgreSQL 二进制参数协议。
#[derive(Debug)]
struct PgBound {
    ty: Type,
    bytes: Option<Vec<u8>>,
}

impl tokio_postgres::types::ToSql for PgBound {
    fn to_sql(
        &self,
        ty: &Type,
        out: &mut bytes::BytesMut,
    ) -> Result<tokio_postgres::types::IsNull, Box<dyn std::error::Error + Sync + Send>> {
        if ty != &self.ty {
            return Err("PostgreSQL 参数类型与预编译结果不一致".into());
        }
        match &self.bytes {
            None => Ok(tokio_postgres::types::IsNull::Yes),
            Some(bytes) => {
                out.extend_from_slice(bytes);
                Ok(tokio_postgres::types::IsNull::No)
            }
        }
    }
    fn accepts(_: &Type) -> bool {
        true
    }
    tokio_postgres::types::to_sql_checked!();
}

fn pg_encode<T: tokio_postgres::types::ToSql>(
    value: T,
    ty: &Type,
) -> Result<Vec<u8>, DatabaseError> {
    let mut bytes = bytes::BytesMut::new();
    value
        .to_sql_checked(ty, &mut bytes)
        .map_err(|_| pg_type_error("PostgreSQL 参数不能无损编码为目标类型"))?;
    Ok(bytes.to_vec())
}

fn pg_supported_type(ty: &Type) -> bool {
    matches!(
        *ty,
        Type::BOOL
            | Type::INT2
            | Type::INT4
            | Type::INT8
            | Type::NUMERIC
            | Type::FLOAT4
            | Type::FLOAT8
            | Type::TEXT
            | Type::VARCHAR
            | Type::BPCHAR
            | Type::NAME
            | Type::BYTEA
            | Type::DATE
            | Type::TIME
            | Type::TIMESTAMP
            | Type::TIMESTAMPTZ
    )
}

fn pg_bind_params(params: &[TypedValue], types: &[Type]) -> Result<Vec<PgBound>, DatabaseError> {
    if params.len() != types.len() {
        return Err(pg_type_error("PostgreSQL 参数数量与预编译结果不一致"));
    }
    params
        .iter()
        .zip(types)
        .map(|(value, ty)| {
            if !pg_supported_type(ty) {
                return Err(pg_type_error(format!(
                    "PostgreSQL 不支持无损绑定参数类型 {ty}"
                )));
            }
            let bytes = match value {
                TypedValue::Null => None,
                TypedValue::Text(v)
                    if matches!(*ty, Type::TEXT | Type::VARCHAR | Type::BPCHAR | Type::NAME) =>
                {
                    Some(pg_encode(v, ty)?)
                }
                TypedValue::Boolean(v) if *ty == Type::BOOL => Some(pg_encode(v, ty)?),
                TypedValue::Integer(v) | TypedValue::UnsignedInteger(v)
                    if matches!(*ty, Type::INT2 | Type::INT4 | Type::INT8 | Type::NUMERIC) =>
                {
                    let n = v
                        .parse::<i128>()
                        .map_err(|_| pg_type_error("PostgreSQL 整数超出支持范围"))?;
                    Some(match *ty {
                        Type::INT2 => pg_encode(
                            i16::try_from(n).map_err(|_| pg_type_error("整数超出 int2 范围"))?,
                            ty,
                        )?,
                        Type::INT4 => pg_encode(
                            i32::try_from(n).map_err(|_| pg_type_error("整数超出 int4 范围"))?,
                            ty,
                        )?,
                        Type::INT8 => pg_encode(
                            i64::try_from(n).map_err(|_| pg_type_error("整数超出 int8 范围"))?,
                            ty,
                        )?,
                        _ => pg_numeric_encode(v),
                    })
                }
                TypedValue::Decimal(v) if *ty == Type::NUMERIC => Some(pg_numeric_encode(v)),
                TypedValue::Float(v) if *ty == Type::FLOAT8 => Some(pg_encode(v, ty)?),
                TypedValue::Float(v) if *ty == Type::FLOAT4 && (*v as f32) as f64 == *v => {
                    Some(pg_encode(*v as f32, ty)?)
                }
                TypedValue::Binary(_) if *ty == Type::BYTEA => Some(value.binary_bytes()?),
                TypedValue::Date(v) if *ty == Type::DATE => Some(pg_encode(
                    chrono::NaiveDate::parse_from_str(v, "%Y-%m-%d")
                        .map_err(|_| pg_type_error("日期无效"))?,
                    ty,
                )?),
                TypedValue::Time(v) if *ty == Type::TIME && v.precision <= 6 => Some(pg_encode(
                    chrono::NaiveTime::parse_from_str(&v.value, "%H:%M:%S%.f")
                        .map_err(|_| pg_type_error("时间无效"))?,
                    ty,
                )?),
                TypedValue::Timestamp(v)
                    if *ty == Type::TIMESTAMP
                        && v.timezone == TimezoneKind::None
                        && v.precision <= 6 =>
                {
                    Some(pg_encode(
                        chrono::NaiveDateTime::parse_from_str(&v.value, "%Y-%m-%dT%H:%M:%S%.f")
                            .map_err(|_| pg_type_error("时间戳无效"))?,
                        ty,
                    )?)
                }
                TypedValue::Timestamp(v)
                    if *ty == Type::TIMESTAMPTZ
                        && v.timezone == TimezoneKind::Offset
                        && v.precision <= 6 =>
                {
                    Some(pg_encode(
                        chrono::DateTime::parse_from_rfc3339(&v.value)
                            .map_err(|_| pg_type_error("带时区时间戳无效"))?,
                        ty,
                    )?)
                }
                _ => {
                    return Err(pg_type_error(format!(
                        "参数与 PostgreSQL 类型 {ty} 不兼容或无法保留精度"
                    )))
                }
            };
            Ok(PgBound {
                ty: ty.clone(),
                bytes,
            })
        })
        .collect()
}

/// NUMERIC wire format 为 base-10000，不经浮点数，也不使用 SQL 文本转换。
fn pg_numeric_encode(value: &str) -> Vec<u8> {
    let unsigned = value.strip_prefix('-').unwrap_or(value);
    let (integer, fraction) = unsigned.split_once('.').unwrap_or((unsigned, ""));
    let padded_integer = format!("{}{}", "0".repeat((4 - integer.len() % 4) % 4), integer);
    let padded_fraction = format!("{}{}", fraction, "0".repeat((4 - fraction.len() % 4) % 4));
    let mut groups: Vec<u16> = format!("{padded_integer}{padded_fraction}")
        .as_bytes()
        .chunks(4)
        .map(|s| std::str::from_utf8(s).unwrap().parse().unwrap())
        .collect();
    let mut weight = padded_integer.len() as i16 / 4 - 1;
    let leading = groups.iter().take_while(|v| **v == 0).count();
    groups.drain(..leading);
    weight -= leading as i16;
    while groups.last() == Some(&0) {
        groups.pop();
    }
    if groups.is_empty() {
        weight = 0;
    }
    let mut bytes = Vec::new();
    for word in [
        groups.len() as i16,
        weight,
        if value.starts_with('-') { 0x4000 } else { 0 },
        fraction.len() as i16,
    ] {
        bytes.extend_from_slice(&word.to_be_bytes());
    }
    for group in groups {
        bytes.extend_from_slice(&group.to_be_bytes());
    }
    bytes
}

fn pg_numeric_decode(raw: &[u8]) -> Option<String> {
    if raw.len() < 8 || raw.len() % 2 != 0 {
        return None;
    }
    let words: Vec<u16> = raw
        .chunks_exact(2)
        .map(|v| u16::from_be_bytes([v[0], v[1]]))
        .collect();
    let count = words[0] as usize;
    let weight = words[1] as i16 as i32;
    let scale = words[3] as usize;
    if count + 4 != words.len()
        || !matches!(words[2], 0 | 0x4000)
        || scale > 1024
        || weight > 256
        || words[4..].iter().any(|v| *v >= 10000)
    {
        return None;
    }
    let group = |position: i32| -> u16 {
        let i = weight - position;
        if i < 0 {
            0
        } else {
            words.get(4 + i as usize).copied().unwrap_or(0)
        }
    };
    let mut value = if weight < 0 {
        "0".into()
    } else {
        let mut s = group(weight).to_string();
        for p in (0..weight).rev() {
            s.push_str(&format!("{:04}", group(p)));
        }
        s
    };
    if scale > 0 {
        value.push('.');
        let mut fraction = String::new();
        for p in 1..=scale.div_ceil(4) {
            fraction.push_str(&format!("{:04}", group(-(p as i32))));
        }
        value.push_str(&fraction[..scale]);
    }
    if words[2] == 0x4000 && value.bytes().any(|b| b != b'0' && b != b'.') {
        value.insert(0, '-');
    }
    crate::typed_value::valid_decimal(&value).then_some(value)
}

/// 只读取长度，避免预算检查为了获取字节数先复制超大单元格。
struct PgRawLength(usize);
impl<'a> tokio_postgres::types::FromSql<'a> for PgRawLength {
    fn from_sql(_: &Type, raw: &'a [u8]) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        Ok(Self(raw.len()))
    }
    fn accepts(_: &Type) -> bool {
        true
    }
}

fn pg_raw_row_fits(budget: &QueryBudget, row: &tokio_postgres::Row) -> Result<bool, DatabaseError> {
    // 协议解码器已经接收单个 wire row；这里只能阻止后续复制、类型转换和结果留存。
    let lengths = (0..row.len())
        .map(|index| {
            row.try_get::<_, Option<PgRawLength>>(index)
                .map(|length| length.map_or(0, |length| length.0))
                .map_err(map_pg_query_error)
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(budget.allows_raw_row(lengths))
}

struct PgRaw(Vec<u8>);
impl<'a> tokio_postgres::types::FromSql<'a> for PgRaw {
    fn from_sql(_: &Type, raw: &'a [u8]) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        Ok(Self(raw.to_vec()))
    }
    fn accepts(_: &Type) -> bool {
        true
    }
}

fn pg_typed_value(row: &tokio_postgres::Row, idx: usize, ty: &Type) -> Option<TypedValue> {
    let raw = row.try_get::<_, Option<PgRaw>>(idx).ok()?;
    let Some(raw) = raw else {
        return Some(TypedValue::Null);
    };
    macro_rules! read {
        ($t:ty) => {
            row.try_get::<_, $t>(idx).ok()?
        };
    }
    let value = match *ty {
        Type::BOOL => TypedValue::Boolean(read!(bool)),
        Type::INT2 => TypedValue::Integer(read!(i16).to_string()),
        Type::INT4 => TypedValue::Integer(read!(i32).to_string()),
        Type::INT8 => TypedValue::Integer(read!(i64).to_string()),
        Type::NUMERIC => TypedValue::Decimal(pg_numeric_decode(&raw.0)?),
        Type::FLOAT4 => TypedValue::Float(read!(f32) as f64),
        Type::FLOAT8 => TypedValue::Float(read!(f64)),
        Type::TEXT | Type::VARCHAR | Type::BPCHAR | Type::NAME => TypedValue::Text(read!(String)),
        Type::BYTEA => TypedValue::binary(&raw.0),
        Type::DATE => TypedValue::Date(read!(chrono::NaiveDate).format("%Y-%m-%d").to_string()),
        Type::TIME => TypedValue::Time(TimeValue {
            value: read!(chrono::NaiveTime).format("%H:%M:%S%.6f").to_string(),
            precision: 6,
        }),
        Type::TIMESTAMP => TypedValue::Timestamp(TimestampValue {
            value: read!(chrono::NaiveDateTime)
                .format("%Y-%m-%dT%H:%M:%S%.6f")
                .to_string(),
            precision: 6,
            timezone: TimezoneKind::None,
        }),
        Type::TIMESTAMPTZ => TypedValue::Timestamp(TimestampValue {
            value: read!(chrono::DateTime<chrono::Utc>)
                .format("%Y-%m-%dT%H:%M:%S%.6fZ")
                .to_string(),
            precision: 6,
            timezone: TimezoneKind::Offset,
        }),
        _ => return None,
    };
    value.validate().ok().map(|_| value)
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
        Type::JSON | Type::JSONB => match row.try_get::<usize, Option<serde_json::Value>>(idx) {
            Ok(Some(v)) => return J::String(v.to_string()),
            Ok(None) => return J::Null,
            Err(_) => {}
        },
        Type::TIMESTAMP => match row.try_get::<usize, Option<chrono::NaiveDateTime>>(idx) {
            Ok(Some(v)) => return J::String(v.format("%Y-%m-%d %H:%M:%S").to_string()),
            Ok(None) => return J::Null,
            Err(_) => {}
        },
        Type::TIMESTAMPTZ => {
            match row.try_get::<usize, Option<chrono::DateTime<chrono::Utc>>>(idx) {
                Ok(Some(v)) => return J::String(v.format("%Y-%m-%d %H:%M:%S").to_string()),
                Ok(None) => return J::Null,
                Err(_) => {}
            }
        }
        Type::DATE => match row.try_get::<usize, Option<chrono::NaiveDate>>(idx) {
            Ok(Some(v)) => return J::String(v.format("%Y-%m-%d").to_string()),
            Ok(None) => return J::Null,
            Err(_) => {}
        },
        Type::TIME => match row.try_get::<usize, Option<chrono::NaiveTime>>(idx) {
            Ok(Some(v)) => return J::String(v.format("%H:%M:%S").to_string()),
            Ok(None) => return J::Null,
            Err(_) => {}
        },
        Type::BYTEA => match row.try_get::<usize, Option<Vec<u8>>>(idx) {
            Ok(Some(v)) => {
                return J::String(format!(
                    "\\x{}",
                    v.iter().map(|b| format!("{:02x}", b)).collect::<String>()
                ))
            }
            Ok(None) => return J::Null,
            Err(_) => {}
        },
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

impl PgDriver {
    async fn columns_for_schema(
        &self,
        database: Option<&str>,
        schema: &str,
        table: &str,
    ) -> Result<Vec<ColumnInfo>, DatabaseError> {
        let client = self.client_for(database).await?;
        let mut rows = client
            .query(
                "SELECT c.column_name, pg_catalog.format_type(a.atttypid, a.atttypmod), c.is_nullable, c.column_default, \
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
             ) AS comment, \
             CASE \
               WHEN COALESCE(to_jsonb(a)->>'attgenerated', '') <> '' THEN \
                 'generated ' || CASE to_jsonb(a)->>'attgenerated' \
                   WHEN 's' THEN 'stored' WHEN 'v' THEN 'virtual' ELSE 'unknown' END \
               WHEN COALESCE(to_jsonb(a)->>'attidentity', '') <> '' THEN \
                 'identity ' || CASE to_jsonb(a)->>'attidentity' \
                   WHEN 'a' THEN 'always' WHEN 'd' THEN 'by default' ELSE 'unknown' END \
               ELSE NULL END AS extra \
             FROM information_schema.columns c \
             LEFT JOIN pg_catalog.pg_attribute a \
               ON a.attrelid = format('%I.%I', c.table_schema, c.table_name)::regclass \
              AND a.attname = c.column_name AND NOT a.attisdropped \
             WHERE c.table_schema = $2 AND c.table_name = $1 \
             ORDER BY c.ordinal_position",
                &[&table, &schema],
            )
            .await
            .map_err(map_pg_query_error)?;

        // information_schema.columns 不列出物化视图，单独从目录读取其列。
        if rows.is_empty() {
            rows = client.query(
                "SELECT a.attname::text, pg_catalog.format_type(a.atttypid, a.atttypmod), \
                 CASE WHEN a.attnotnull THEN 'NO' ELSE 'YES' END, \
                 pg_catalog.pg_get_expr(d.adbin, d.adrelid), false, \
                 pg_catalog.col_description(c.oid, a.attnum), \
                 CASE \
                   WHEN COALESCE(to_jsonb(a)->>'attgenerated', '') <> '' THEN \
                     'generated ' || CASE to_jsonb(a)->>'attgenerated' \
                       WHEN 's' THEN 'stored' WHEN 'v' THEN 'virtual' ELSE 'unknown' END \
                   WHEN COALESCE(to_jsonb(a)->>'attidentity', '') <> '' THEN \
                     'identity ' || CASE to_jsonb(a)->>'attidentity' \
                       WHEN 'a' THEN 'always' WHEN 'd' THEN 'by default' ELSE 'unknown' END \
                   ELSE NULL END AS extra \
                 FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
                 JOIN pg_catalog.pg_attribute a ON a.attrelid = c.oid \
                 LEFT JOIN pg_catalog.pg_attrdef d ON d.adrelid = c.oid AND d.adnum = a.attnum \
                 WHERE n.nspname = $2 AND c.relname = $1 AND c.relkind = 'm' \
                 AND a.attnum > 0 AND NOT a.attisdropped ORDER BY a.attnum",
                &[&table, &schema],
            ).await.map_err(map_pg_query_error)?;
        }

        let columns = rows
            .iter()
            .map(|row| {
                let name: String = row.try_get(0).map_err(map_pg_query_error)?;
                let data_type: String = row.try_get(1).map_err(map_pg_query_error)?;
                let is_nullable: String = row.try_get(2).map_err(map_pg_query_error)?;
                let default: Option<String> = row.try_get(3).map_err(map_pg_query_error)?;
                let is_pk: bool = row.try_get(4).map_err(map_pg_query_error)?;
                let comment: Option<String> = row.try_get(5).map_err(map_pg_query_error)?;
                let extra: Option<String> = row.try_get(6).map_err(map_pg_query_error)?;
                Ok(ColumnInfo {
                    name,
                    data_type,
                    nullable: is_nullable == "YES",
                    is_primary_key: is_pk,
                    default_value: default,
                    extra,
                    comment,
                })
            })
            .collect::<Result<Vec<_>, DatabaseError>>()?;
        Ok(columns)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn postgres_wire_stub_stops_without_waiting_for_completion() {
        use crate::database::result_budget::MAX_QUERY_ROWS;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        fn frame(output: &mut Vec<u8>, tag: u8, body: &[u8]) {
            output.push(tag);
            output.extend_from_slice(&((body.len() + 4) as u32).to_be_bytes());
            output.extend_from_slice(body);
        }

        async fn read_until_sync(stream: &mut tokio::net::TcpStream) {
            loop {
                let tag = stream.read_u8().await.unwrap();
                let length = stream.read_u32().await.unwrap() as usize;
                assert!((4..65536).contains(&length));
                let mut body = vec![0; length - 4];
                stream.read_exact(&mut body).await.unwrap();
                if tag == b'S' {
                    break;
                }
            }
        }

        // 只启动随机端口的协议桩，不读取连接配置，也不访问任何已有数据库。
        for typed in [false, true] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let startup_length = stream.read_u32().await.unwrap() as usize;
                assert!((8..65536).contains(&startup_length));
                stream
                    .read_exact(&mut vec![0; startup_length - 4])
                    .await
                    .unwrap();
                let mut startup = Vec::new();
                frame(&mut startup, b'R', &0u32.to_be_bytes());
                frame(&mut startup, b'K', &[0, 0, 0, 1, 0, 0, 0, 2]);
                frame(&mut startup, b'Z', b"I");
                stream.write_all(&startup).await.unwrap();

                read_until_sync(&mut stream).await;
                let mut description = 1u16.to_be_bytes().to_vec();
                description.extend_from_slice(b"value\0");
                description.extend_from_slice(&0u32.to_be_bytes());
                description.extend_from_slice(&0u16.to_be_bytes());
                description.extend_from_slice(&23u32.to_be_bytes()); // INT4
                description.extend_from_slice(&4u16.to_be_bytes());
                description.extend_from_slice(&(-1i32).to_be_bytes());
                description.extend_from_slice(&0u16.to_be_bytes());
                let mut prepared = Vec::new();
                frame(&mut prepared, b'1', &[]);
                frame(&mut prepared, b't', &0u16.to_be_bytes());
                frame(&mut prepared, b'T', &description);
                frame(&mut prepared, b'Z', b"I");
                stream.write_all(&prepared).await.unwrap();

                read_until_sync(&mut stream).await;
                let mut rows = Vec::new();
                frame(&mut rows, b'2', &[]);
                for value in 0..=MAX_QUERY_ROWS {
                    let mut body = 1u16.to_be_bytes().to_vec();
                    body.extend_from_slice(&4u32.to_be_bytes());
                    body.extend_from_slice(&(value as i32).to_be_bytes());
                    frame(&mut rows, b'D', &body);
                }
                stream.write_all(&rows).await.unwrap();
                // 故意不发送 CommandComplete/ReadyForQuery；排空流的实现会挂在这里。
                let mut byte = [0];
                match stream.read(&mut byte).await {
                    Ok(0) => {}
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::ConnectionReset
                                | std::io::ErrorKind::ConnectionAborted
                        ) => {}
                    result => panic!("截断后物理连接未关闭：{result:?}"),
                }
            });
            let config: ConnectionConfig = serde_json::from_value(serde_json::json!({
                "name": "本地协议桩", "driverType": "postgres", "host": "127.0.0.1",
                "port": port, "user": "test", "password": "", "database": "test"
            }))
            .unwrap();
            let driver = PgDriver {
                config,
                connected: Arc::new(AtomicBool::new(true)),
                clients: Mutex::new(HashMap::new()),
                cancel_tokens: CancelRegistry::new(),
            };
            let mut lease = driver.new_lease(None).await.unwrap();
            let cancellation = lease.cancel_handle().unwrap();
            let result = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                if typed {
                    let result = lease.query_typed("SELECT 1", &[]).await.unwrap();
                    assert_eq!(result.typed_rows.len(), MAX_QUERY_ROWS);
                    assert_eq!(
                        result.typed_rows[0],
                        vec![Some(TypedValue::Integer("0".into()))]
                    );
                    result.result
                } else {
                    lease.query("SELECT 1").await.unwrap()
                }
            })
            .await
            .expect("流式截断不应等待完整响应");
            assert!(result.truncated);
            assert_eq!(result.rows.len(), MAX_QUERY_ROWS);
            assert!(lease.query("SELECT 2").await.is_err());
            assert!(cancellation.cancel().await.is_err());
            Box::new(lease).release().await.unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(5), server)
                .await
                .expect("协议桩未观察到连接关闭")
                .unwrap();
        }
    }

    #[test]
    fn raw_length_probe_rejects_oversized_cells_before_value_conversion() {
        use crate::database::result_budget::MAX_QUERY_CELL_BYTES;
        use tokio_postgres::types::FromSql;

        // 检查器不持有数据副本，也不依赖 BYTEA/TEXT 等具体 SQL 类型。
        assert_eq!(
            std::mem::size_of::<PgRawLength>(),
            std::mem::size_of::<usize>()
        );
        let raw = vec![b'a'; MAX_QUERY_CELL_BYTES + 1];
        let budget = QueryBudget::new(&[]).unwrap();
        for ty in [Type::BYTEA, Type::TEXT, Type::NUMERIC, Type::JSONB] {
            let length = PgRawLength::from_sql(&ty, &raw).unwrap().0;
            assert_eq!(length, raw.len());
            assert!(!budget.allows_raw_row([length]));
            assert!(budget.allows_raw_row([MAX_QUERY_CELL_BYTES]));
        }
        assert!(budget.allows_raw_row([0]));
    }

    #[tokio::test]
    async fn disconnected_driver_rejects_metadata_without_opening_a_socket() {
        let config: ConnectionConfig = serde_json::from_value(serde_json::json!({
            "name": "离线断开验证", "driverType": "postgres", "host": "不可访问测试主机.invalid",
            "port": 5432, "user": "", "password": ""
        }))
        .unwrap();
        let driver = PgDriver {
            config,
            connected: Arc::new(AtomicBool::new(false)),
            clients: Mutex::new(HashMap::new()),
            cancel_tokens: CancelRegistry::new(),
        };
        fn disconnected<T>(result: Result<T, DatabaseError>) {
            assert!(
                matches!(result, Err(DatabaseError::ConnectionError(ref message)) if message == "连接已断开")
            );
        }
        disconnected(driver.client_for(None).await);
        disconnected(driver.get_tables(None).await);
        disconnected(driver.get_columns(None, "不存在的表").await);
        disconnected(
            driver
                .resolve_table("postgres", &["不存在的表".into()])
                .await,
        );
        disconnected(driver.lease_session(None).await);
        assert!(driver.clients.lock().await.is_empty());
        // 锁等待期间发生断开，也必须在拿锁后拒绝，不能进入DNS/网络建连。
        driver.connected.store(true, Ordering::SeqCst);
        let guard = driver.clients.lock().await;
        let mut acquiring = Box::pin(driver.client_for(None));
        std::future::poll_fn(|cx| {
            assert!(acquiring.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        driver.connected.store(false, Ordering::SeqCst);
        drop(guard);
        disconnected(acquiring.await);
        assert!(driver.clients.lock().await.is_empty());
    }

    #[test]
    fn numeric_wire_is_exact_and_preserves_scale() {
        // PostgreSQL NUMERIC: ndigits=2, weight=0, sign=negative, dscale=4, groups=12/3456。
        let expected = vec![0, 2, 0, 0, 0x40, 0, 0, 4, 0, 12, 13, 128];
        assert_eq!(pg_numeric_encode("-12.3456"), expected);
        assert_eq!(pg_numeric_decode(&expected).as_deref(), Some("-12.3456"));
        for value in [
            "0",
            "0.0000",
            "-0.0001",
            "10000",
            "100000000",
            "0.000000001",
            "123456789012345678901.12345678901234567",
            "-99999999999999999999999999999999999999",
        ] {
            assert_eq!(
                pg_numeric_decode(&pg_numeric_encode(value)).as_deref(),
                Some(value)
            );
        }
        // numeric(p, -s) 在服务器按十位等舍入后，wire dscale=0、正 weight。
        assert_eq!(
            pg_numeric_decode(&[0, 1, 0, 1, 0, 0, 0, 0, 0, 12]).as_deref(),
            Some("120000")
        );
        for sign in [0xc000u16, 0xd000, 0xf000] {
            let mut raw = vec![0, 0, 0, 0];
            raw.extend_from_slice(&sign.to_be_bytes());
            raw.extend_from_slice(&[0, 0]);
            assert!(pg_numeric_decode(&raw).is_none());
        }
        assert!(pg_numeric_decode(&[0, 1, 0, 0, 0, 0, 0, 0, 0x27, 0x10]).is_none());
    }

    #[test]
    fn binding_checks_actual_statement_types_before_execution() {
        assert!(pg_bind_params(&[TypedValue::Integer("32768".into())], &[Type::INT2]).is_err());
        assert!(pg_bind_params(
            &[TypedValue::UnsignedInteger(u64::MAX.to_string())],
            &[Type::INT8]
        )
        .is_err());
        assert!(pg_bind_params(
            &[TypedValue::UnsignedInteger(u64::MAX.to_string())],
            &[Type::NUMERIC]
        )
        .is_ok());
        assert!(pg_bind_params(&[TypedValue::Null], &[Type::UUID]).is_err());
        assert!(pg_bind_params(&[TypedValue::Text("42".into())], &[Type::INT8]).is_err());
        assert!(pg_bind_params(&[TypedValue::Decimal("0.1".into())], &[Type::FLOAT8]).is_err());
        let null = pg_bind_params(&[TypedValue::Null], &[Type::NUMERIC]).unwrap();
        assert!(null[0].bytes.is_none());
        assert_eq!(null[0].ty, Type::NUMERIC);
    }
    use std::future::{pending, poll_fn, Future};
    use std::task::Poll;

    #[test]
    fn cancel_registration_cleans_success_and_errors() {
        let registry = CancelRegistry::new();
        for result in [
            Ok(()),
            Err(DatabaseError::QueryError("预编译失败".into())),
            Err(DatabaseError::ExecuteError("执行失败".into())),
            Err(DatabaseError::QueryCancelled),
        ] {
            let run = || -> Result<(), DatabaseError> {
                let _registration = registry.register(7, ());
                assert_eq!(registry.entries().len(), 1);
                result?;
                Ok(())
            };
            let _ = run();
            assert_eq!(registry.entries().len(), 0);
        }
    }

    #[test]
    fn cancel_registration_keeps_replacement() {
        let registry = CancelRegistry::new();
        let old = registry.register(7, "旧查询");
        let current = registry.register(7, "新查询");
        drop(old);
        assert_eq!(registry.entries().len(), 1);
        assert_eq!(registry.get(7).as_deref(), Some(&"新查询"));
        drop(current);
        assert!(registry.entries().is_empty());
    }

    #[test]
    fn cancel_registration_cleans_on_panic() {
        let registry = CancelRegistry::new();
        let result = std::panic::catch_unwind(|| {
            let _registration = registry.register(7, ());
            panic!("模拟查询内部 panic");
        });
        assert!(result.is_err());
        assert!(registry.entries().is_empty());
    }

    #[tokio::test]
    async fn cancel_registration_cleans_when_future_is_dropped() {
        let registry = CancelRegistry::new();
        let mut query = Box::pin(async {
            let _registration = registry.register(7, ());
            pending::<()>().await;
        });
        poll_fn(|cx| {
            assert!(query.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        assert_eq!(registry.entries().len(), 1);
        drop(query);
        assert!(registry.entries().is_empty());
    }

    /// 显式运行：设置 SQLINK_TEST_POSTGRES 后执行 cargo test postgres_cancel_cleanup -- --ignored。
    #[tokio::test]
    #[ignore = "需要 SQLINK_TEST_POSTGRES 指向可用的 PostgreSQL 测试库"]
    async fn postgres_cancel_cleanup() {
        let url = std::env::var("SQLINK_TEST_POSTGRES").expect("需要 SQLINK_TEST_POSTGRES");
        let cfg: Config = url.parse().expect("PostgreSQL 测试连接串无效");
        let (client, connection) = cfg
            .connect(NoTls)
            .await
            .expect("连接 PostgreSQL 测试库失败");
        let connection_task = tokio::spawn(async move { connection.await });
        let host = match cfg.get_hosts().first().expect("测试连接串必须包含主机") {
            tokio_postgres::config::Host::Tcp(host) => host.clone(),
            _ => panic!("测试连接串必须使用TCP主机"),
        };
        let config: ConnectionConfig = serde_json::from_value(serde_json::json!({
            "name": "取消登记回归", "driverType": "postgres", "host": host,
            "port": cfg.get_ports().first().copied().unwrap_or(5432),
            "user": cfg.get_user().unwrap_or("postgres"),
            "password": String::from_utf8_lossy(cfg.get_password().unwrap_or_default()),
            "database": cfg.get_dbname().unwrap_or("postgres")
        }))
        .unwrap();
        drop(client);
        let driver = PgDriver::connect(&config).await.unwrap();

        for _ in 0..3 {
            driver.query("SELECT 1", None, Some(7)).await.unwrap();
            assert!(driver.cancel_tokens.entries().is_empty());
            // Parse 阶段失败：回归原先 prepare 后通过 ? 提前退出的泄漏。
            assert!(matches!(
                driver.query("SELECT (", None, Some(7)).await,
                Err(DatabaseError::QueryError(_))
            ));
            assert!(driver.cancel_tokens.entries().is_empty());
            assert!(matches!(
                driver.query("SELECT 1 / 0", None, Some(7)).await,
                Err(DatabaseError::QueryError(_))
            ));
            assert!(driver.cancel_tokens.entries().is_empty());
            driver.execute("SELECT 1", None, Some(7)).await.unwrap();
            assert!(driver.cancel_tokens.entries().is_empty());
            assert!(matches!(
                driver.execute("SELECT 1 / 0", None, Some(7)).await,
                Err(DatabaseError::ExecuteError(_))
            ));
            assert!(driver.cancel_tokens.entries().is_empty());
        }

        for execute in [false, true] {
            let operation = async {
                if execute {
                    driver.execute("SELECT pg_sleep(30)", None, Some(7)).await
                } else {
                    driver.query("SELECT pg_sleep(30)", None, Some(7)).await
                }
            };
            tokio::pin!(operation);
            let cancel = async {
                let mut interval = tokio::time::interval(std::time::Duration::from_millis(20));
                loop {
                    interval.tick().await;
                    if driver.cancel_tokens.get(7).is_some() {
                        driver.cancel_query(7).await.unwrap();
                    }
                }
            };
            let result = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                tokio::select! {
                    result = &mut operation => result,
                    _ = cancel => unreachable!(),
                }
            })
            .await
            .expect("取消查询超时");
            assert!(matches!(result, Err(DatabaseError::QueryCancelled)));
            assert!(driver.cancel_tokens.entries().is_empty());
        }
        driver.disconnect().await.unwrap();
        connection_task.abort();
    }
}
