//! 有界原值导出。文件提交与数据库读取分开，未完成的导出不能覆盖旧目标。
use crate::atomic_export::AtomicExportFile;
use crate::database::{pool::ConnectionPoolManager, DatabaseDriver};
use crate::models::{DatabaseError, DumpSummary, ResolvedTable};
use crate::session::SessionLease;
use crate::task_registry::{CleanupState, CommitState, TaskGuard, TaskPhase};
use crate::typed_value::{TypedValue, MAX_CELL_BYTES};
use std::io::Write;
use std::sync::{Arc, Mutex};

pub const MAX_EXPORT_ROW_BYTES: usize = 16 * 1024 * 1024;
/// 旧 String API 仅作有界兼容；真正的文件导出不受总行数和总字节数限制。
pub const MAX_LEGACY_DUMP_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_SQL_STATEMENT_BYTES: usize = 8 * 1024 * 1024;

/// 原子替换也不能覆盖正在使用的数据库及其日志。硬链接别名替换不会改写源 inode。
pub fn reject_database_paths(path: &str, sources: &[String]) -> Result<(), DatabaseError> {
    fn normalized(path: &std::path::Path) -> Result<std::path::PathBuf, DatabaseError> {
        let absolute = std::path::absolute(path).map_err(io_error)?;
        if absolute.exists() {
            return absolute.canonicalize().map_err(io_error);
        }
        let parent = absolute
            .parent()
            .ok_or_else(|| protocol_error("导出路径缺少父目录"))?
            .canonicalize()
            .map_err(io_error)?;
        Ok(parent.join(
            absolute
                .file_name()
                .ok_or_else(|| protocol_error("导出路径缺少文件名"))?,
        ))
    }
    let output = normalized(std::path::Path::new(path))?;
    for source in sources
        .iter()
        .filter(|source| !source.is_empty() && source.as_str() != ":memory:")
    {
        for suffix in ["", "-wal", "-shm", "-journal"] {
            let candidate = normalized(std::path::Path::new(&format!("{source}{suffix}")))?;
            let same = if cfg!(windows) {
                output
                    .to_string_lossy()
                    .eq_ignore_ascii_case(&candidate.to_string_lossy())
            } else {
                output == candidate
            };
            if same {
                return Err(protocol_error(
                    "目标路径是活动 SQLite 数据库或日志文件，不能用导出结果替换",
                ));
            }
        }
    }
    Ok(())
}

pub struct ExportColumn {
    pub name: String,
}
pub trait ExportSink: Send + Sync {
    fn columns(&self, columns: &[ExportColumn]) -> Result<(), DatabaseError>;
    fn row(&self, row: &[TypedValue]) -> Result<(), DatabaseError>;
    fn check_cancelled(&self) -> Result<(), DatabaseError>;
}

