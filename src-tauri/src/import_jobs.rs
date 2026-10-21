//! 冻结预检计划、同会话执行和不含 SQL 的有界恢复记录。
//! 恢复仅用于人工核对：任意 SQL 都不能按语句序号自动续跑。
use crate::commands::import::{rewrite_create_sql, table_exists_in_session};
use crate::database::dump::{parse_create_table, SqlSplitter};
use crate::database::pool::ConnectionPoolManager;
use crate::task_registry::{CleanupState, CommitState, TaskGuard, TaskPhase, TaskSnapshot};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use uuid::Uuid;

const PLAN_TTL_MS: i64 = 5 * 60 * 1000;
const MAX_PLANS: usize = 128;
const MAX_RECEIPTS: usize = 384;
const MAX_JOURNAL_BYTES: u64 = 64 * 1024;
const NOTICE: &str = "executed 仅表示执行成功，不代表已提交。沿用脚本事务；DDL 可能隐式提交，归还会话会清理未闭合事务。已发送 SQL 或结果未知时，只能人工核对，不能按序号续跑或盲目重试。";

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ImportPlanRequest {
    pub connection_id: String,
    pub database: Option<String>,
    pub path: String,
    pub on_conflict: String,
    pub task_id: String,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ImportExecuteRequest {
    pub plan_id: String,
    pub task_id: String,
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportPlan {
    pub plan_id: String,
    pub connection_id: String,
    pub database: Option<String>,
    pub path: String,
    pub on_conflict: String,
    pub connection_generation: String,
    pub file_bytes: u64,
    pub statement_count: u64,
    pub warnings: Vec<String>,
    pub expires_at: i64,
    pub task: TaskSnapshot,
}
#[derive(Clone)]
struct FrozenPlan {
    owner: usize,
    plan: ImportPlan,
    digest: Vec<u8>,
    claimed: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportRunSummary {
    pub statements: u64,
    pub executed: u64,
    pub skipped: u64,
    pub cleanup_notice: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportReport {
    pub run_id: String,
    pub plan_id: String,
    /// 序列化快照使跨版本 journal 读取不依赖运行中的注册表。
    pub task: serde_json::Value,
    pub summary: ImportRunSummary,
    pub failed_statement: Option<u64>,
    pub sent_statements: u64,
    pub commit_state: String,
    pub cleanup_state: String,
    pub error_code: Option<String>,
    pub message: Option<String>,
    pub durable: bool,
    pub recovery: String,
    pub recovered: bool,
}
#[derive(Default)]
struct Registry {
    plans: HashMap<String, FrozenPlan>,
    receipts: HashMap<(usize, String), ImportReport>,
}
fn registry() -> &'static Mutex<Registry> {
    static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(Registry::default()))
}
fn owner(manager: &ConnectionPoolManager) -> usize {
    Arc::as_ptr(&manager.tasks) as usize
}
fn now() -> i64 {
    chrono::Utc::now().timestamp_millis()
}
fn text_error(error: impl std::fmt::Display) -> String {
    error.to_string()
}
fn database_failure(error: crate::models::DatabaseError, code: &str, message: &str) -> String {
    if matches!(error, crate::models::DatabaseError::QueryCancelled) {
        "IMPORT_CANCELLED: 驱动已确认当前 SQL 中断，已发送部分请人工核对".into()
    } else {
        format!("{code}: {message}")
    }
}
fn cancellation_confirmed(error: &str) -> bool {
    error.starts_with("IMPORT_CANCELLED:") || error.starts_with("CANCELLED_BEFORE_EXECUTION:")
}
fn task_value(task: &TaskGuard) -> Result<serde_json::Value, String> {
    serde_json::to_value(task.snapshot().map_err(text_error)?).map_err(text_error)
}
fn open_source(path: &str) -> Result<File, String> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // 只共享读取：校验与执行期间拒绝写入和替换，且始终使用同一打开句柄。
        options.share_mode(1);
    }
    let file = options
        .open(path)
        .map_err(|_| "IMPORT_FILE_UNREADABLE: 无法打开源文件".to_string())?;
    if !file.metadata().map_err(text_error)?.is_file() {
        return Err("IMPORT_FILE_INVALID: 来源必须为普通文件".into());
    }
    Ok(file)
}

