use crate::database::result_budget::QueryBudget;
use crate::database::DatabaseDriver;
use crate::export::{check_raw_lengths, ExportColumn, ExportSink};
use crate::models::*;
use crate::session::{SessionCancellation, SessionCapabilities, SessionLease, TargetSnapshot};
use crate::typed_value::{
    validate_values, TimeValue, TimestampValue, TimezoneKind, TypedQueryResult, TypedValue,
};
use async_trait::async_trait;
use mysql_async::{prelude::Queryable, Opts, OptsBuilder, Pool};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::timeout;

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

/// 租约持有完整 Conn，只有健康、明确归还的连接才能重新进入连接池。
struct MySqlLease {
    id: String,
    conn: Option<mysql_async::Conn>,
    connected: Arc<AtomicBool>,
    cancellation: Arc<MySqlCancellation>,
    dirty: bool,
    /// 操作future未正常返回时保留，禁止在未知会话状态下继续发SQL。
    in_flight: bool,
}

struct MySqlCancellation {
    conn_id: u32,
    opts: Opts,
    active: AtomicBool,
    cancelled: AtomicBool,
    connected: Arc<AtomicBool>,
    gate: Mutex<()>,
}

#[async_trait]
impl SessionCancellation for MySqlCancellation {
    async fn cancel(&self) -> Result<(), DatabaseError> {
        let _guard = self.gate.lock().await;
        if !self.active.load(Ordering::SeqCst) || !self.connected.load(Ordering::SeqCst) {
            return Err(DatabaseError::Unknown("租约已归还或连接已断开".into()));
        }
        // 一旦发起取消就隔离此物理连接，不能把请求发送成功当成语句已取消。
        self.cancelled.store(true, Ordering::SeqCst);
        let mut killer = mysql_async::Conn::new(self.opts.clone())
            .await
            .map_err(|e| DatabaseError::ConnectionError(e.to_string()))?;
        let result = killer
            .query_drop(format!("KILL QUERY {}", self.conn_id))
            .await
            .map_err(map_query_error);
        let _ = killer.disconnect().await;
        result
    }
}

struct MySqlRegistration<'a> {
    entries: &'a StdMutex<HashMap<u64, Arc<MySqlCancellation>>>,
    id: u64,
    handle: Arc<MySqlCancellation>,
}
impl<'a> MySqlRegistration<'a> {
    fn new(
        entries: &'a StdMutex<HashMap<u64, Arc<MySqlCancellation>>>,
        id: u64,
        handle: Arc<MySqlCancellation>,
    ) -> Self {
        entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, handle.clone());
        Self {
            entries,
            id,
            handle,
        }
    }
}
impl Drop for MySqlRegistration<'_> {
    fn drop(&mut self) {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        if entries
            .get(&self.id)
            .is_some_and(|handle| Arc::ptr_eq(handle, &self.handle))
        {
            entries.remove(&self.id);
        }
    }
}

/// mysql_async 0.34 的 disconnect 首次 poll 先设置 disconnected，再等待写 QUIT。
/// 同步推进到首个挂起点后析构，recycler 会丢弃连接，运行时关闭也不能返还脏连接。
fn discard_mysql_connection(conn: mysql_async::Conn) {
    use std::future::Future;
    use std::task::{Context, Wake, Waker};
    struct Noop;
    impl Wake for Noop {
        fn wake(self: Arc<Self>) {}
    }
    let waker = Waker::from(Arc::new(Noop));
    let mut context = Context::from_waker(&waker);
    let mut disconnect = Box::pin(conn.disconnect());
    let _ = disconnect.as_mut().poll(&mut context);
}

impl MySqlDriver {
    async fn new_lease(&self, database: Option<&str>) -> Result<MySqlLease, DatabaseError> {
        if !self.connected.load(Ordering::SeqCst) {
            return Err(DatabaseError::ConnectionError("连接已断开".into()));
        }
        let conn = self
            .pool
            .get_conn()
            .await
            .map_err(|e| DatabaseError::ConnectionError(e.to_string()))?;
        let cancellation = Arc::new(MySqlCancellation {
            conn_id: conn.id(),
            opts: self.opts.clone(),
            active: AtomicBool::new(true),
            cancelled: AtomicBool::new(false),
            connected: self.connected.clone(),
            gate: Mutex::new(()),
        });
        let mut lease = MySqlLease {
            id: uuid::Uuid::new_v4().to_string(),
            conn: Some(conn),
            connected: self.connected.clone(),
            cancellation,
            dirty: false,
            in_flight: false,
        };
        lease.ensure_live()?;
        if let Some(database) = database.filter(|d| !d.is_empty()).or(self.opts.db_name()) {
            lease
                .execute(&format!("USE `{}`", database.replace('`', "``")))
                .await?;
        }
        Ok(lease)
    }
}

fn mysql_raw_length(value: Option<&mysql_async::Value>) -> usize {
    match value {
        None | Some(mysql_async::Value::NULL) => 0,
        Some(mysql_async::Value::Bytes(bytes)) => bytes.len(),
        _ => 16,
    }
}