pub fn check_raw_lengths(lengths: impl IntoIterator<Item = usize>) -> Result<(), DatabaseError> {
    let mut total = 0usize;
    for length in lengths {
        if length > MAX_CELL_BYTES || length > MAX_EXPORT_ROW_BYTES.saturating_sub(total) {
            return Err(DatabaseError::TypeError(
                "EXPORT_VALUE_LIMIT: 原始单元格超过 1 MiB 或单行超过 16 MiB；导出已停止，没有截断值".into(),
            ));
        }
        total += length;
    }
    Ok(())
}
fn io_error(error: std::io::Error) -> DatabaseError {
    DatabaseError::Unknown(format!("EXPORT_WRITE_FAILED: 写入导出文件失败：{error}"))
}
fn protocol_error(message: &str) -> DatabaseError {
    DatabaseError::TypeError(format!("EXPORT_PROTOCOL_ERROR: {message}"))
}
fn quote_identifier(dialect: &str, value: &str) -> String {
    match dialect {
        "mysql" | "mariadb" => format!("`{}`", value.replace('`', "``")),
        "mssql" => format!("[{}]", value.replace(']', "]]")),
        _ => format!("\"{}\"", value.replace('"', "\"\"")),
    }
}
fn qualified(dialect: &str, target: &ResolvedTable) -> String {
    let schema = match dialect {
        "mysql" | "mariadb" => Some(target.database.as_str()),
        _ => target.schema.as_deref(),
    };
    match schema {
        Some(schema) => format!(
            "{}.{}",
            quote_identifier(dialect, schema),
            quote_identifier(dialect, &target.table)
        ),
        None => quote_identifier(dialect, &target.table),
    }
}
struct TablePlan {
    target: ResolvedTable,
    sql: String,
    table_sql: String,
}
async fn prepare_table<D: DatabaseDriver + ?Sized>(
    driver: &D,
    database: Option<&str>,
    table: &str,
    dump: bool,
) -> Result<TablePlan, DatabaseError> {
    let db = crate::database::current_database(driver, database).await?;
    // 入口的表名是一段实际目录名称，名称里的点不能被重新解释为 schema 分隔符。
    let target = driver.resolve_table(&db, &[table.to_owned()]).await?;
    prepare_resolved(driver.driver_type(), target, dump)
}
fn prepare_resolved(
    dialect: &str,
    target: ResolvedTable,
    dump: bool,
) -> Result<TablePlan, DatabaseError> {
    let read_table = qualified(dialect, &target);
    // MySQL 的 SHOW CREATE TABLE 不带源库；恢复时沿用导入租约的目标库。
    let table_sql = if matches!(dialect, "mysql" | "mariadb") {
        quote_identifier(dialect, &target.table)
    } else {
        read_table.clone()
    };
    let columns: Vec<_> = target
        .columns
        .iter()
        .filter(|column| {
            let extra = column
                .extra
                .as_deref()
                .unwrap_or_default()
                .to_ascii_lowercase();
            !dump
                || !(extra.contains("generated virtual")
                    || extra.contains("generated stored")
                    || extra.contains("virtual generated")
                    || extra.contains("stored generated")
                    || extra.contains("computed"))
        })
        .map(|column| quote_identifier(dialect, &column.name))
        .collect();
    if columns.is_empty() {
        return Err(protocol_error("没有可导出的真实列"));
    }
    // 当前 DDL 兼容层不能重建 identity / GENERATED ALWAYS 的全部插入语义，先明确拒绝。
    if dump
        && target.columns.iter().any(|column| {
            column.extra.as_deref().is_some_and(|extra| {
                let extra = extra.to_ascii_lowercase();
                extra.contains("identity")
                    || (matches!(dialect, "postgres" | "mssql")
                        && (extra.contains("generated") || extra.contains("computed")))
            })
        })
    {
        return Err(DatabaseError::TypeError(
            "EXPORT_SCHEMA_UNSUPPORTED: identity 列的恢复语义尚未验证，请使用 CSV；旧文件未替换"
                .into(),
        ));
    }
    let sql = format!("SELECT {} FROM {}", columns.join(", "), read_table);
    Ok(TablePlan {
        target,
        sql,
        table_sql,
    })
}