/// 只提取注释后的命令头，不将 SQL 内容放入错误或记录。
fn command_head(mut sql: &str) -> &str {
    loop {
        sql = sql.trim_start();
        if sql.starts_with("--") || sql.starts_with('#') {
            sql = sql.split_once('\n').map_or("", |(_, rest)| rest);
        } else if sql.starts_with("/*") && !sql.starts_with("/*!") {
            sql = sql.split_once("*/").map_or("", |(_, rest)| rest);
        } else {
            return sql;
        }
    }
}
fn unsupported_client_or_dollar(sql: &str, mysql: bool) -> bool {
    let bytes = sql.as_bytes();
    let (mut index, mut quote, mut block, mut line_start) = (0, None, false, true);
    while index < bytes.len() {
        let byte = bytes[index];
        if let Some(end) = quote {
            if mysql && byte == b'\\' {
                index = (index + 2).min(bytes.len());
                continue;
            }
            if byte == end {
                if bytes.get(index + 1) == Some(&end) {
                    index += 2;
                    continue;
                }
                quote = None;
            }
            index += 1;
            continue;
        }
        if block {
            if byte == b'\n' {
                line_start = true;
            }
            if byte == b'*' && bytes.get(index + 1) == Some(&b'/') {
                block = false;
                index += 2;
            } else {
                index += 1;
            }
            continue;
        }
        if byte == b'-' && bytes.get(index + 1) == Some(&b'-') || mysql && byte == b'#' {
            while index < bytes.len() && bytes[index] != b'\n' {
                index += 1;
            }
            continue;
        }
        if byte == b'/' && bytes.get(index + 1) == Some(&b'*') {
            block = true;
            index += 2;
            continue;
        }
        if byte == b'\n' {
            line_start = true;
            index += 1;
            continue;
        }
        if line_start && !byte.is_ascii_whitespace() {
            let end = bytes[index..]
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(bytes.len(), |offset| index + offset);
            let line = sql[index..end].trim();
            if byte == b'\\'
                || byte == b':'
                || line.eq_ignore_ascii_case("GO")
                || line
                    .to_ascii_uppercase()
                    .strip_prefix("GO ")
                    .is_some_and(|tail| tail.trim().bytes().all(|byte| byte.is_ascii_digit()))
            {
                return true;
            }
            line_start = false;
        }
        if matches!(byte, b'\'' | b'"' | b'`' | b'[') {
            quote = Some(if byte == b'[' { b']' } else { byte });
        }
        if !mysql
            && byte == b'$'
            && (index == 0
                || !bytes[index - 1].is_ascii_alphanumeric()
                    && !matches!(bytes[index - 1], b'_' | b'$'))
        {
            let mut end = index + 1;
            if bytes.get(end) == Some(&b'$') {
                return true;
            }
            if bytes
                .get(end)
                .is_some_and(|byte| byte.is_ascii_alphabetic() || *byte == b'_')
            {
                end += 1;
                while bytes
                    .get(end)
                    .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
                {
                    end += 1;
                }
                if bytes.get(end) == Some(&b'$') {
                    return true;
                }
            }
        }
        index += 1;
    }
    false
}
fn inspect_statement(sql: &str, mysql: bool) -> Result<(bool, bool), String> {
    let head = command_head(sql);
    let keyword = crate::database::first_keyword(sql);
    let first = keyword.as_str();
    if head.starts_with('\\')
        || head.starts_with('.')
        || matches!(
            first,
            "SOURCE" | "CONNECT" | "QUIT" | "EXIT" | "GO" | ":R" | ":CONNECT"
        )
        || first == "COPY"
        || unsupported_client_or_dollar(sql, mysql)
    {
        return Err("IMPORT_CLIENT_FORMAT_UNSUPPORTED: 文件包含不支持的客户端命令、COPY 指令或美元引号过程体，请改用兼容的逐条 SQL 转储".into());
    }
    Ok((
        matches!(first, "CREATE" | "ALTER" | "DROP" | "TRUNCATE" | "RENAME"),
        matches!(
            first,
            "BEGIN" | "START" | "COMMIT" | "ROLLBACK" | "SAVEPOINT" | "RELEASE"
        ),
    ))
}

async fn scan(
    file: &mut File,
    mysql: bool,
    task: &TaskGuard,
    bytes: u64,
) -> Result<(Vec<u8>, u64, bool, bool), String> {
    file.seek(SeekFrom::Start(0)).map_err(text_error)?;
    let mut digest = Sha256::new();
    // 跨 await 的数组会内联进入整个 IPC Future，在 Tauri release 多层传参时耗尽桌面栈。
    // 直接从堆分配固定容量，不用 Box::new([0; N]) 先在栈上构造大数组。
    let mut buffer = vec![0u8; 65536];
    let mut read = 0;
    loop {
        if task.cancel_requested() {
            return Err("IMPORT_CANCELLED: 已取消，未发送 SQL".into());
        }
        let length = file
            .read(&mut buffer)
            .map_err(|_| "IMPORT_FILE_UNREADABLE: 读取文件失败".to_string())?;
        if length == 0 {
            break;
        }
        digest.update(&buffer[..length]);
        read += length as u64;
        task.progress("validating", read.min(bytes), Some(bytes), 0)
            .map_err(text_error)?;
        tokio::task::yield_now().await;
    }
    if read != bytes {
        return Err("IMPORT_FILE_CHANGED: 文件大小已变化".into());
    }
    file.seek(SeekFrom::Start(0)).map_err(text_error)?;
    let mut reader = BufReader::with_capacity(65536, file);
    let mut splitter = SqlSplitter::new(mysql);
    let (mut count, mut ddl, mut transaction) = (0, false, false);
    while let Some(sql) = splitter
        .next_statement(&mut reader)
        .map_err(|_| "IMPORT_PARSE_ERROR: SQL 文件拆分失败或超出 16 MiB 单语句预算".to_string())?
    {
        if task.cancel_requested() {
            return Err("IMPORT_CANCELLED: 已取消，未发送 SQL".into());
        }
        let (d, t) = inspect_statement(&sql, mysql)?;
        ddl |= d;
        transaction |= t;
        count += 1;
        task.progress("parsing", 0, Some(bytes), count)
            .map_err(text_error)?;
        tokio::task::yield_now().await;
    }
    Ok((digest.finalize().to_vec(), count, ddl, transaction))
}