/// 只消费首个结果集；达到预算后直接归还未读状态，由租约关闭物理连接。
async fn collect_mysql_query<P: mysql_async::prelude::Protocol>(
    mut source: mysql_async::QueryResult<'_, '_, P>,
    typed_mode: bool,
    start: std::time::Instant,
) -> Result<TypedQueryResult, DatabaseError> {
    let raw_columns = source.columns_ref().to_vec();
    let columns: Vec<ColumnMetadata> = raw_columns
        .iter()
        .map(|column| ColumnMetadata {
            name: column.name_str().into_owned(),
            data_type: format!("{:?}", column.column_type()),
            unified_type: "Unknown".into(),
            nullable: !column
                .flags()
                .contains(mysql_async::consts::ColumnFlags::NOT_NULL_FLAG),
            length: Some(column.column_length() as usize),
            precision: None,
            scale: None,
        })
        .collect();
    let mut budget = QueryBudget::new(&columns)?;
    let mut rows = Vec::new();
    let mut typed_rows = Vec::new();
    let mut truncated = false;
    while let Some(native) = source.next().await.map_err(map_query_error)? {
        if budget.row_limit_reached()
            || !budget.allows_raw_row(
                (0..native.len()).map(|index| mysql_raw_length(native.as_ref(index))),
            )
        {
            truncated = true;
            break;
        }
        let mut values = Vec::with_capacity(columns.len());
        let mut typed = Vec::with_capacity(columns.len());
        for (value, column) in native.unwrap_raw().into_iter().zip(&raw_columns) {
            let decoded = typed_mode
                .then(|| {
                    value
                        .as_ref()
                        .and_then(|value| mysql_value_typed(value, column))
                })
                .flatten();
            values.push(Some(
                decoded
                    .as_ref()
                    .map(TypedValue::display_json)
                    .unwrap_or_else(|| value_to_json(value, col_kind(column))),
            ));
            if typed_mode {
                typed.push(decoded);
            }
        }
        let row = RowData { values };
        if !budget.try_add_row(&row, typed_mode.then_some(typed.as_slice()))? {
            truncated = true;
            break;
        }
        rows.push(row);
        if typed_mode {
            typed_rows.push(typed);
        }
    }
    // next() 在首结果末尾已转到后续元数据，但不会收集其数据行。
    // 禁止 drop_result 隐式读完巨大尾部；整个租约会在外层隔离。
    truncated |= !source.is_empty();
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

/// 导出逐行解码、逐行交给写入端；未消费的结果始终留给外层隔离物理连接。
async fn stream_mysql_export<P: mysql_async::prelude::Protocol>(
    mut source: mysql_async::QueryResult<'_, '_, P>,
    sink: &dyn ExportSink,
    cancellation: &MySqlCancellation,
) -> Result<u64, DatabaseError> {
    let check_cancelled = || {
        sink.check_cancelled()?;
        if cancellation.cancelled.load(Ordering::SeqCst) {
            return Err(DatabaseError::QueryCancelled);
        }
        if !cancellation.active.load(Ordering::SeqCst)
            || !cancellation.connected.load(Ordering::SeqCst)
        {
            return Err(DatabaseError::ConnectionError(
                "连接已断开或租约已归还".into(),
            ));
        }
        Ok(())
    };
    check_cancelled()?;
    let raw_columns = source.columns_ref().to_vec();
    let columns: Vec<ExportColumn> = raw_columns
        .iter()
        .map(|column| ExportColumn {
            name: column.name_str().into_owned(),
        })
        .collect();
    // 无数据的表也必须输出列名。
    sink.columns(&columns)?;
    let mut rows_written = 0u64;
    loop {
        check_cancelled()?;
        let Some(native) = source.next().await.map_err(map_query_error)? else {
            break;
        };
        check_cancelled()?;
        check_raw_lengths((0..native.len()).map(|index| mysql_raw_length(native.as_ref(index))))?;
        if native.len() != raw_columns.len() {
            return Err(DatabaseError::TypeError(
                "EXPORT_TYPE_UNSUPPORTED: MySQL 导出行与列元数据不一致".into(),
            ));
        }
        let values = native
            .unwrap_raw()
            .into_iter()
            .zip(&raw_columns)
            .map(|(value, column)| {
                value
                    .as_ref()
                    .and_then(|value| mysql_value_typed(value, column))
                    .ok_or_else(|| {
                        DatabaseError::TypeError(format!(
                            "EXPORT_TYPE_UNSUPPORTED: MySQL 列 {} 的 {:?} 载荷不能无损解码",
                            column.name_str(),
                            column.column_type()
                        ))
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;
        sink.row(&values)?;
        rows_written += 1;
    }
    // next() 仅推进到第二结果的元数据，禁止 drop_result 隐式排空尾部。
    if !source.is_empty() {
        return Err(DatabaseError::QueryError(
            "EXPORT_MULTIPLE_RESULTS: MySQL 导出只接受一个结果集".into(),
        ));
    }
    check_cancelled()?;
    Ok(rows_written)
}

impl MySqlLease {
    fn quarantine_stream(&mut self) {
        self.cancellation.active.store(false, Ordering::SeqCst);
        self.dirty = true;
        if let Some(conn) = self.conn.take() {
            discard_mysql_connection(conn);
        }
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

impl Drop for MySqlLease {
    fn drop(&mut self) {
        self.cancellation.active.store(false, Ordering::SeqCst);
        if let Some(conn) = self.conn.take() {
            discard_mysql_connection(conn);
        }
    }
}

#[async_trait]
impl SessionLease for MySqlLease {
    fn session_id(&self) -> &str {
        &self.id
    }
    fn capabilities(&self) -> SessionCapabilities {
        SessionCapabilities::supported()
    }
    fn cancel_handle(&self) -> Option<Arc<dyn SessionCancellation>> {
        Some(self.cancellation.clone())
    }
    async fn stream_export(
        &mut self,
        sql: &str,
        sink: Arc<dyn ExportSink>,
    ) -> Result<u64, DatabaseError> {
        let was_dirty = self.begin_operation()?;
        let result = async {
            sink.check_cancelled()?;
            self.ensure_live()?;
            if self.cancellation.cancelled.load(Ordering::SeqCst) {
                return Err(DatabaseError::QueryCancelled);
            }
            let conn = self.conn.as_mut().expect("租约连接存在");
            // 二进制协议保留 int64/uint64、DECIMAL 和 binary 的原生类型。
            let source = conn
                .exec_iter(sql, mysql_async::Params::Empty)
                .await
                .map_err(map_query_error)?;
            stream_mysql_export(source, sink.as_ref(), &self.cancellation).await
        }
        .await;
        self.in_flight = false;
        if result.is_err() {
            // 包括写文件失败、类型/预算拒绝、取消和 SQL 错误，均不排空未读流。
            self.quarantine_stream();
            return result;
        }
        self.dirty = was_dirty;
        self.ensure_live()?;
        result
    }
    async fn query_typed(
        &mut self,
        sql: &str,
        params: &[TypedValue],
    ) -> Result<TypedQueryResult, DatabaseError> {
        let start = std::time::Instant::now();
        let was_dirty = self.begin_operation()?;
        let mut stream_started = false;
        let result = async {
            let params = mysql_params(params)?;
            self.ensure_live()?;
            let conn = self.conn.as_mut().expect("租约连接存在");
            let source = conn.exec_iter(sql, params).await.map_err(map_query_error)?;
            stream_started = true;
            collect_mysql_query(source, true, start).await
        }
        .await;
        self.in_flight = false;
        if stream_started && result.as_ref().map_or(true, |read| read.result.truncated) {
            self.quarantine_stream();
            return result;
        }
        if result.is_ok() {
            self.dirty = was_dirty;
        }
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
            let params = mysql_params(params)?;
            self.ensure_live()?;
            let conn = self.conn.as_mut().expect("租约连接存在");
            let result = conn
                .exec_iter(sql, params)
                .await
                .map_err(map_execute_error)?;
            let affected = result.affected_rows();
            result.drop_result().await.map_err(map_execute_error)?;
            Ok(affected)
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
        let start = std::time::Instant::now();
        let was_dirty = self.begin_operation()?;
        let mut stream_started = false;
        let result = async {
            self.ensure_live()?;
            let conn = self.conn.as_mut().expect("租约连接存在");
            let source = conn.query_iter(sql).await.map_err(map_query_error)?;
            stream_started = true;
            collect_mysql_query(source, false, start)
                .await
                .map(|read| read.result)
        }
        .await;
        self.in_flight = false;
        if stream_started && result.as_ref().map_or(true, |read| read.truncated) {
            self.quarantine_stream();
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
            let conn = self.conn.as_mut().expect("租约连接存在");
            let result = conn.query_iter(sql).await;
            let result = match result {
                Ok(r) => r,
                Err(e) => {
                    return Err(map_execute_error(e));
                }
            };

            let affected = result.affected_rows();
            let last_id = result.last_insert_id();

            // 非查询执行只丢弃返回行，不为不可见数据构造完整 Vec。
            result.drop_result().await.map_err(map_execute_error)?;

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
        _lock: bool,
    ) -> Result<TargetSnapshot, DatabaseError> {
        // InnoDB 的内部 NAME 使用自己的文件名编码；尚未验证的名称不能猜测映射。
        if [&target.database, &target.table].iter().any(|name| {
            name.is_empty() || !name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
        }) {
            return Err(DatabaseError::QueryError(
                "OBJECT_IDENTITY_UNSUPPORTED: 尚未验证此 MySQL 内部名称映射".into(),
            ));
        }
        let was_dirty = self.begin_operation()?;
        let result = async {
            let conn = self.conn.as_mut().ok_or_else(|| DatabaseError::ConnectionError("租约连接不存在".into()))?;
            let version: Option<String> = conn.query_first("SELECT VERSION()").await.map_err(map_query_error)?;
            let version = version.ok_or_else(|| DatabaseError::QueryError("MySQL 版本不可用".into()))?;
            if !(version.starts_with("8.0.") || version.starts_with("8.4.")) || version.to_ascii_lowercase().contains("mariadb") {
                return Err(DatabaseError::QueryError("OBJECT_IDENTITY_UNSUPPORTED: 仅实现 MySQL 8.0/8.4 的 InnoDB 目录身份".into()));
            }
            // 实际访问基础表以获得事务范围的 MDL；不使用可能完全跳过对象访问的 LIMIT 0。
            let quoted = format!("`{}`.`{}`", target.database, target.table);
            let definitions: Vec<(String, String)> = conn.query(format!("SHOW CREATE TABLE {quoted}")).await.map_err(map_query_error)?;
            if definitions.len() != 1 || !definitions[0].1.starts_with("CREATE TABLE ") {
                return Err(DatabaseError::QueryError("OBJECT_IDENTITY_UNSUPPORTED: 临时对象或非基础表不能建立身份".into()));
            }
            let _: Vec<mysql_async::Row> = conn.query(format!("SELECT 1 FROM {quoted} LIMIT 1")).await.map_err(map_query_error)?;
            let info: Vec<(String, String, String, Option<String>)> = conn.exec(
                "SELECT TABLE_SCHEMA, TABLE_NAME, TABLE_TYPE, ENGINE FROM information_schema.TABLES \
                 WHERE BINARY TABLE_SCHEMA = BINARY ? AND BINARY TABLE_NAME = BINARY ? LIMIT 2",
                (&target.database, &target.table),
            ).await.map_err(map_query_error)?;
            if info.len() != 1 { return Err(DatabaseError::QueryError("OBJECT_IDENTITY_UNSUPPORTED: 目标基础表不唯一".into())); }
            let (database, table, kind, engine) = &info[0];
            if kind != "BASE TABLE" || engine.as_deref() != Some("InnoDB") {
                return Err(DatabaseError::QueryError("NON_TRANSACTIONAL_TARGET: 仅验证了 InnoDB 基础表".into()));
            }
            let internal_name = format!("{database}/{table}");
            // 此目录需要 PROCESS 等权限；权限不足直接失败，让调用方降级为只读。
            let ids: Vec<(String, String)> = conn.exec(
                "SELECT CAST(TABLE_ID AS CHAR), CAST(SPACE AS CHAR) FROM information_schema.INNODB_TABLES WHERE BINARY NAME = BINARY ?",
                (&internal_name,),
            ).await.map_err(map_query_error)?;
            if ids.len() != 1 { return Err(DatabaseError::QueryError("OBJECT_IDENTITY_UNSUPPORTED: 无法证明唯一 InnoDB 内部表身份".into())); }
            let server_uuid: Option<String> = conn.query_first("SELECT @@server_uuid").await.map_err(map_query_error)?;
            let server_uuid = server_uuid.filter(|v| !v.is_empty()).ok_or_else(|| DatabaseError::QueryError("OBJECT_IDENTITY_UNSUPPORTED: 服务器身份不可用".into()))?;
            let column_rows: Vec<(String, String, String, String, Option<String>, String, String)> = conn.exec(
                "SELECT COLUMN_NAME, COLUMN_TYPE, IS_NULLABLE, COLUMN_KEY, COLUMN_DEFAULT, EXTRA, COLUMN_COMMENT \
                 FROM information_schema.COLUMNS WHERE BINARY TABLE_SCHEMA = BINARY ? AND BINARY TABLE_NAME = BINARY ? ORDER BY ORDINAL_POSITION",
                (database, table),
            ).await.map_err(map_query_error)?;
            let columns: Vec<ColumnInfo> = column_rows.into_iter().map(|(name, data_type, nullable, key, default_value, extra, comment)| ColumnInfo {
                name, data_type, nullable: nullable == "YES", is_primary_key: key == "PRI", default_value,
                extra: Some(extra), comment: Some(comment),
            }).collect();
            if columns.is_empty() || columns.iter().any(|c| c.name.is_empty() || c.data_type.is_empty()) {
                return Err(DatabaseError::QueryError("OBJECT_IDENTITY_UNSUPPORTED: 完整列目录不可用".into()));
            }
            let mut write_reasons: Vec<String> = Vec::new();
            if !version.starts_with("8.4.") { write_reasons.push("DRIVER_VERSION_NOT_VALIDATED".into()); }
            // 仅接受当前账号的表/精确 schema 直接授权。全局授权目录不能证明旧会话已生效。
            let account: Option<String> = conn.query_first("SELECT CURRENT_USER()").await.map_err(map_query_error)?;
            let grantee = account.as_deref().and_then(|account| account.rsplit_once('@')).filter(|(user, host)|
                !user.is_empty() && !host.is_empty() && user.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
                && host.bytes().all(|c| c.is_ascii_alphanumeric() || b"._:%-".contains(&c)))
                .map(|(user, host)| format!("'{user}'@'{host}'"));
            let visible = if let Some(grantee) = grantee {
                let count: Option<u64> = conn.exec_first(
                    "SELECT (SELECT COUNT(*) FROM information_schema.TABLE_PRIVILEGES WHERE BINARY GRANTEE = BINARY ? AND BINARY TABLE_SCHEMA = BINARY ? AND BINARY TABLE_NAME = BINARY ? AND PRIVILEGE_TYPE = 'TRIGGER') + \
                     (SELECT COUNT(*) FROM information_schema.SCHEMA_PRIVILEGES WHERE BINARY GRANTEE = BINARY ? AND BINARY TABLE_SCHEMA = BINARY ? AND PRIVILEGE_TYPE = 'TRIGGER')",
                    (&grantee, database, table, &grantee, database),
                ).await.map_err(map_query_error)?;
                count.is_some_and(|count| count > 0)
            } else { false };
            if !visible { write_reasons.push("TRIGGER_VISIBILITY_UNVERIFIED".into()); }
            // 数据库权限在下次 USE 生效；目录核验后重新选择，再读取触发器，避免沿用旧权限缓存。
            conn.query_drop(format!("USE `{database}`")).await.map_err(map_query_error)?;
            let trigger_count: Option<u64> = conn.exec_first(
                "SELECT COUNT(*) FROM information_schema.TRIGGERS WHERE BINARY EVENT_OBJECT_SCHEMA = BINARY ? AND BINARY EVENT_OBJECT_TABLE = BINARY ?",
                (database, table),
            ).await.map_err(map_query_error)?;
            let state: Option<(String, String, String, String, String, u8, u8)> = conn.query_first(
                "SELECT @@SESSION.sql_mode, @@SESSION.character_set_client, @@SESSION.character_set_connection, @@SESSION.character_set_results, @@SESSION.time_zone, @@SESSION.foreign_key_checks, @@SESSION.unique_checks"
            ).await.map_err(map_query_error)?;
            let (mode, client_charset, connection_charset, results_charset, timezone, foreign_key_checks, unique_checks) = state
                .ok_or_else(|| DatabaseError::QueryError("会话转换状态不可用".into()))?;
            if !mode.split(',').any(|v| matches!(v, "STRICT_ALL_TABLES" | "STRICT_TRANS_TABLES")) {
                write_reasons.push("NON_STRICT_SQL_MODE".into());
            }
            let incompatible_charsets: Option<u64> = conn.exec_first(
                "SELECT COUNT(*) FROM information_schema.COLUMNS WHERE BINARY TABLE_SCHEMA = BINARY ? AND BINARY TABLE_NAME = BINARY ? AND CHARACTER_SET_NAME IS NOT NULL AND CHARACTER_SET_NAME <> 'utf8mb4'",
                (database, table),
            ).await.map_err(map_query_error)?;
            if [client_charset, connection_charset, results_charset].iter().any(|v| v != "utf8mb4")
                || incompatible_charsets != Some(0) {
                write_reasons.push("CHARSET_CONVERSION_UNVERIFIED".into());
            }
            if timezone != "+00:00" && columns.iter().any(|column| {
                matches!(column.data_type.split(['(', ' ']).next().unwrap_or(""), "timestamp" | "datetime" | "date" | "time")
            }) { write_reasons.push("SESSION_TIMEZONE_UNVERIFIED".into()); }
            if foreign_key_checks != 1 { write_reasons.push("FOREIGN_KEY_CHECKS_DISABLED".into()); }
            if unique_checks != 1 { write_reasons.push("UNIQUE_CHECKS_DISABLED".into()); }
            // 无权限时目录会隐藏触发器，因此零个可见触发器不足以放行写入。
            if trigger_count != Some(0) { write_reasons.push("TRIGGERS_UNSUPPORTED".into()); }
            let identity = serde_json::to_string(&(server_uuid, &ids[0]))
                .map_err(|_| DatabaseError::Unknown("对象身份编码失败".into()))?;
            Ok(TargetSnapshot { table: ResolvedTable { database: database.clone(), schema: None,
                table: table.clone(), columns, object_kind: Some("table".into()) }, identity, write_reasons })
        }.await;
        self.in_flight = false;
        if result.is_ok() {
            self.dirty = was_dirty;
        }
        self.ensure_live()?;
        result
    }
    async fn begin(&mut self) -> Result<(), DatabaseError> {
        self.execute("START TRANSACTION").await.map(|_| ())
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
        if self.in_flight
            || self.dirty
            || cancellation.cancelled.load(Ordering::SeqCst)
            || !self.connected.load(Ordering::SeqCst)
        {
            return Ok(()); // Drop 同步隔离。
        }
        let conn = self.conn.as_mut().expect("租约连接存在");
        // COM_CHANGE_USER恢复初始schema并清除事务、临时表、变量与语句缓存。
        // mysql_async明确支持将它作为COM_RESET_CONNECTION的兼容替代。
        conn.query_drop("ROLLBACK")
            .await
            .map_err(map_execute_error)?;
        conn.change_user(Default::default())
            .await
            .map_err(map_execute_error)?;
        // gate内去活后完成reset，才允许池复用；release future被取消时仍由Drop隔离。
        if self.connected.load(Ordering::SeqCst) {
            drop(self.conn.take());
        }
        Ok(())
    }
}

/// 系统目录类型或列缺失必须显式失败，避免静默丢掉元数据行。
fn metadata_value<T: mysql_async::prelude::FromValue>(
    row: &mysql_async::Row,
    name: &str,
) -> Result<T, DatabaseError> {
    row.get_opt(name)
        .ok_or_else(|| DatabaseError::TypeError(format!("MySQL 元数据缺少 {}", name)))?
        .map_err(|error| {
            DatabaseError::TypeError(format!("MySQL 元数据 {} 解码失败：{}", name, error))
        })
}

pub struct MySqlDriver {
    pool: Pool,
    opts: Opts,
    connected: Arc<AtomicBool>,
    /// query_id -> 本次租约独有取消句柄；归还后旧句柄同步失效。
    active_queries: Arc<StdMutex<HashMap<u64, Arc<MySqlCancellation>>>>,
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
            opts: build_opts(config),
            connected: Arc::new(AtomicBool::new(true)),
            active_queries: Arc::new(StdMutex::new(HashMap::new())),
        })
    }

    /// 元数据与导出也必须尊重断开标记，不能趁后台池回收尚未启动重新建连。
    async fn metadata_conn(&self) -> Result<mysql_async::Conn, DatabaseError> {
        if !self.connected.load(Ordering::SeqCst) {
            return Err(DatabaseError::ConnectionError("连接已断开".into()));
        }
        let conn = self
            .pool
            .get_conn()
            .await
            .map_err(|e| DatabaseError::ConnectionError(e.to_string()))?;
        if !self.connected.load(Ordering::SeqCst) {
            discard_mysql_connection(conn);
            return Err(DatabaseError::ConnectionError("连接已断开".into()));
        }
        Ok(conn)
    }

    async fn columns_for_table(
        &self,
        database: &str,
        table: &str,
    ) -> Result<Vec<ColumnInfo>, DatabaseError> {
        timeout(Duration::from_secs(15), async {
        let mut conn = self.metadata_conn().await?;
        let rows: Vec<mysql_async::Row> = conn.exec(
            "SELECT COLUMN_NAME, COLUMN_TYPE, IS_NULLABLE, COLUMN_KEY, COLUMN_DEFAULT, EXTRA, COLUMN_COMMENT FROM information_schema.COLUMNS WHERE BINARY TABLE_SCHEMA = BINARY ? AND BINARY TABLE_NAME = BINARY ? ORDER BY ORDINAL_POSITION",
            (database, table),
        ).await.map_err(map_query_error)?;
        rows.iter()
            .map(|row| {
                Ok(ColumnInfo {
                    name: metadata_value(row, "COLUMN_NAME")?,
                    data_type: metadata_value(row, "COLUMN_TYPE")?,
                    nullable: metadata_value::<String>(row, "IS_NULLABLE")? == "YES",
                    is_primary_key: metadata_value::<String>(row, "COLUMN_KEY")? == "PRI",
                    default_value: metadata_value(row, "COLUMN_DEFAULT")?,
                    extra: metadata_value(row, "EXTRA")?,
                    comment: metadata_value(row, "COLUMN_COMMENT")?,
                })
            })
            .collect::<Result<Vec<_>, DatabaseError>>()
        }).await.map_err(|_| DatabaseError::QueryError("读取列信息超时（可能存在未提交的表结构锁）".into()))?
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
        "mysql"
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
            MySqlRegistration::new(&self.active_queries, qid, lease.cancellation.clone())
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
            MySqlRegistration::new(&self.active_queries, qid, lease.cancellation.clone())
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
            .active_queries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&query_id)
            .cloned()
            .ok_or_else(|| DatabaseError::Unknown("查询不存在或已结束".into()))?;
        handle.cancel().await
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
        let result = match database {
            Some(db) => self.query_typed(
                "SELECT TABLE_NAME, ENGINE, TABLE_ROWS, TABLE_COMMENT, TABLE_SCHEMA FROM information_schema.TABLES WHERE TABLE_SCHEMA = ? AND TABLE_TYPE = 'BASE TABLE' ORDER BY TABLE_NAME",
                &[TypedValue::Text(db.into())], None).await?.result,
            None => self.query("SELECT TABLE_NAME, ENGINE, TABLE_ROWS, TABLE_COMMENT, TABLE_SCHEMA FROM information_schema.TABLES WHERE TABLE_SCHEMA = DATABASE() AND TABLE_TYPE = 'BASE TABLE' ORDER BY TABLE_NAME",None,None).await?,
        };
        let tables = complete_table_catalog(&result)?
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
                    schema: row
                        .values
                        .get(4)
                        .and_then(|v| v.as_ref())
                        .and_then(|v| v.as_str())
                        .map(str::to_owned),
                    row_count,
                    engine,
                    comment,
                })
            })
            .collect();
        Ok(tables)
    }

    async fn get_views(&self, database: Option<&str>) -> Result<Vec<TableInfo>, DatabaseError> {
        let mut conn = self.metadata_conn().await?;
        let rows: Vec<mysql_async::Row> = conn.exec(
            "SELECT TABLE_SCHEMA, TABLE_NAME FROM information_schema.TABLES WHERE TABLE_SCHEMA = COALESCE(NULLIF(?, ''), DATABASE()) AND TABLE_TYPE = 'VIEW' ORDER BY TABLE_NAME",
            (database.unwrap_or(""),),
        ).await.map_err(map_query_error)?;
        rows.iter()
            .map(|row| {
                Ok(TableInfo {
                    name: metadata_value(row, "TABLE_NAME")?,
                    schema: Some(metadata_value(row, "TABLE_SCHEMA")?),
                    row_count: None,
                    engine: None,
                    comment: None,
                })
            })
            .collect()
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
        let mut conn = self.metadata_conn().await?;
        let row: mysql_async::Row = conn
            .query_first(format!(
                "SHOW CREATE VIEW `{}`.`{}`",
                self.escape_identifier(&target.database),
                self.escape_identifier(&target.table)
            ))
            .await
            .map_err(map_query_error)?
            .ok_or_else(|| DatabaseError::QueryError("MySQL 视图不存在".into()))?;
        row.get_opt::<String, _>(1)
            .ok_or_else(|| DatabaseError::TypeError("MySQL 视图定义列缺失".into()))?
            .map_err(|e| DatabaseError::TypeError(format!("MySQL 视图定义解码失败：{}", e)))
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
        let mut conn = self.metadata_conn().await?;
        let identities: Vec<(String, String, String)> = conn.exec(
            "SELECT TABLE_SCHEMA, TABLE_NAME, TABLE_TYPE FROM information_schema.TABLES \
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
        let (database, table, table_type) = identities.into_iter().next().unwrap();
        let object_kind = match table_type.as_str() {
            "BASE TABLE" => "table",
            "VIEW" => "view",
            _ => "unknown",
        };
        let columns = self.columns_for_table(&database, &table).await?;
        if columns.is_empty() {
            return Err(DatabaseError::QueryError("目标表列信息不可用".into()));
        }
        Ok(ResolvedTable {
            database,
            schema: None,
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
        let mut conn = self.metadata_conn().await?;
        // 标识符只采用反引号引用，兼容 NO_BACKSLASH_ESCAPES 和 ANSI_QUOTES。
        let qualified = format!(
            "`{}`.`{}`",
            self.escape_identifier(&target.database),
            self.escape_identifier(&target.table)
        );
        let rows: Vec<mysql_async::Row> = conn
            .query(format!("SHOW INDEX FROM {}", qualified))
            .await
            .map_err(map_query_error)?;
        let mut grouped =
            std::collections::BTreeMap::<String, Vec<(u64, bool, IndexKeyPart)>>::new();
        let mut needs_definition = false;
        for row in &rows {
            let name: String = metadata_value(row, "Key_name")?;
            let non_unique: u64 = metadata_value(row, "Non_unique")?;
            let ordinal: u64 = metadata_value(row, "Seq_in_index")?;
            let column: Option<String> = metadata_value(row, "Column_name")?;
            // 5.7 没有 Expression 列，按实际列目录探测，不能查询不存在的 I_S 字段。
            let expression = if row
                .columns_ref()
                .iter()
                .any(|c| c.name_str() == "Expression")
            {
                metadata_value::<Option<String>>(row, "Expression")?
            } else {
                None
            };
            let descending =
                metadata_value::<Option<String>>(row, "Collation")?.as_deref() == Some("D");
            let prefix: Option<u64> = metadata_value(row, "Sub_part")?;
            needs_definition |= column.is_none() || prefix.is_some();
            grouped.entry(name).or_default().push((
                ordinal,
                non_unique == 0,
                IndexKeyPart {
                    column,
                    expression,
                    descending,
                },
            ));
        }
        let definition = if needs_definition {
            let row: mysql_async::Row = conn
                .query_first(format!("SHOW CREATE TABLE {}", qualified))
                .await
                .map_err(map_query_error)?
                .ok_or_else(|| DatabaseError::QueryError("MySQL 索引定义不可用".into()))?;
            Some(
                row.get_opt::<String, _>(1)
                    .ok_or_else(|| DatabaseError::TypeError("MySQL 表定义列缺失".into()))?
                    .map_err(|e| {
                        DatabaseError::TypeError(format!("MySQL 表定义类型无效：{}", e))
                    })?,
            )
        } else {
            None
        };
        let mut indexes = Vec::new();
        for (name, mut parts) in grouped {
            parts.sort_by_key(|part| part.0);
            let unique = parts[0].1;
            if parts
                .iter()
                .enumerate()
                .any(|(i, part)| part.0 != (i + 1) as u64 || part.1 != unique)
            {
                return Err(DatabaseError::QueryError(
                    "MySQL 索引键序号或唯一标记不一致".into(),
                ));
            }
            let key_parts: Vec<_> = parts.into_iter().map(|(_, _, part)| part).collect();
            let columns = key_parts
                .iter()
                .filter_map(|part| part.column.clone())
                .collect();
            indexes.push(IndexInfo {
                is_primary: name == "PRIMARY",
                name,
                columns,
                is_unique: unique,
                key_parts,
                included_columns: Vec::new(),
                definition: definition.clone(),
                predicate: None,
            });
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
        let mut conn = self.metadata_conn().await?;
        let rows: Vec<mysql_async::Row> = conn.exec(
            "SELECT k.CONSTRAINT_NAME, k.COLUMN_NAME, k.REFERENCED_TABLE_SCHEMA, k.REFERENCED_TABLE_NAME, k.REFERENCED_COLUMN_NAME, rc.UPDATE_RULE, rc.DELETE_RULE, k.ORDINAL_POSITION \
             FROM information_schema.KEY_COLUMN_USAGE k JOIN information_schema.REFERENTIAL_CONSTRAINTS rc \
             ON BINARY rc.CONSTRAINT_CATALOG = BINARY k.CONSTRAINT_CATALOG \
             AND BINARY rc.CONSTRAINT_SCHEMA = BINARY k.CONSTRAINT_SCHEMA \
             AND BINARY rc.CONSTRAINT_NAME = BINARY k.CONSTRAINT_NAME \
             AND BINARY rc.TABLE_NAME = BINARY k.TABLE_NAME \
             AND BINARY rc.UNIQUE_CONSTRAINT_SCHEMA = BINARY k.REFERENCED_TABLE_SCHEMA \
             AND BINARY rc.REFERENCED_TABLE_NAME = BINARY k.REFERENCED_TABLE_NAME \
             WHERE BINARY k.TABLE_SCHEMA = BINARY ? AND BINARY k.CONSTRAINT_SCHEMA = BINARY ? \
             AND BINARY k.TABLE_NAME = BINARY ? AND k.REFERENCED_TABLE_NAME IS NOT NULL \
             ORDER BY k.CONSTRAINT_NAME, k.ORDINAL_POSITION",
            (&target.database, &target.database, &target.table),
        ).await.map_err(map_query_error)?;
        let mut result: Vec<ForeignKeyInfo> = Vec::new();
        for row in &rows {
            let name: String = metadata_value(row, "CONSTRAINT_NAME")?;
            let ordinal: u64 = metadata_value(row, "ORDINAL_POSITION")?;
            let referenced_schema: String = metadata_value(row, "REFERENCED_TABLE_SCHEMA")?;
            let referenced_table: String = metadata_value(row, "REFERENCED_TABLE_NAME")?;
            if result.last().map(|fk| &fk.name) != Some(&name) {
                result.push(ForeignKeyInfo {
                    name,
                    columns: Vec::new(),
                    ref_table: referenced_table.clone(),
                    ref_database: Some(referenced_schema.clone()),
                    ref_schema: Some(referenced_schema.clone()),
                    ref_columns: Vec::new(),
                    on_update: metadata_value(row, "UPDATE_RULE")?,
                    on_delete: metadata_value(row, "DELETE_RULE")?,
                });
            }
            let fk = result.last_mut().expect("已建立外键分组");
            if ordinal != (fk.columns.len() + 1) as u64
                || fk.ref_table != referenced_table
                || fk.ref_schema.as_deref() != Some(referenced_schema.as_str())
            {
                return Err(DatabaseError::QueryError(
                    "MySQL 复合外键身份或键序号不一致".into(),
                ));
            }
            fk.columns.push(metadata_value(row, "COLUMN_NAME")?);
            fk.ref_columns
                .push(metadata_value(row, "REFERENCED_COLUMN_NAME")?);
        }
        Ok(result)
    }

    async fn disconnect(&self) -> Result<(), DatabaseError> {
        self.connected.store(false, Ordering::SeqCst);
        // Pool::disconnect等待所有借出的Conn，不能阻塞仍持有租约的调用者。
        // connected已同步失效；活动租约结束/归还时会隔离，后台再回收池。
        let pool = self.pool.clone();
        tokio::spawn(async move {
            let _ = pool.disconnect().await;
        });
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
        let mut conn = self.metadata_conn().await?;

        if let Some(db) = database {
            conn.query_drop(format!("USE `{}`", self.escape_identifier(db)))
                .await
                .map_err(map_query_error)?;
        }

        let esc = self.escape_identifier(table);
        let mut out = String::new();
        out.push_str("-- SQLink 表定义\n\n");

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
            // SHOW CREATE 的连接不跨越数据导出占用，数据由独占租约逐行读取。
            drop(conn);
            rows_written = crate::export::append_dump_data(self, database, table, &mut out).await?;
        }

        Ok((out, rows_written))
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

    fn escape_identifier(&self, identifier: &str) -> String {
        identifier.replace('`', "``")
    }
}

fn complete_table_catalog(result: &QueryResult) -> Result<&[RowData], DatabaseError> {
    if result.truncated {
        return Err(DatabaseError::QueryError(
            "EXPORT_CATALOG_INCOMPLETE: 表目录达到读取预算，不能把部分目录当作全部表；请分批选择表"
                .into(),
        ));
    }
    Ok(&result.rows)
}

#[cfg(test)]
mod export_catalog_tests {
    use super::*;
    #[test]
    fn truncated_catalog_is_rejected_even_when_no_or_valid_rows_were_retained() {
        let mut result = QueryResult {
            columns: vec![],
            rows: vec![],
            total_count: Some(0),
            execution_time_ms: 0,
            rows_affected: None,
            last_insert_id: None,
            truncated: true,
        };
        assert!(complete_table_catalog(&result)
            .unwrap_err()
            .to_string()
            .contains("EXPORT_CATALOG_INCOMPLETE"));
        result.rows.push(RowData {
            values: vec![Some(serde_json::json!("valid_table"))],
        });
        assert!(complete_table_catalog(&result).is_err());
        result.truncated = false;
        assert_eq!(
            complete_table_catalog(&result).unwrap()[0].values[0],
            Some(serde_json::json!("valid_table"))
        );
    }
}

fn mysql_params(params: &[TypedValue]) -> Result<mysql_async::Params, DatabaseError> {
    use mysql_async::Value as V;
    validate_values(params)?;
    let values = params
        .iter()
        .map(|value| {
            Ok(match value {
                TypedValue::Null => V::NULL,
                TypedValue::Text(value) | TypedValue::Decimal(value) => {
                    V::Bytes(value.as_bytes().to_vec())
                }
                TypedValue::Boolean(value) => V::Int(i64::from(*value)),
                TypedValue::Integer(value) => V::Int(
                    value
                        .parse()
                        .map_err(|_| DatabaseError::TypeError("MySQL int64 参数无效".into()))?,
                ),
                TypedValue::UnsignedInteger(value) => V::UInt(
                    value
                        .parse()
                        .map_err(|_| DatabaseError::TypeError("MySQL uint64 参数无效".into()))?,
                ),
                TypedValue::Float(value) => V::Double(*value),
                TypedValue::Binary(_) => V::Bytes(value.binary_bytes()?),
                TypedValue::Date(value) => {
                    let (year, month, day) = mysql_date_parts(value)?;
                    V::Date(year, month, day, 0, 0, 0, 0)
                }
                TypedValue::Time(value) => {
                    let (hour, minute, second, micros) =
                        mysql_time_parts(&value.value, value.precision)?;
                    V::Time(false, 0, hour, minute, second, micros)
                }
                TypedValue::Timestamp(value) => {
                    if value.timezone != TimezoneKind::None {
                        return Err(DatabaseError::TypeError(
                            "MySQL 参数协议不保存时间偏移，拒绝隐式去除时区".into(),
                        ));
                    }
                    let (year, month, day) = mysql_date_parts(&value.value[..10])?;
                    let (hour, minute, second, micros) =
                        mysql_time_parts(&value.value[11..], value.precision)?;
                    V::Date(year, month, day, hour, minute, second, micros)
                }
            })
        })
        .collect::<Result<Vec<_>, DatabaseError>>()?;
    Ok(mysql_async::Params::Positional(values))
}

fn mysql_date_parts(value: &str) -> Result<(u16, u8, u8), DatabaseError> {
    let invalid = |_| DatabaseError::TypeError("MySQL 日期参数无效".into());
    Ok((
        value[..4].parse().map_err(invalid)?,
        value[5..7].parse().map_err(invalid)?,
        value[8..10].parse().map_err(invalid)?,
    ))
}

fn mysql_time_parts(value: &str, precision: u8) -> Result<(u8, u8, u8, u32), DatabaseError> {
    if precision > 6 {
        return Err(DatabaseError::TypeError(
            "MySQL 时间精度最多为 6 位，拒绝截断参数".into(),
        ));
    }
    let invalid = |_| DatabaseError::TypeError("MySQL 时间参数无效".into());
    let micros = if precision == 0 {
        0
    } else {
        value[9..]
            .parse::<u32>()
            .map_err(|_| DatabaseError::TypeError("MySQL 时间小数无效".into()))?
            * 10u32.pow(6 - precision as u32)
    };
    Ok((
        value[..2].parse().map_err(invalid)?,
        value[3..5].parse().map_err(invalid)?,
        value[6..8].parse().map_err(invalid)?,
        micros,
    ))
}

fn mysql_fraction(micros: u32, precision: u8) -> Option<String> {
    if precision > 6 || micros >= 1_000_000 || micros % 10u32.pow(6 - precision as u32) != 0 {
        return None;
    }
    Some(if precision == 0 {
        String::new()
    } else {
        format!(".{:06}", micros)[..1 + precision as usize].into()
    })
}

/// 只声明元数据和原生载荷均可无损表达的类型；BIT、时区语义未知的 TIMESTAMP 等保守拒绝。
fn mysql_value_typed(
    value: &mysql_async::Value,
    column: &mysql_async::Column,
) -> Option<TypedValue> {
    use mysql_async::{consts::ColumnType as T, Value as V};
    if matches!(value, V::NULL) {
        return Some(TypedValue::Null);
    }
    let decoded = match (column.column_type(), value) {
        (
            T::MYSQL_TYPE_TINY
            | T::MYSQL_TYPE_SHORT
            | T::MYSQL_TYPE_LONG
            | T::MYSQL_TYPE_INT24
            | T::MYSQL_TYPE_LONGLONG,
            V::Int(value),
        ) => TypedValue::Integer(value.to_string()),
        (
            T::MYSQL_TYPE_TINY
            | T::MYSQL_TYPE_SHORT
            | T::MYSQL_TYPE_LONG
            | T::MYSQL_TYPE_INT24
            | T::MYSQL_TYPE_LONGLONG,
            V::UInt(value),
        ) => TypedValue::UnsignedInteger(value.to_string()),
        (T::MYSQL_TYPE_FLOAT, V::Float(value)) => TypedValue::Float(*value as f64),
        (T::MYSQL_TYPE_DOUBLE, V::Double(value)) => TypedValue::Float(*value),
        (T::MYSQL_TYPE_DECIMAL | T::MYSQL_TYPE_NEWDECIMAL, V::Bytes(value)) => {
            TypedValue::Decimal(std::str::from_utf8(value).ok()?.into())
        }
        (
            T::MYSQL_TYPE_STRING
            | T::MYSQL_TYPE_VAR_STRING
            | T::MYSQL_TYPE_VARCHAR
            | T::MYSQL_TYPE_BLOB
            | T::MYSQL_TYPE_TINY_BLOB
            | T::MYSQL_TYPE_MEDIUM_BLOB
            | T::MYSQL_TYPE_LONG_BLOB,
            V::Bytes(value),
        ) => {
            if column.character_set() == 63 {
                TypedValue::binary(value)
            } else {
                TypedValue::Text(std::str::from_utf8(value).ok()?.into())
            }
        }
        (T::MYSQL_TYPE_DATE | T::MYSQL_TYPE_NEWDATE, V::Date(year, month, day, 0, 0, 0, 0)) => {
            TypedValue::Date(format!("{year:04}-{month:02}-{day:02}"))
        }
        (
            T::MYSQL_TYPE_DATETIME | T::MYSQL_TYPE_DATETIME2,
            V::Date(year, month, day, hour, minute, second, micros),
        ) => TypedValue::Timestamp(TimestampValue {
            value: format!(
                "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}{}",
                mysql_fraction(*micros, column.decimals())?
            ),
            precision: column.decimals(),
            timezone: TimezoneKind::None,
        }),
        (
            T::MYSQL_TYPE_TIME | T::MYSQL_TYPE_TIME2,
            V::Time(false, 0, hour, minute, second, micros),
        ) if *hour < 24 => TypedValue::Time(TimeValue {
            value: format!(
                "{hour:02}:{minute:02}:{second:02}{}",
                mysql_fraction(*micros, column.decimals())?
            ),
            precision: column.decimals(),
        }),
        _ => return None,
    };
    decoded.validate().ok()?;
    Some(decoded)
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

#[cfg(test)]
mod typed_binding_tests {
    use super::*;
    use mysql_async::{consts::ColumnType as T, Column, Value as V};

    #[tokio::test]
    async fn disconnected_driver_rejects_metadata_without_opening_a_socket() {
        let config: ConnectionConfig = serde_json::from_value(serde_json::json!({
            "name": "离线断开验证", "driverType": "mysql", "host": "不可访问测试主机.invalid",
            "port": 3306, "user": "", "password": ""
        }))
        .unwrap();
        let opts = build_opts(&config);
        let driver = MySqlDriver {
            pool: Pool::new(opts.clone()),
            opts,
            connected: Arc::new(AtomicBool::new(false)),
            active_queries: Arc::new(StdMutex::new(HashMap::new())),
        };
        fn disconnected<T>(result: Result<T, DatabaseError>) {
            assert!(
                matches!(result, Err(DatabaseError::ConnectionError(ref message)) if message == "连接已断开")
            );
        }
        disconnected(driver.metadata_conn().await);
        disconnected(driver.get_tables(None).await);
        disconnected(driver.get_columns(None, "不存在的表").await);
        disconnected(driver.resolve_table("test", &["不存在的表".into()]).await);
        disconnected(
            driver
                .export_table_csv(None, "不存在的表", "不会创建的文件.csv")
                .await,
        );
        disconnected(driver.dump_table(None, "不存在的表", false, false).await);
        disconnected(driver.lease_session(None).await);
        driver.pool.clone().disconnect().await.unwrap();
    }

    #[test]
    fn preserves_exact_decimal_native_integer_binary_and_boolean_parameters() {
        let decimal = "12345678901234567890.12345678901234567890";
        let params = mysql_params(&[
            TypedValue::Decimal(decimal.into()),
            TypedValue::Integer(i64::MIN.to_string()),
            TypedValue::UnsignedInteger(u64::MAX.to_string()),
            TypedValue::binary(&[0, 255]),
            TypedValue::Boolean(true),
            TypedValue::Boolean(false),
            TypedValue::Null,
        ])
        .unwrap();
        assert_eq!(
            params,
            mysql_async::Params::Positional(vec![
                V::Bytes(decimal.as_bytes().to_vec()),
                V::Int(i64::MIN),
                V::UInt(u64::MAX),
                V::Bytes(vec![0, 255]),
                V::Int(1),
                V::Int(0),
                V::NULL,
            ])
        );
    }

    #[test]
    fn rejects_time_precision_loss_offsets_and_invalid_parameters() {
        for value in [
            TypedValue::Time(TimeValue {
                value: "12:34:56.1234567".into(),
                precision: 7,
            }),
            TypedValue::Timestamp(TimestampValue {
                value: "2026-10-20T12:34:56+08:00".into(),
                precision: 0,
                timezone: TimezoneKind::Offset,
            }),
            TypedValue::Integer("01".into()),
            TypedValue::Float(f64::NAN),
            TypedValue::Binary("%%%".into()),
        ] {
            assert!(mysql_params(&[value]).is_err());
        }
    }

    #[test]
    fn decoding_uses_binary_metadata_strict_utf8_and_explicit_unknowns() {
        let text = Column::new(T::MYSQL_TYPE_BLOB).with_character_set(45);
        let binary = Column::new(T::MYSQL_TYPE_BLOB).with_character_set(63);
        assert_eq!(
            mysql_value_typed(&V::Bytes(vec![0, 255]), &binary),
            Some(TypedValue::binary(&[0, 255]))
        );
        assert_eq!(mysql_value_typed(&V::Bytes(vec![255]), &text), None);
        assert_eq!(
            mysql_value_typed(&V::Bytes(b"NULL".to_vec()), &text),
            Some(TypedValue::Text("NULL".into()))
        );
        assert_eq!(mysql_value_typed(&V::NULL, &text), Some(TypedValue::Null));
        assert_eq!(
            mysql_value_typed(&V::Bytes(vec![1]), &Column::new(T::MYSQL_TYPE_BIT)),
            None
        );
        assert_eq!(
            mysql_value_typed(
                &V::Date(2026, 10, 20, 12, 34, 56, 123456),
                &Column::new(T::MYSQL_TYPE_TIMESTAMP).with_decimals(6)
            ),
            None
        );
    }

    #[test]
    fn dates_and_fractional_times_preserve_precision_or_return_unknown() {
        let datetime = Column::new(T::MYSQL_TYPE_DATETIME).with_decimals(6);
        assert_eq!(
            mysql_value_typed(&V::Date(2026, 10, 20, 12, 34, 56, 123456), &datetime),
            Some(TypedValue::Timestamp(TimestampValue {
                value: "2026-10-20T12:34:56.123456".into(),
                precision: 6,
                timezone: TimezoneKind::None,
            }))
        );
        assert_eq!(
            mysql_value_typed(
                &V::Date(0, 0, 0, 0, 0, 0, 0),
                &Column::new(T::MYSQL_TYPE_DATE)
            ),
            None
        );
        assert_eq!(
            mysql_value_typed(
                &V::Time(true, 0, 1, 2, 3, 0),
                &Column::new(T::MYSQL_TYPE_TIME)
            ),
            None
        );
        assert_eq!(
            mysql_value_typed(
                &V::Time(false, 1, 1, 2, 3, 0),
                &Column::new(T::MYSQL_TYPE_TIME)
            ),
            None
        );
        assert_eq!(
            mysql_value_typed(
                &V::Time(false, 0, 1, 2, 3, 123456),
                &Column::new(T::MYSQL_TYPE_TIME).with_decimals(3)
            ),
            None
        );
        let time = TypedValue::Time(TimeValue {
            value: "01:02:03.120".into(),
            precision: 3,
        });
        assert_eq!(
            mysql_params(&[time.clone()]).unwrap(),
            mysql_async::Params::Positional(vec![V::Time(false, 0, 1, 2, 3, 120000)])
        );
        assert_eq!(
            mysql_value_typed(
                &V::Time(false, 0, 1, 2, 3, 120000),
                &Column::new(T::MYSQL_TYPE_TIME).with_decimals(3)
            ),
            Some(time)
        );
    }
}