enum Output {
    File(AtomicExportFile),
    Memory(Vec<u8>),
}
impl Write for Output {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        match self {
            Self::File(file) => file.write(bytes),
            Self::Memory(buffer) => {
                if bytes.len() > MAX_LEGACY_DUMP_BYTES.saturating_sub(buffer.len()) {
                    return Err(std::io::Error::other(
                        "EXPORT_LEGACY_LIMIT: 文本兼容接口超过 16 MiB，请使用文件导出",
                    ));
                }
                buffer.write(bytes)
            }
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Self::File(file) => file.flush(),
            Self::Memory(_) => Ok(()),
        }
    }
}
type SharedOutput = Arc<Mutex<Option<Output>>>;
type CancelCheck = Arc<dyn Fn() -> bool + Send + Sync>;
enum Format {
    Csv,
    Sql { dialect: String, table: String },
}
struct EncoderState {
    columns: Option<usize>,
    quoted_columns: String,
    batch: usize,
    batch_bytes: usize,
}
struct Encoder {
    output: SharedOutput,
    format: Format,
    state: Mutex<EncoderState>,
    cancelled: CancelCheck,
}
impl Encoder {
    fn new(output: SharedOutput, format: Format, cancelled: CancelCheck) -> Self {
        Self {
            output,
            format,
            state: Mutex::new(EncoderState {
                columns: None,
                quoted_columns: String::new(),
                batch: 0,
                batch_bytes: 0,
            }),
            cancelled,
        }
    }
    fn with_writer<T>(
        &self,
        run: impl FnOnce(&mut Output) -> std::io::Result<T>,
    ) -> Result<T, DatabaseError> {
        let mut output = self
            .output
            .lock()
            .map_err(|_| protocol_error("输出锁失效"))?;
        run(output
            .as_mut()
            .ok_or_else(|| protocol_error("导出已经结束"))?)
        .map_err(io_error)
    }
    fn finish(&self) -> Result<(), DatabaseError> {
        self.check_cancelled()?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| protocol_error("编码状态失效"))?;
        if state.batch > 0 {
            self.with_writer(|out| out.write_all(b";\n\n"))?;
            state.batch = 0;
        }
        Ok(())
    }
}
fn csv_text(out: &mut impl Write, text: &str) -> std::io::Result<()> {
    out.write_all(b"\"")?;
    for part in text.split_inclusive('"') {
        out.write_all(part.as_bytes())?;
        if part.ends_with('"') {
            out.write_all(b"\"")?;
        }
    }
    out.write_all(b"\"")
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
fn csv_value(out: &mut impl Write, value: &TypedValue) -> Result<(), DatabaseError> {
    match value {
        TypedValue::Null => out.write_all(b"\\N").map_err(io_error),
        TypedValue::Text(text) | TypedValue::Date(text) => csv_text(out, text).map_err(io_error),
        TypedValue::Time(value) => csv_text(out, &value.value).map_err(io_error),
        TypedValue::Timestamp(value) => csv_text(out, &value.value).map_err(io_error),
        TypedValue::Binary(_) => {
            csv_text(out, &format!("\\x{}", hex(&value.binary_bytes()?))).map_err(io_error)
        }
        TypedValue::Integer(text)
        | TypedValue::UnsignedInteger(text)
        | TypedValue::Decimal(text) => out.write_all(text.as_bytes()).map_err(io_error),
        TypedValue::Boolean(value) => out
            .write_all(if *value { b"true" } else { b"false" })
            .map_err(io_error),
        TypedValue::Float(value) => out
            .write_all(
                serde_json::to_string(value)
                    .map_err(|_| protocol_error("浮点编码失败"))?
                    .as_bytes(),
            )
            .map_err(io_error),
    }
}
fn sql_text(dialect: &str, text: &str) -> String {
    match dialect {
        // 不依赖 NO_BACKSLASH_ESCAPES；文本与二进制保持不同的 SQL 类型。
        "mysql" | "mariadb" => format!("CONVERT(X'{}' USING utf8mb4)", hex(text.as_bytes())),
        "postgres" => format!("E'{}'", text.replace('\\', "\\\\").replace('\'', "''")),
        "mssql" => format!("N'{}'", text.replace('\'', "''")),
        _ if text.contains('\0') => format!("CAST(X'{}' AS TEXT)", hex(text.as_bytes())),
        _ => format!("'{}'", text.replace('\'', "''")),
    }
}
fn sql_value(dialect: &str, value: &TypedValue) -> Result<String, DatabaseError> {
    value.validate()?;
    Ok(match value {
        TypedValue::Null => "NULL".into(),
        TypedValue::Text(text) | TypedValue::Date(text) => sql_text(dialect, text),
        TypedValue::Time(value) => sql_text(dialect, &value.value),
        TypedValue::Timestamp(value) => sql_text(dialect, &value.value),
        TypedValue::Integer(text)
        | TypedValue::UnsignedInteger(text)
        | TypedValue::Decimal(text) => text.clone(),
        TypedValue::Float(value) => {
            serde_json::to_string(value).map_err(|_| protocol_error("浮点编码失败"))?
        }
        TypedValue::Boolean(value) if dialect == "postgres" => value.to_string(),
        TypedValue::Boolean(value) => if *value { "1" } else { "0" }.into(),
        TypedValue::Binary(_) => {
            let encoded = hex(&value.binary_bytes()?);
            match dialect {
                "postgres" => format!("decode('{encoded}', 'hex')"),
                "mssql" => format!("0x{encoded}"),
                _ => format!("X'{encoded}'"),
            }
        }
    })
}
impl ExportSink for Encoder {
    fn check_cancelled(&self) -> Result<(), DatabaseError> {
        if (self.cancelled)() {
            Err(DatabaseError::QueryCancelled)
        } else {
            Ok(())
        }
    }
    fn columns(&self, columns: &[ExportColumn]) -> Result<(), DatabaseError> {
        self.check_cancelled()?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| protocol_error("编码状态失效"))?;
        if state.columns.is_some() || columns.is_empty() {
            return Err(protocol_error("不支持多结果或无列导出"));
        }
        check_raw_lengths(columns.iter().map(|column| column.name.len()))?;
        state.columns = Some(columns.len());
        match &self.format {
            Format::Csv => self.with_writer(|out| {
                for (index, column) in columns.iter().enumerate() {
                    if index > 0 {
                        out.write_all(b",")?;
                    }
                    csv_text(out, &column.name)?;
                }
                out.write_all(b"\n")
            })?,
            Format::Sql { dialect, .. } => {
                state.quoted_columns = columns
                    .iter()
                    .map(|column| quote_identifier(dialect, &column.name))
                    .collect::<Vec<_>>()
                    .join(", ");
            }
        }
        Ok(())
    }
    fn row(&self, row: &[TypedValue]) -> Result<(), DatabaseError> {
        self.check_cancelled()?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| protocol_error("编码状态失效"))?;
        if state.columns != Some(row.len()) {
            return Err(protocol_error("行列映射不一致"));
        }
        for value in row {
            value.validate()?;
        }
        // Base64 等原值编码有有限膨胀；不能再次把编码后的长度当成原始单元格长度。
        let retained = row.iter().fold(0usize, |sum, value| {
            sum.saturating_add(value.allocated_bytes())
        });
        if retained > MAX_EXPORT_ROW_BYTES * 2 {
            return Err(protocol_error("类型化单行持有值超过 32 MiB"));
        }
        match &self.format {
            Format::Csv => {
                let mut output = self
                    .output
                    .lock()
                    .map_err(|_| protocol_error("输出锁失效"))?;
                let out = output
                    .as_mut()
                    .ok_or_else(|| protocol_error("导出已经结束"))?;
                for (index, value) in row.iter().enumerate() {
                    if index > 0 {
                        out.write_all(b",").map_err(io_error)?;
                    }
                    csv_value(out, value)?;
                }
                out.write_all(b"\n").map_err(io_error)?;
            }
            Format::Sql { dialect, table } => {
                let header = format!("INSERT INTO {table} ({}) VALUES\n", state.quoted_columns);
                let mut encoded = Vec::with_capacity(row.len());
                let mut row_bytes = 2usize;
                for value in row {
                    let value = sql_value(dialect, value)?;
                    row_bytes = row_bytes.saturating_add(value.len()).saturating_add(2);
                    if row_bytes.saturating_add(header.len()).saturating_add(4)
                        > MAX_SQL_STATEMENT_BYTES
                    {
                        return Err(protocol_error(
                            "单条 INSERT 超过 8 MiB，不能生成无法有界导入的语句",
                        ));
                    }
                    encoded.push(value);
                }
                let mut output = self
                    .output
                    .lock()
                    .map_err(|_| protocol_error("输出锁失效"))?;
                let out = output
                    .as_mut()
                    .ok_or_else(|| protocol_error("导出已经结束"))?;
                if state.batch > 0
                    && state
                        .batch_bytes
                        .saturating_add(row_bytes)
                        .saturating_add(4)
                        > MAX_SQL_STATEMENT_BYTES
                {
                    out.write_all(b";\n\n").map_err(io_error)?;
                    state.batch = 0;
                }
                if state.batch == 0 {
                    out.write_all(header.as_bytes()).map_err(io_error)?;
                    state.batch_bytes = header.len();
                } else {
                    out.write_all(b",\n").map_err(io_error)?;
                }
                out.write_all(b"(").map_err(io_error)?;
                for (index, value) in encoded.iter().enumerate() {
                    if index > 0 {
                        out.write_all(b", ").map_err(io_error)?;
                    }
                    out.write_all(value.as_bytes()).map_err(io_error)?;
                }
                out.write_all(b")").map_err(io_error)?;
                state.batch += 1;
                state.batch_bytes += row_bytes;
                if state.batch == 100 {
                    out.write_all(b";\n\n").map_err(io_error)?;
                    state.batch = 0;
                }
            }
        }
        Ok(())
    }
}
fn never_cancelled() -> CancelCheck {
    Arc::new(|| false)
}
async fn finish_lease<T>(
    lease: Box<dyn SessionLease>,
    outcome: Result<T, DatabaseError>,
) -> Result<T, DatabaseError> {
    let cleanup = lease.release().await;
    combine_cleanup(outcome, cleanup)
}
fn combine_cleanup<T>(
    outcome: Result<T, DatabaseError>,
    cleanup: Result<(), DatabaseError>,
) -> Result<T, DatabaseError> {
    match (outcome, cleanup) {
        (Err(error), Err(cleanup)) => Err(DatabaseError::Unknown(format!("EXPORT_CLEANUP_FAILED: 原始导出错误：{error}；会话清理也失败：{cleanup}；旧文件未替换，执行停止状态需要核查"))),
        (Err(error), Ok(())) => Err(error),
        (Ok(value), Ok(())) => Ok(value),
        (Ok(_), Err(_)) => Err(DatabaseError::Unknown(
            "EXPORT_CLEANUP_FAILED: 读取已结束但会话清理失败，旧文件未替换".into(),
        )),
    }
}
fn commit_output(output: &SharedOutput) -> Result<(), DatabaseError> {
    let output = output
        .lock()
        .map_err(|_| protocol_error("输出锁失效"))?
        .take()
        .ok_or_else(|| protocol_error("文件已经提交"))?;
    match output {
        Output::File(file) => file.commit(),
        Output::Memory(_) => Err(protocol_error("不能提交内存输出")),
    }
}
pub async fn export_csv<D: DatabaseDriver + ?Sized>(
    driver: &D,
    database: Option<&str>,
    table: &str,
    path: &str,
) -> Result<u64, DatabaseError> {
    let plan = prepare_table(driver, database, table, false).await?;
    driver.validate_export_path(path).await?;
    let output = Arc::new(Mutex::new(Some(Output::File(AtomicExportFile::create(
        path,
    )?))));
    let sink = Arc::new(Encoder::new(output.clone(), Format::Csv, never_cancelled()));
    let mut lease = driver.lease_session(database).await?;
    let outcome = lease.stream_export(&plan.sql, sink.clone()).await;
    let count = finish_lease(lease, outcome).await?;
    sink.finish()?;
    driver.validate_export_path(path).await?;
    commit_output(&output)?;
    Ok(count)
}
pub async fn append_dump_data<D: DatabaseDriver + ?Sized>(
    driver: &D,
    database: Option<&str>,
    table: &str,
    out: &mut String,
) -> Result<u64, DatabaseError> {
    let target = crate::metadata::resolve_legacy(driver, database, table).await?;
    append_dump_data_for(driver, &target, out).await
}
pub async fn append_dump_data_for<D: DatabaseDriver + ?Sized>(
    driver: &D,
    target: &ResolvedTable,
    out: &mut String,
) -> Result<u64, DatabaseError> {
    let plan = prepare_resolved(driver.driver_type(), target.clone(), true)?;
    let output = Arc::new(Mutex::new(Some(Output::Memory(Vec::new()))));
    let sink = Arc::new(Encoder::new(
        output.clone(),
        Format::Sql {
            dialect: driver.driver_type().into(),
            table: plan.table_sql,
        },
        never_cancelled(),
    ));
    let mut lease = driver.lease_session(Some(&target.database)).await?;
    let outcome = lease.stream_export(&plan.sql, sink.clone()).await;
    let count = finish_lease(lease, outcome).await?;
    sink.finish()?;
    let output = output
        .lock()
        .map_err(|_| protocol_error("输出锁失效"))?
        .take();
    if let Some(Output::Memory(bytes)) = output {
        if bytes.len() > MAX_LEGACY_DUMP_BYTES.saturating_sub(out.len()) {
            return Err(protocol_error("旧文本转储接口超过 16 MiB，请使用文件导出"));
        }
        out.push_str(std::str::from_utf8(&bytes).map_err(|_| protocol_error("SQL 不是 UTF-8"))?);
    }
    Ok(count)
}