pub async fn preview(
    manager: &ConnectionPoolManager,
    request: ImportPlanRequest,
) -> Result<ImportPlan, String> {
    if !matches!(request.on_conflict.as_str(), "error" | "skip" | "overwrite") {
        return Err("IMPORT_CONFLICT_INVALID: 冲突策略无效".into());
    }
    let mut task = manager
        .tasks
        .start(
            Some(request.task_id.clone()),
            &request.connection_id,
            None,
            "importPlan",
        )
        .map_err(text_error)?;
    let result = async {
        let snapshot = manager.get_connection_snapshot(&request.connection_id).await.map_err(text_error)?;
        task.prepare(&snapshot.generation, snapshot.driver.session_capabilities()).map_err(text_error)?;
        let mut file = open_source(&request.path)?;
        let bytes = file.metadata().map_err(text_error)?.len();
        let (digest, count, ddl, transaction) = scan(&mut file, matches!(snapshot.driver.driver_type(), "mysql" | "mariadb"), &task, bytes).await?;
        let current = manager.active_connection_snapshot(&request.connection_id).await.map_err(text_error)?;
        if current.generation != snapshot.generation { return Err("CONNECTION_CHANGED: 请重新预检".into()); }
        let mut warnings = vec!["预检只检查全文件可读、拆分预算和明显不兼容格式；没有执行 SQL，未验证服务器语法、对象存在性或权限。".into(), "兼容边界：不支持 COPY 指令、psql/sqlcmd 等客户端命令和 PostgreSQL 美元引号过程体；MySQL DELIMITER 只按现有有界拆分器处理。".into(), NOTICE.into()];
        if ddl { warnings.push("包含 DDL：部分数据库会隐式提交，不能承诺整文件原子性。".into()); }
        if transaction { warnings.push("包含事务控制：沿用脚本的提交/回滚边界，不添加默认事务。".into()); }
        if !cfg!(windows) { warnings.push("当前平台不能保证读取期间文件不可被并发写入，请确保来源不被修改。".into()); }
        if manager.import_journal_dir().is_none() { warnings.push("执行记录仅保存在有界内存中，应用重启后不可恢复。".into()); }
        Ok::<_, String>((snapshot.generation, digest, bytes, count, warnings))
    }.await;
    let (generation, digest, bytes, count, warnings) = match result {
        Ok(value) => value,
        Err(error) => {
            let cancelled = cancellation_confirmed(&error);
            let _ = task.finish(
                if cancelled {
                    TaskPhase::Cancelled
                } else {
                    TaskPhase::Failed
                },
                CommitState::NotStarted,
                CleanupState::NotStarted,
                CleanupState::NotStarted,
                Some(if cancelled {
                    "IMPORT_CANCELLED"
                } else {
                    "IMPORT_PREVIEW_FAILED"
                }),
            );
            return Err(error);
        }
    };
    let task_snapshot = task
        .finish(
            TaskPhase::Succeeded,
            CommitState::NotStarted,
            CleanupState::NotStarted,
            CleanupState::NotStarted,
            None,
        )
        .map_err(text_error)?;
    let plan = ImportPlan {
        plan_id: Uuid::new_v4().to_string(),
        connection_id: request.connection_id,
        database: request.database,
        path: request.path,
        on_conflict: request.on_conflict,
        connection_generation: generation,
        file_bytes: bytes,
        statement_count: count,
        warnings,
        expires_at: now() + PLAN_TTL_MS,
        task: task_snapshot,
    };
    let mut registry = registry().lock().unwrap_or_else(|e| e.into_inner());
    registry
        .plans
        .retain(|_, entry| entry.plan.expires_at > now());
    if registry.plans.len() >= MAX_PLANS {
        return Err("IMPORT_PLAN_CAPACITY: 预检计划容量已满，请稍后重试".into());
    }
    registry.plans.insert(
        plan.plan_id.clone(),
        FrozenPlan {
            owner: owner(manager),
            plan: plan.clone(),
            digest,
            claimed: false,
        },
    );
    Ok(plan)
}