/// CLI/测试与 IPC 共用的文件导出入口。多表共用一份文件与一个租约；不许诺整库快照。
pub async fn export_job(
    manager: &ConnectionPoolManager,
    connection_id: String,
    database: Option<String>,
    tables: Option<Vec<String>>,
    path: String,
    sql: bool,
    include_drop: bool,
    include_data: bool,
    task_id: Option<String>,
) -> Result<DumpSummary, String> {
    export_job_sources(
        manager,
        connection_id,
        database,
        tables,
        None,
        path,
        sql,
        include_drop,
        include_data,
        task_id,
    )
    .await
}
pub async fn export_job_sources(
    manager: &ConnectionPoolManager,
    connection_id: String,
    database: Option<String>,
    tables: Option<Vec<String>>,
    table_sources: Option<Vec<crate::metadata::ObjectIdentity>>,
    path: String,
    sql: bool,
    include_drop: bool,
    include_data: bool,
    task_id: Option<String>,
) -> Result<DumpSummary, String> {
    let mut task = manager
        .tasks
        .start(task_id, &connection_id, None, "export")
        .map_err(|error| error.to_string())?;
    let outcome = export_job_inner(
        manager,
        &task,
        &connection_id,
        database.as_deref(),
        tables,
        table_sources,
        &path,
        sql,
        include_drop,
        include_data,
    )
    .await;
    let cancelled = matches!(&outcome, Err(error) if matches!(error,DatabaseError::QueryCancelled));
    let committing = task
        .snapshot()
        .is_ok_and(|snapshot| snapshot.phase == TaskPhase::Committing);
    let replaced = outcome.as_ref().is_err_and(|error| {
        error
            .to_string()
            .contains("EXPORT_FILE_REPLACED_DURABILITY_UNKNOWN")
    });
    let cleanup_failed = outcome
        .as_ref()
        .is_err_and(|error| error.to_string().contains("EXPORT_CLEANUP_FAILED"));
    let _ = task.finish(
        if outcome.is_ok() {
            TaskPhase::Succeeded
        } else if committing || cleanup_failed {
            TaskPhase::Unknown
        } else if cancelled {
            TaskPhase::Cancelled
        } else {
            TaskPhase::Failed
        },
        if outcome.is_ok() || replaced {
            CommitState::Committed
        } else if committing {
            CommitState::Unknown
        } else {
            CommitState::NotApplicable
        },
        CleanupState::NotStarted,
        if cleanup_failed {
            CleanupState::Failed
        } else {
            CleanupState::Succeeded
        },
        outcome.as_ref().err().map(|_| {
            if cancelled {
                "EXPORT_CANCELLED"
            } else {
                "EXPORT_FAILED"
            }
        }),
    );
    outcome.map_err(|error| error.to_string())
}
async fn export_job_inner(
    manager: &ConnectionPoolManager,
    task: &TaskGuard,
    connection_id: &str,
    database: Option<&str>,
    tables: Option<Vec<String>>,
    table_sources: Option<Vec<crate::metadata::ObjectIdentity>>,
    path: &str,
    sql: bool,
    include_drop: bool,
    include_data: bool,
) -> Result<DumpSummary, DatabaseError> {
    let snapshot = manager.get_connection_snapshot(connection_id).await?;
    task.prepare(&snapshot.generation, snapshot.driver.session_capabilities())
        .map_err(|e| protocol_error(&e.to_string()))?;
    let check: CancelCheck = {
        let tasks = manager.tasks.clone();
        let id = task.task_id().to_owned();
        Arc::new(move || {
            tasks.snapshot(&id).map_or(true, |snapshot| {
                snapshot.cancel_requested || snapshot.phase.terminal()
            })
        })
    };
    if check() {
        return Err(DatabaseError::QueryCancelled);
    }
    tokio::select! { biased; _ = task.cancelled() => return Err(DatabaseError::QueryCancelled), result = snapshot.driver.validate_export_path(path) => result? };
    let requested_database = crate::database::current_database(&*snapshot.driver, database).await?;
    // SQLite 的 UI 库标签与实际文件身份分开；schema（main/temp/ATTACH）独立传递。
    let sqlite = snapshot.driver.driver_type() == "sqlite";
    let database_name = if sqlite {
        snapshot
            .database
            .clone()
            .unwrap_or_else(|| requested_database.clone())
    } else {
        requested_database.clone()
    };
    let mut sources = match (table_sources, tables) {
        (Some(sources), _) if !sources.is_empty() => sources,
        (Some(_), _) => return Err(protocol_error("显式对象清单不能为空")),
        (_, Some(tables)) if !tables.is_empty() => tables.into_iter().map(|table| crate::metadata::ObjectIdentity { database: database_name.clone(), schema: None, table }).collect(),
        _ if sql => tokio::select! { biased; _ = task.cancelled() => return Err(DatabaseError::QueryCancelled), result = snapshot.driver.get_tables(database) => result? }
            .into_iter()
            .map(|table| crate::metadata::ObjectIdentity { database: database_name.clone(), schema: table.schema, table: table.name })
            .collect(),
        _ => return Err(protocol_error("CSV 必须指定一个表")),
    };
    if sqlite {
        for source in &mut sources {
            if source.database == requested_database {
                source.database = database_name.clone();
            }
        }
    }
    if !sql && sources.len() != 1 {
        return Err(protocol_error("CSV 只能导出一个表"));
    }
    let output = Arc::new(Mutex::new(Some(Output::File(AtomicExportFile::create(
        path,
    )?))));
    if sql {
        output
            .lock()
            .map_err(|_| protocol_error("输出锁失效"))?
            .as_mut()
            .unwrap()
            .write_all(
                "-- SQLink SQL 转储\n-- 一致性：各 SELECT 独立读取；不承诺整库快照。\n\n"
                    .as_bytes(),
            )
            .map_err(io_error)?;
    }
    // 所有目录预检先完成，SQLite 元数据不能在持有其独占租约时再申请租约。
    let mut plans = Vec::new();
    let mut plan_bytes = sources.iter().fold(
        sources
            .len()
            .saturating_mul(std::mem::size_of::<crate::metadata::ObjectIdentity>()),
        |sum, source| {
            sum.saturating_add(source.database.capacity())
                .saturating_add(source.table.capacity())
                .saturating_add(source.schema.as_ref().map_or(0, String::capacity))
        },
    );
    if plan_bytes > MAX_EXPORT_ROW_BYTES {
        return Err(protocol_error("选择的表目录超过 16 MiB，请分批导出"));
    }
    for source in &sources {
        if check() {
            return Err(DatabaseError::QueryCancelled);
        }
        if source.database != database_name {
            return Err(protocol_error("同一导出任务不能混用不同数据库"));
        }
        let parts = source.parts(snapshot.driver.driver_type());
        let target = tokio::select! { biased; _ = task.cancelled() => return Err(DatabaseError::QueryCancelled), result = snapshot.driver.resolve_table(&source.database, &parts) => result? };
        let plan = prepare_resolved(snapshot.driver.driver_type(), target, sql)?;
        let ddl = if sql {
            tokio::select! { biased; _ = task.cancelled() => return Err(DatabaseError::QueryCancelled), result = snapshot.driver.dump_table_for(
                    &plan.target,
                    include_drop,
                    false,
                ) => result? }.0
        } else {
            String::new()
        };
        if ddl.len() > MAX_SQL_STATEMENT_BYTES {
            return Err(protocol_error("单表定义超过 8 MiB"));
        }
        // DDL 先逐表落临时文件，避免把整个数据库的所有定义收成一个 String。
        if sql {
            output
                .lock()
                .map_err(|_| protocol_error("输出锁失效"))?
                .as_mut()
                .unwrap()
                .write_all(ddl.as_bytes())
                .map_err(io_error)?;
        }
        plan_bytes = plan_bytes
            .saturating_add(plan.sql.len())
            .saturating_add(plan.table_sql.len());
        if plan_bytes > MAX_EXPORT_ROW_BYTES {
            return Err(protocol_error("导出目录计划超过 16 MiB，请分批选择表"));
        }
        // 逐表释放实际目录，只保留执行需要的 SQL 与恢复目标。
        plans.push((plan.sql, plan.table_sql));
    }
    let mut lease = tokio::select! {
        biased;
        _ = task.cancelled() => return Err(DatabaseError::QueryCancelled),
        result = snapshot.driver.lease_session(database) => result?,
    };
    if let Err(error) = task.attach(&*lease).and_then(|_| task.running()) {
        let _ = lease.release().await;
        return Err(if check() {
            DatabaseError::QueryCancelled
        } else {
            protocol_error(&error.to_string())
        });
    }
    let result = async {
        let mut count = 0u64;
        for (read_sql, table_sql) in plans {
            if check() {
                return Err(DatabaseError::QueryCancelled);
            }
            if !sql || include_data {
                let format = if sql {
                    Format::Sql {
                        dialect: snapshot.driver.driver_type().into(),
                        table: table_sql,
                    }
                } else {
                    Format::Csv
                };
                let sink = Arc::new(Encoder::new(output.clone(), format, check.clone()));
                count += lease.stream_export(&read_sql, sink.clone()).await?;
                sink.finish()?;
            }
        }
        Ok(DumpSummary {
            tables: sources.len() as u64,
            rows: count,
        })
    }
    .await;
    let result = finish_lease(lease, result).await?;
    snapshot.driver.validate_export_path(path).await?;
    if check() {
        return Err(DatabaseError::QueryCancelled);
    }
    if manager
        .active_connection_snapshot(connection_id)
        .await
        .map_err(|error| protocol_error(&error.to_string()))?
        .generation
        != snapshot.generation
    {
        return Err(protocol_error("连接代次已改变，旧导出文件未提交"));
    }
    // 锁住取消/提交的竞态边界，文件替换后不把迟到取消当作未导出。
    task.committing().map_err(|error| {
        if check() {
            DatabaseError::QueryCancelled
        } else {
            protocol_error(&error.to_string())
        }
    })?;
    if let Err(error) = commit_output(&output) {
        if error
            .to_string()
            .contains("EXPORT_FILE_REPLACED_DURABILITY_UNKNOWN")
        {
            task.committed()
                .map_err(|error| protocol_error(&error.to_string()))?;
        }
        return Err(error);
    }
    task.committed()
        .map_err(|error| protocol_error(&error.to_string()))?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn simultaneous_read_and_cleanup_failures_keep_both_causes_and_unknown_cleanup() {
        for original in [
            DatabaseError::QueryCancelled,
            DatabaseError::Unknown("磁盘写入失败".into()),
        ] {
            let original_text = original.to_string();
            let error = combine_cleanup::<()>(
                Err(original),
                Err(DatabaseError::ConnectionError("回滚/关闭未确认".into())),
            )
            .unwrap_err()
            .to_string();
            assert!(error.contains("EXPORT_CLEANUP_FAILED"));
            assert!(error.contains(&original_text));
            assert!(error.contains("回滚/关闭未确认"));
            assert!(error.contains("停止状态需要核查"));
        }
        assert!(matches!(
            combine_cleanup::<()>(Err(DatabaseError::QueryCancelled), Ok(())),
            Err(DatabaseError::QueryCancelled)
        ));
    }
    fn memory_encoder(format: Format) -> (SharedOutput, Encoder) {
        let output = Arc::new(Mutex::new(Some(Output::Memory(Vec::new()))));
        let encoder = Encoder::new(output.clone(), format, never_cancelled());
        (output, encoder)
    }
    fn text(output: &SharedOutput) -> String {
        let locked = output.lock().unwrap();
        match locked.as_ref().unwrap() {
            Output::Memory(bytes) => String::from_utf8(bytes.clone()).unwrap(),
            _ => panic!("测试必须用内存输出"),
        }
    }
    #[test]
    fn csv_distinguishes_null_empty_text_binary_and_numeric_text_exactly() {
        let (output, encoder) = memory_encoder(Format::Csv);
        let values = [
            TypedValue::Null,
            TypedValue::Text(String::new()),
            TypedValue::Text("\\N".into()),
            TypedValue::Text("9007199254740993".into()),
            TypedValue::Integer("9007199254740993".into()),
            TypedValue::binary(&[0, 255]),
            TypedValue::binary(&[]),
            TypedValue::Text("中,\"\n".into()),
        ];
        encoder
            .columns(
                &(0..values.len())
                    .map(|index| ExportColumn {
                        name: index.to_string(),
                    })
                    .collect::<Vec<_>>(),
            )
            .unwrap();
        encoder.row(&values).unwrap();
        encoder.finish().unwrap();
        assert_eq!(text(&output), "\"0\",\"1\",\"2\",\"3\",\"4\",\"5\",\"6\",\"7\"\n\\N,\"\",\"\\N\",\"9007199254740993\",9007199254740993,\"\\x00ff\",\"\\x\",\"中,\"\"\n\"\n");
    }
    #[test]
    fn dialect_literals_keep_binary_and_text_separate_without_sql_mode_assumptions() {
        let value = TypedValue::Text("a'\\\0中".into());
        assert_eq!(
            sql_value("mysql", &value).unwrap(),
            "CONVERT(X'61275c00e4b8ad' USING utf8mb4)"
        );
        assert_eq!(
            sql_value("mysql", &TypedValue::binary(b"a'\\\0\xe4\xb8\xad")).unwrap(),
            "X'61275c00e4b8ad'"
        );
        assert_eq!(
            sql_value("sqlite", &value).unwrap(),
            "CAST(X'61275c00e4b8ad' AS TEXT)"
        );
        assert_eq!(
            sql_value("postgres", &TypedValue::Text("a'\\b".into())).unwrap(),
            "E'a''\\\\b'"
        );
        assert_eq!(
            sql_value("mssql", &TypedValue::Text("中'".into())).unwrap(),
            "N'中'''"
        );
        assert_eq!(sql_value("sqlite", &TypedValue::Float(1.0)).unwrap(), "1.0");
    }
    #[test]
    fn sql_batches_obey_byte_budget_before_the_hundred_row_boundary() {
        let (output, encoder) = memory_encoder(Format::Sql {
            dialect: "sqlite".into(),
            table: "\"t\"".into(),
        });
        encoder
            .columns(&[ExportColumn { name: "v".into() }])
            .unwrap();
        let value = TypedValue::Text("a".repeat(MAX_CELL_BYTES));
        for _ in 0..9 {
            encoder.row(std::slice::from_ref(&value)).unwrap();
        }
        encoder.finish().unwrap();
        let text = text(&output);
        let mut source = std::io::Cursor::new(text.as_bytes());
        let mut splitter = crate::database::dump::SqlSplitter::new(false);
        let mut count = 0;
        while let Some(statement) = splitter.next_statement(&mut source).unwrap() {
            assert!(statement.len() <= MAX_SQL_STATEMENT_BYTES);
            count += 1;
        }
        assert_eq!(count, 2);
    }
    #[test]
    fn unsupported_values_and_shape_errors_never_become_fabricated_nulls() {
        let (output, encoder) = memory_encoder(Format::Csv);
        encoder
            .columns(&[ExportColumn { name: "v".into() }])
            .unwrap();
        let header = text(&output);
        assert!(encoder.row(&[TypedValue::Float(f64::NAN)]).is_err());
        assert!(encoder.row(&[]).is_err());
        assert!(encoder
            .columns(&[ExportColumn {
                name: "later".into()
            }])
            .is_err());
        assert_eq!(text(&output), header);
    }
}