fn terminal(report: &ImportReport) -> bool {
    matches!(
        report.task["phase"].as_str(),
        Some("succeeded" | "failed" | "cancelled" | "unknown")
    )
}
fn save(manager: &ConnectionPoolManager, report: &ImportReport) -> Result<(), String> {
    static JOURNAL_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    let _journal_lock = JOURNAL_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(directory) = manager.import_journal_dir() {
        std::fs::create_dir_all(&directory)
            .map_err(|_| "IMPORT_JOURNAL_FAILED: 无法创建执行记录目录".to_string())?;
        // 总计最多 384 份，容量满时回收最早的终态；不删除活动记录。
        // 临时文件也计入容量，避免反复崩溃留下的临时文件无界增长。
        let mut files: Vec<_> = std::fs::read_dir(&directory)
            .map_err(text_error)?
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .path()
                    .extension()
                    .is_some_and(|extension| extension == "json" || extension == "tmp")
            })
            .collect();
        files.sort_by_key(|entry| entry.metadata().and_then(|meta| meta.modified()).ok());
        if files.len() >= MAX_RECEIPTS.saturating_sub(1)
            && !directory.join(format!("{}.tmp", report.run_id)).exists()
        {
            let mut removed = false;
            for entry in files {
                if entry.path() == directory.join(format!("{}.json", report.run_id)) {
                    continue;
                }
                if entry
                    .path()
                    .extension()
                    .is_some_and(|extension| extension == "tmp")
                    && entry
                        .metadata()
                        .and_then(|meta| meta.modified())
                        .ok()
                        .and_then(|time| time.elapsed().ok())
                        .is_some_and(|age| age.as_secs() > 86400)
                {
                    std::fs::remove_file(entry.path()).map_err(text_error)?;
                    removed = true;
                    break;
                }
                if let Ok(old) = read_journal(&entry.path()) {
                    if terminal(&old) {
                        std::fs::remove_file(entry.path()).map_err(text_error)?;
                        removed = true;
                        break;
                    }
                }
            }
            if !removed {
                return Err("IMPORT_JOURNAL_CAPACITY: 执行记录已满，未发送更多 SQL".into());
            }
        }
        let data = serde_json::to_vec(report).map_err(text_error)?;
        if data.len() as u64 > MAX_JOURNAL_BYTES {
            return Err("IMPORT_JOURNAL_LIMIT: 单份执行记录超过预算".into());
        }
        let temporary = directory.join(format!("{}.tmp", report.run_id));
        let result = (|| {
            let mut file = File::create(&temporary)?;
            file.write_all(&data)?;
            file.sync_all()?;
            std::fs::rename(
                &temporary,
                directory.join(format!("{}.json", report.run_id)),
            )?;
            #[cfg(unix)]
            File::open(&directory)?.sync_all()?;
            Ok::<_, std::io::Error>(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(temporary);
            return Err(
                "IMPORT_JOURNAL_FAILED: 执行记录写入失败，已停止后续 SQL；请人工核对".into(),
            );
        }
    }
    cache(manager, report)
}
fn cache(manager: &ConnectionPoolManager, report: &ImportReport) -> Result<(), String> {
    let mut registry = registry().lock().unwrap_or_else(|e| e.into_inner());
    let key = (owner(manager), report.run_id.clone());
    if registry.receipts.len() >= MAX_RECEIPTS && !registry.receipts.contains_key(&key) {
        let oldest = registry
            .receipts
            .iter()
            .filter(|(_, value)| terminal(value))
            .min_by_key(|(_, value)| value.task["completedAt"].as_i64().unwrap_or(0))
            .map(|(key, _)| key.clone());
        if let Some(key) = oldest {
            registry.receipts.remove(&key);
        } else {
            return Err("IMPORT_JOURNAL_CAPACITY: 内存执行记录已满".into());
        }
    }
    registry.receipts.insert(key, report.clone());
    Ok(())
}
fn read_journal(path: &Path) -> Result<ImportReport, String> {
    let file = File::open(path).map_err(text_error)?;
    if file.metadata().map_err(text_error)?.len() > MAX_JOURNAL_BYTES {
        return Err("IMPORT_JOURNAL_LIMIT".into());
    }
    let report: ImportReport = serde_json::from_reader(file.take(MAX_JOURNAL_BYTES + 1))
        .map_err(|_| "IMPORT_JOURNAL_INVALID: 执行记录无效".to_string())?;
    if Uuid::parse_str(&report.run_id)
        .ok()
        .map(|id| id.to_string())
        .as_deref()
        != Some(&report.run_id)
        || path.file_stem().and_then(|stem| stem.to_str()) != Some(&report.run_id)
    {
        return Err("IMPORT_JOURNAL_INVALID: 记录身份与文件名不一致".into());
    }
    Ok(report)
}
fn recovered_unknown(report: &mut ImportReport) {
    report.task["phase"] = "unknown".into();
    report.task["commitState"] = "unknown".into();
    report.task["cleanupState"] = "unknown".into();
    report.task["errorCode"] = "IMPORT_RECOVERY_UNKNOWN".into();
    report.commit_state = "unknown".into();
    report.cleanup_state = "unknown".into();
    report.error_code = Some("IMPORT_RECOVERY_UNKNOWN".into());
    report.message = Some(
        "执行任务已中断或最终记录缺失，提交与清理边界未知，请人工核对数据库，禁止按序号自动重放。"
            .into(),
    );
    report.recovery = "manualVerification".into();
    report.recovered = true;
}
fn resolve_receipt_id(
    manager: &ConnectionPoolManager,
    connection_id: &str,
    lookup: &str,
) -> Result<String, String> {
    let lookup = Uuid::parse_str(lookup)
        .map_err(|_| "IMPORT_RUN_INVALID".to_string())?
        .to_string();
    let mut candidates = HashSet::new();
    {
        let registry = registry().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(report) = registry.receipts.get(&(owner(manager), lookup.clone())) {
            if report.task["connectionId"].as_str() != Some(connection_id) {
                return Err("IMPORT_SOURCE_MISMATCH".into());
            }
            return Ok(lookup);
        }
        for ((entry_owner, run_id), report) in &registry.receipts {
            if *entry_owner == owner(manager)
                && report.task["taskId"].as_str() == Some(&lookup)
                && report.task["connectionId"].as_str() == Some(connection_id)
            {
                candidates.insert(run_id.clone());
            }
        }
    }
    if let Some(directory) = manager.import_journal_dir() {
        let exact = directory.join(format!("{lookup}.json"));
        if exact.exists() {
            let report = read_journal(&exact)?;
            if report.run_id != lookup
                || report.task["connectionId"].as_str() != Some(connection_id)
            {
                return Err("IMPORT_SOURCE_MISMATCH".into());
            }
            return Ok(lookup);
        }
        if directory.exists() {
            let entries = std::fs::read_dir(&directory)
                .map_err(|_| "IMPORT_JOURNAL_UNREADABLE".to_string())?;
            let mut count = 0;
            for entry in entries {
                let entry = entry.map_err(|_| "IMPORT_JOURNAL_UNREADABLE".to_string())?;
                count += 1;
                if count > MAX_RECEIPTS {
                    return Err("IMPORT_JOURNAL_CAPACITY: 记录集合超过读取预算，请人工核对".into());
                }
                if !entry
                    .path()
                    .extension()
                    .is_some_and(|extension| extension == "json")
                {
                    continue;
                }
                let report = read_journal(&entry.path())?;
                if report.task["taskId"].as_str() == Some(&lookup)
                    && report.task["connectionId"].as_str() == Some(connection_id)
                {
                    candidates.insert(report.run_id);
                }
            }
        }
    }
    if candidates.len() > 1 {
        return Err(
            "IMPORT_RECEIPT_AMBIGUOUS: 同一恢复编号匹配多份记录，请改用精确 runId 人工核对".into(),
        );
    }
    candidates
        .into_iter()
        .next()
        .ok_or_else(|| "IMPORT_RECEIPT_NOT_FOUND: 当前连接没有该编号的执行记录".into())
}
pub fn receipt(
    manager: &ConnectionPoolManager,
    connection_id: String,
    run_id: String,
) -> Result<ImportReport, String> {
    // 精确 runId 优先；执行尚未返回时，可用前端已知的 taskId 恢复编号查找。
    let id = resolve_receipt_id(manager, &connection_id, &run_id)?;
    let cached = {
        registry()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .receipts
            .get(&(owner(manager), id.clone()))
            .cloned()
    };
    if let Some(mut report) = cached {
        if report.task["connectionId"].as_str() != Some(&connection_id) {
            return Err("IMPORT_SOURCE_MISMATCH".into());
        }
        if !terminal(&report) {
            let current = report.task["taskId"]
                .as_str()
                .and_then(|task_id| manager.tasks.snapshot(task_id).ok());
            if current.as_ref().is_none_or(|task| task.phase.terminal()) {
                // finish 与内存缓存更新之间的短窗口，磁盘可能已经持有完整终态。
                if let Some(directory) = manager.import_journal_dir() {
                    if let Ok(finished) = read_journal(&directory.join(format!("{id}.json"))) {
                        if terminal(&finished)
                            && finished.run_id == id
                            && finished.task["connectionId"].as_str() == Some(&connection_id)
                        {
                            let _ = cache(manager, &finished);
                            return Ok(finished);
                        }
                    }
                }
                if let Some(current) = current {
                    report.task = serde_json::to_value(current).map_err(text_error)?;
                }
                recovered_unknown(&mut report);
                let _ = save(manager, &report);
                let _ = cache(manager, &report);
            }
        }
        return Ok(report);
    }
    let directory = manager
        .import_journal_dir()
        .ok_or("IMPORT_RECEIPT_NOT_FOUND: 内存执行记录已过期或应用已重启")?;
    let mut report = read_journal(&directory.join(format!("{id}.json")))?;
    if report.run_id != id || report.task["connectionId"].as_str() != Some(&connection_id) {
        return Err("IMPORT_SOURCE_MISMATCH".into());
    }
    report.recovered = true;
    // 重启后，未完成记录无法证明最后一次发送与服务器提交之间的结果。
    if !terminal(&report) {
        recovered_unknown(&mut report);
    }
    Ok(report)
}

pub async fn execute(
    manager: &ConnectionPoolManager,
    request: ImportExecuteRequest,
) -> Result<ImportReport, String> {
    let frozen = {
        let mut registry = registry().lock().unwrap_or_else(|e| e.into_inner());
        let entry = registry
            .plans
            .get_mut(&request.plan_id)
            .ok_or("IMPORT_PLAN_EXPIRED: 请重新预检")?;
        if entry.owner != owner(manager) || entry.plan.expires_at <= now() {
            return Err("IMPORT_PLAN_EXPIRED: 请重新预检".into());
        }
        if entry.claimed {
            return Err("IMPORT_PLAN_USED: 该计划正在执行或已有 SQL 发送，不能重复执行".into());
        }
        entry.claimed = true;
        entry.clone()
    };
    let plan = &frozen.plan;
    let mut task =
        match manager
            .tasks
            .start(Some(request.task_id), &plan.connection_id, None, "import")
        {
            Ok(task) => task,
            Err(error) => {
                if let Some(entry) = registry()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .plans
                    .get_mut(&plan.plan_id)
                {
                    entry.claimed = false;
                }
                return Err(error.to_string());
            }
        };
    let mut report = ImportReport {
        run_id: Uuid::new_v4().to_string(),
        plan_id: plan.plan_id.clone(),
        task: task_value(&task)?,
        summary: ImportRunSummary {
            statements: 0,
            executed: 0,
            skipped: 0,
            cleanup_notice: NOTICE.into(),
        },
        failed_statement: None,
        sent_statements: 0,
        commit_state: "notStarted".into(),
        cleanup_state: "notStarted".into(),
        error_code: None,
        message: None,
        durable: manager.import_journal_dir().is_some(),
        recovery: "safeToRetry".into(),
        recovered: false,
    };
    let mut lease = None;
    let outcome = async {
        let snapshot = manager.active_connection_snapshot(&plan.connection_id).await.map_err(text_error)?;
        if snapshot.generation != plan.connection_generation { return Err("CONNECTION_CHANGED: 连接代次已变化，请重新预检".into()); }
        task.prepare(&snapshot.generation, snapshot.driver.session_capabilities()).map_err(text_error)?;
        report.task = task_value(&task)?; save(manager, &report)?;
        let mut file = open_source(&plan.path)?;
        let bytes = file.metadata().map_err(text_error)?.len();
        if bytes != plan.file_bytes { return Err("IMPORT_FILE_CHANGED: 预检后文件已更改，请重新预检".into()); }
        let (digest, count, _, _) = scan(&mut file, matches!(snapshot.driver.driver_type(), "mysql" | "mariadb"), &task, bytes).await?;
        if digest != frozen.digest || count != plan.statement_count { return Err("IMPORT_FILE_CHANGED: 预检后文件已更改，请重新预检".into()); }
        if task.cancel_requested() { return Err("IMPORT_CANCELLED: 已取消，未发送 SQL".into()); }
        let session = tokio::select! {
            biased;
            _ = task.cancelled() => return Err("IMPORT_CANCELLED: 排队准备已取消，未发送 SQL".into()),
            result = snapshot.driver.lease_session(plan.database.as_deref()) => result.map_err(|error|database_failure(error,"IMPORT_PREPARE_FAILED","获取导入会话失败"))?,
        };
        lease = Some(session);
        task.attach(&**lease.as_ref().unwrap()).map_err(text_error)?;
        if task.cancel_requested() { return Err("IMPORT_CANCELLED: 准备完成后已取消，未发送 SQL".into()); }
        let current = manager.active_connection_snapshot(&plan.connection_id).await.map_err(text_error)?;
        if current.generation != plan.connection_generation { return Err("CONNECTION_CHANGED: 请重新预检".into()); }
        task.running().map_err(text_error)?;
        file.seek(SeekFrom::Start(0)).map_err(text_error)?;
        let mut reader = BufReader::with_capacity(65536, file);
        let mut splitter = SqlSplitter::new(matches!(snapshot.driver.driver_type(), "mysql" | "mariadb"));
        let mut processed = 0u64;
        while let Some(mut sql) = splitter.next_statement(&mut reader).map_err(|_| "IMPORT_PARSE_ERROR: 文件拆分失败".to_string())? {
            if task.cancel_requested() { return Err("IMPORT_CANCELLED: 已停止后续语句，已发送部分请人工核对".into()); }
            let current = manager.active_connection_snapshot(&plan.connection_id).await.map_err(text_error)?;
            if current.generation != plan.connection_generation { return Err("CONNECTION_CHANGED: 已停止后续语句".into()); }
            report.summary.statements += 1;
            report.failed_statement = Some(report.summary.statements);
            processed = processed.saturating_add(sql.len() as u64).min(bytes);
            task.progress("executing", processed, Some(bytes), report.summary.executed + report.summary.skipped).map_err(text_error)?;
            let session = lease.as_mut().unwrap();
            if let Some((name_token, insert_pos, has_ine)) = parse_create_table(&sql) {
                if plan.on_conflict == "skip" {
                    mark_sending(manager, &task, &mut report)?;
                    if table_exists_in_session(&mut **session, snapshot.driver.driver_type(), &name_token).await.map_err(|error| database_failure(error,"IMPORT_CATALOG_FAILED","目录检查失败，请核对已发送边界"))? {
                        report.summary.skipped += 1; report.task = task_value(&task)?; save(manager, &report)?; continue;
                    }
                }
                sql = rewrite_create_sql(sql,insert_pos,has_ine,&plan.on_conflict,snapshot.driver.driver_type()).map_err(str::to_string)?;
                if plan.on_conflict == "overwrite" {
                    if task.cancel_requested() {return Err("IMPORT_CANCELLED: 已停止后续语句".into());}
                    mark_sending(manager, &task, &mut report)?;
                    session.execute(&format!("DROP TABLE IF EXISTS {name_token}")).await.map_err(|error| database_failure(error,"IMPORT_DROP_FAILED","删除目标表失败，请核对数据库"))?;
                    report.task = task_value(&task)?; save(manager, &report)?;
                }
            }
            if task.cancel_requested() { return Err("IMPORT_CANCELLED: 已停止后续语句".into()); }
            mark_sending(manager, &task, &mut report)?;
            session.execute(&sql).await.map_err(|error| database_failure(error,"IMPORT_STATEMENT_FAILED","SQL 执行失败；详细 SQL 不写入执行记录，请按失败序号人工核对"))?;
            report.summary.executed += 1;
            report.task = task_value(&task)?; save(manager, &report)?;
            tokio::task::yield_now().await;
        }
        report.failed_statement = None;
        task.progress("executing", bytes, Some(bytes), report.summary.executed + report.summary.skipped).map_err(text_error)?;
        Ok::<_, String>(())
    }.await;
    let _ = task.progress(
        "cleanup",
        0,
        Some(plan.file_bytes),
        report.summary.executed + report.summary.skipped,
    );
    report.task = task_value(&task)?;
    let before_cleanup = save(manager, &report);
    let cleanup_ok = if let Some(session) = lease {
        session.release().await.is_ok()
    } else {
        true
    };
    let outcome = outcome.and(before_cleanup);
    let (mut phase, error_code, message) =
        terminal_result(outcome, cleanup_ok, report.sent_statements);
    let commit = if report.sent_statements == 0 {
        CommitState::NotStarted
    } else {
        CommitState::Unknown
    };
    let cleanup = if cleanup_ok {
        CleanupState::Succeeded
    } else {
        CleanupState::Unknown
    };
    // 不能从 execute 成功推断脚本提交状态，也不能把清理成功等同整份导入回滚。
    let mut completed = task.snapshot().map_err(text_error)?;
    completed.phase = phase;
    completed.commit_state = commit;
    completed.cleanup_state = cleanup;
    completed.error_code = error_code.clone();
    completed.completed_at = Some(now());
    completed.sequence += 1;
    report.task = serde_json::to_value(completed).map_err(text_error)?;
    report.commit_state = if report.sent_statements == 0 {
        "notStarted"
    } else {
        "unknown"
    }
    .into();
    report.cleanup_state = if cleanup_ok { "succeeded" } else { "unknown" }.into();
    report.error_code = error_code;
    report.message = message;
    if report.sent_statements == 0 {
        if let Some(entry) = registry()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .plans
            .get_mut(&plan.plan_id)
        {
            entry.claimed = false;
        }
        // 内容变化/过期/代次变化的计划不能安全重试。
        if matches!(
            report.error_code.as_deref(),
            Some("IMPORT_FILE_CHANGED" | "CONNECTION_CHANGED" | "IMPORT_PLAN_EXPIRED")
        ) {
            report.recovery = "manualVerification".into();
        }
    }
    if let Err(error) = save(manager, &report) {
        phase = TaskPhase::Unknown;
        report.task["phase"] = "unknown".into();
        report.task["errorCode"] = "IMPORT_JOURNAL_FAILED".into();
        report.error_code = Some("IMPORT_JOURNAL_FAILED".into());
        report.message = Some(error);
        report.recovery = "manualVerification".into();
        // 无法落盘时保留返回边界；旧磁盘记录仍是执行前的保守 unknown 恢复点。
    }
    report.task = serde_json::to_value(
        task.finish(
            phase,
            commit,
            CleanupState::NotStarted,
            cleanup,
            report.error_code.as_deref(),
        )
        .map_err(text_error)?,
    )
    .map_err(text_error)?;
    // 落盘失败也保留本进程可查询的最新边界，不能返回旧的成功快照。
    let _ = cache(manager, &report);
    Ok(report)
}
fn mark_sending(
    manager: &ConnectionPoolManager,
    task: &TaskGuard,
    report: &mut ImportReport,
) -> Result<(), String> {
    // 先写“可能发送”，再调用驱动：崩溃后宁可保守未知，也不错误地允许重放。
    task.script_sql_sent().map_err(text_error)?;
    report.sent_statements += 1;
    report.recovery = "manualVerification".into();
    report.commit_state = "unknown".into();
    report.task = task_value(task)?;
    save(manager, report)
}

#[cfg(test)]
mod tests {
    #[test]
    fn import_ipc_futures_have_bounded_inline_storage() {
        use super::*;
        use std::future::Future;
        // 只推导返回类型，不先在测试线程构造可能超过桌面栈容量的 Future。
        fn size2<A, B, F, Fut>(_: F) -> usize
        where
            F: FnOnce(A, B) -> Fut,
            Fut: Future,
        {
            std::mem::size_of::<Fut>()
        }
        fn size4<A, B, C, D, F, Fut>(_: F) -> usize
        where
            F: FnOnce(A, B, C, D) -> Fut,
            Fut: Future,
        {
            std::mem::size_of::<Fut>()
        }
        let scan_bytes = size4::<&'static mut File, bool, &'static TaskGuard, u64, _, _>(scan);
        let preview_bytes =
            size2::<&'static ConnectionPoolManager, ImportPlanRequest, _, _>(preview);
        let execute_bytes =
            size2::<&'static ConnectionPoolManager, ImportExecuteRequest, _, _>(execute);
        let snapshot_bytes = size2::<&'static ConnectionPoolManager, &'static str, _, _>(
            ConnectionPoolManager::get_connection_snapshot,
        );
        let command_preview =
            size2::<ImportPlanRequest, tauri::State<'static, ConnectionPoolManager>, _, _>(
                crate::commands::import::sql_import_preview,
            );
        let command_execute =
            size2::<ImportExecuteRequest, tauri::State<'static, ConnectionPoolManager>, _, _>(
                crate::commands::import::sql_import_execute,
            );
        println!("导入异步状态内联字节：scan={scan_bytes}, preview={preview_bytes}, execute={execute_bytes}, snapshot={snapshot_bytes}, commandPreview={command_preview}, commandExecute={command_execute}");
        // Windows EXE 主线程预留栈仅 1 MiB；为 Tauri 多层传参及其它栈帧留足空间。
        for (name, bytes) in [
            ("scan", scan_bytes),
            ("preview", preview_bytes),
            ("execute", execute_bytes),
            ("commandPreview", command_preview),
            ("commandExecute", command_execute),
        ] {
            assert!(bytes < 16 * 1024, "{name} 的 Future 内联状态过大：{bytes}");
        }
    }
    use super::*;
    #[test]
    fn format_check_distinguishes_literals_comments_and_client_protocol() {
        for sql in [
            "INSERT INTO x VALUES('$$', '$body$', 'COPY x FROM STDIN')",
            "SELECT '$tag$'",
            "/* $$ */ SELECT 1",
            "SELECT '多行\nGO\n\\copy'",
            "SELECT foo$tag$bar",
        ] {
            assert!(inspect_statement(sql, false).is_ok(), "{sql}");
        }
        for sql in [
            "COPY x FROM\nSTDIN",
            "CREATE FUNCTION f() RETURNS int AS $tag$ BEGIN RETURN 1",
            "SELECT 1\nGO",
            "-- 注释\n\\connect other",
            "SELECT 1\n:r secret.sql",
        ] {
            assert!(inspect_statement(sql, false).is_err(), "{sql}");
        }
        assert_eq!(
            inspect_statement("/* 注释 */ CREATE/* 保留 */TABLE x(id)", false).unwrap(),
            (true, false)
        );
    }
    #[tokio::test]
    async fn expired_plan_is_rejected_without_starting_an_execution_task() {
        let manager = ConnectionPoolManager::new(None);
        let task = manager
            .tasks
            .start(
                Some(Uuid::new_v4().to_string()),
                "source",
                None,
                "importPlan",
            )
            .unwrap();
        let plan_id = Uuid::new_v4().to_string();
        let plan = ImportPlan {
            plan_id: plan_id.clone(),
            connection_id: "source".into(),
            database: None,
            path: "unread.sql".into(),
            on_conflict: "error".into(),
            connection_generation: "old".into(),
            file_bytes: 0,
            statement_count: 0,
            warnings: vec![],
            expires_at: now() - 1,
            task: task.snapshot().unwrap(),
        };
        registry()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .plans
            .insert(
                plan_id.clone(),
                FrozenPlan {
                    owner: owner(&manager),
                    plan,
                    digest: vec![],
                    claimed: false,
                },
            );
        let id = Uuid::new_v4().to_string();
        let error = execute(
            &manager,
            ImportExecuteRequest {
                plan_id: plan_id.clone(),
                task_id: id.clone(),
            },
        )
        .await
        .unwrap_err();
        assert!(error.contains("IMPORT_PLAN_EXPIRED"));
        assert!(manager.tasks.snapshot(&id).is_err());
        registry()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .plans
            .remove(&plan_id);
    }
    #[tokio::test]
    async fn late_cancel_cannot_relabel_a_known_sql_failure_or_success() {
        let manager = ConnectionPoolManager::new(None);
        for (outcome, expected) in [
            (
                Err("IMPORT_STATEMENT_FAILED: 已确认SQL失败".into()),
                TaskPhase::Failed,
            ),
            (Ok(()), TaskPhase::Succeeded),
            (
                Err("IMPORT_CANCELLED: 驱动确认中断".into()),
                TaskPhase::Cancelled,
            ),
        ] {
            let mut task = manager
                .tasks
                .start(Some(Uuid::new_v4().to_string()), "source", None, "import")
                .unwrap();
            task.prepare(
                "generation",
                crate::session::SessionCapabilities::supported(),
            )
            .unwrap();
            task.running().unwrap();
            task.script_sql_sent().unwrap();
            // 已知执行结果形成后，再注入取消，模拟清理/落盘边界；不依赖sleep时序。
            manager.tasks.request_cancel(task.task_id()).await.unwrap();
            assert!(task.cancel_requested());
            let (phase, code, _) = terminal_result(outcome, true, 1);
            assert_eq!(phase, expected);
            let completed = task
                .finish(
                    phase,
                    CommitState::Unknown,
                    CleanupState::NotStarted,
                    CleanupState::Succeeded,
                    code.as_deref(),
                )
                .unwrap();
            assert_eq!(completed.phase, expected);
            assert_eq!(completed.commit_state, CommitState::Unknown);
        }
        let error = database_failure(
            crate::models::DatabaseError::QueryCancelled,
            "IMPORT_STATEMENT_FAILED",
            "不可泄露详情",
        );
        assert!(cancellation_confirmed(&error));
        let error = database_failure(
            crate::models::DatabaseError::QueryError("敏感SQL".into()),
            "IMPORT_STATEMENT_FAILED",
            "执行失败",
        );
        assert!(!cancellation_confirmed(&error));
        assert!(!error.contains("敏感SQL"));
        let (phase, code, _) = terminal_result(
            Err("CANCELLED_BEFORE_EXECUTION: 已请求取消，不发送下一条SQL".into()),
            true,
            0,
        );
        assert_eq!(phase, TaskPhase::Cancelled);
        assert_eq!(code.as_deref(), Some("IMPORT_CANCELLED"));
        assert_eq!(manager.tasks.active_count(), 0);
    }
}

fn terminal_result(
    outcome: Result<(), String>,
    cleanup_ok: bool,
    sent_statements: u64,
) -> (TaskPhase, Option<String>, Option<String>) {
    if !cleanup_ok {
        (
            TaskPhase::Unknown,
            Some("IMPORT_CLEANUP_UNKNOWN".to_string()),
            Some("会话清理无法确认，请人工核对数据库。".to_string()),
        )
    } else if let Err(error) = outcome {
        let code = if error.starts_with("IMPORT_JOURNAL") {
            "IMPORT_JOURNAL_FAILED"
        } else if cancellation_confirmed(&error) {
            "IMPORT_CANCELLED"
        } else {
            error.split(':').next().unwrap_or("IMPORT_FAILED")
        };
        (
            if code == "IMPORT_JOURNAL_FAILED" && sent_statements > 0 {
                TaskPhase::Unknown
            } else if cancellation_confirmed(&error) {
                TaskPhase::Cancelled
            } else {
                TaskPhase::Failed
            },
            Some(code.to_string()),
            Some(error.clone()),
        )
    } else {
        (TaskPhase::Succeeded, None, None)
    }
}
