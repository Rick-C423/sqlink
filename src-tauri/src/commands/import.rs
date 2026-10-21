//! SQL 转储导入：流式拆分 .sql 文件并逐条执行
//!
//! 冲突处理（on_conflict）：
//! - "error"（默认）：原样执行，遇到已存在的表由服务器报错
//! - "skip"：CREATE TABLE 的目标表已存在时跳过该语句（数据语句仍会照常执行）
//! - "overwrite"：每条 CREATE TABLE 前先 DROP TABLE IF EXISTS（数据被清掉重建）

use std::fs::File;
use std::io::{BufRead, BufReader};

use tauri::State;

use crate::database::dump::{
    decode_identifier_parts, parse_create_table, SqlSplitter, MAX_IMPORT_STATEMENT_BYTES,
};
use crate::database::pool::ConnectionPoolManager;
use crate::models::{DatabaseError, DumpSummary, ImportSummary};
use crate::session::SessionLease;
use crate::typed_value::TypedValue;

const IMPORT_CLEANUP_NOTICE: &str =
    "executed 仅表示语句执行成功条数，不代表全部已提交；归还会话会回滚未闭合事务，请核对提交结果";

/// 只给确实支持该语法的方言补充 IF NOT EXISTS，且在 overwrite 的 DROP 前检查预算。
pub(crate) fn rewrite_create_sql(
    sql: String,
    insert_pos: usize,
    has_ine: bool,
    conflict: &str,
    driver: &str,
) -> Result<String, &'static str> {
    if sql.len() > MAX_IMPORT_STATEMENT_BYTES {
        return Err("IMPORT_STATEMENT_LIMIT: SQL 超过导入字节预算");
    }
    if matches!(driver, "sqlite" | "postgres" | "mysql" | "mariadb")
        && conflict != "error"
        && !has_ine
    {
        if sql.len() > MAX_IMPORT_STATEMENT_BYTES.saturating_sub(" IF NOT EXISTS".len()) {
            return Err("IMPORT_STATEMENT_LIMIT: 补充 IF NOT EXISTS 后的 SQL 超过导入字节预算");
        }
        return Ok(format!(
            "{} IF NOT EXISTS{}",
            &sql[..insert_pos],
            &sql[insert_pos..]
        ));
    }
    Ok(sql)
}

fn import_error(
    detail: impl std::fmt::Display,
    statement: u64,
    executed: u64,
    skipped: u64,
) -> String {
    format!("第 {statement} 条语句处理失败（已执行成功 {executed} 条，已跳过 {skipped} 条）：{detail}；此前成功语句可能已提交，停止不代表全文件回滚，请核查数据库，不要盲目重试")
}

#[tauri::command]
pub async fn sql_import_file(
    connection_id: String,
    database: Option<String>,
    path: String,
    on_conflict: String,
    manager: State<'_, ConnectionPoolManager>,
) -> Result<ImportSummary, String> {
    // 兼容旧 IPC 的参数与返回值，同时避免绕过新导入生命周期。
    let plan = crate::import_jobs::preview(
        &manager,
        crate::import_jobs::ImportPlanRequest {
            connection_id,
            database,
            path,
            on_conflict,
            task_id: uuid::Uuid::new_v4().to_string(),
        },
    )
    .await?;
    let report = crate::import_jobs::execute(
        &manager,
        crate::import_jobs::ImportExecuteRequest {
            plan_id: plan.plan_id,
            task_id: uuid::Uuid::new_v4().to_string(),
        },
    )
    .await?;
    if let Some(message) = report.message {
        return Err(import_error(
            message,
            report
                .failed_statement
                .unwrap_or(report.summary.statements + 1),
            report.summary.executed,
            report.summary.skipped,
        ));
    }
    Ok(ImportSummary {
        statements: report.summary.statements,
        executed: report.summary.executed,
        skipped: report.summary.skipped,
        cleanup_notice: report.summary.cleanup_notice,
    })
}

#[tauri::command]
pub async fn sql_import_preview(
    request: crate::import_jobs::ImportPlanRequest,
    manager: State<'_, ConnectionPoolManager>,
) -> Result<crate::import_jobs::ImportPlan, String> {
    crate::import_jobs::preview(&manager, request).await
}

#[tauri::command]
pub async fn sql_import_execute(
    request: crate::import_jobs::ImportExecuteRequest,
    manager: State<'_, ConnectionPoolManager>,
) -> Result<crate::import_jobs::ImportReport, String> {
    crate::import_jobs::execute(&manager, request).await
}

#[tauri::command]
pub fn sql_import_receipt(
    connection_id: String,
    run_id: String,
    manager: State<'_, ConnectionPoolManager>,
) -> Result<crate::import_jobs::ImportReport, String> {
    crate::import_jobs::receipt(&manager, connection_id, run_id)
}

/// 在导入已经占用的同一物理会话检查 CREATE 目标；不通过连接池再借元数据租约。
pub(crate) async fn table_exists_in_session(
    lease: &mut dyn SessionLease,
    driver: &str,
    name_token: &str,
) -> Result<bool, DatabaseError> {
    let invalid = || {
        DatabaseError::TypeError(
            "IMPORT_TARGET_UNSUPPORTED: CREATE TABLE 目标无法按当前方言完整解析".into(),
        )
    };
    let (sql, params) = match driver {
        "postgres" => (
            "SELECT CASE WHEN EXISTS (SELECT 1 FROM pg_catalog.pg_class WHERE oid = pg_catalog.to_regclass($1) AND relkind IN ('r', 'p', 'f')) THEN 1 ELSE 0 END::bigint".into(),
            vec![TypedValue::Text(name_token.into())],
        ),
        "mssql" => (
            "SELECT CAST(CASE WHEN OBJECT_ID(@P1, 'U') IS NOT NULL THEN 1 ELSE 0 END AS BIGINT)".into(),
            vec![TypedValue::Text(name_token.into())],
        ),
        "sqlite" => {
            let parts = decode_identifier_parts(name_token).ok_or_else(invalid)?;
            let (schema, table) = match parts.as_slice() {
                [table] => ("main", table.as_str()),
                [schema, table] => (schema.as_str(), table.as_str()),
                _ => return Err(invalid()),
            };
            (format!("SELECT CASE WHEN EXISTS (SELECT 1 FROM \"{}\".sqlite_schema WHERE type = 'table' AND name = ?1 COLLATE NOCASE) THEN 1 ELSE 0 END", schema.replace('"', "\"\"")),
             vec![TypedValue::Text(table.into())])
        }
        "mysql" | "mariadb" => {
            let parts = decode_identifier_parts(name_token).ok_or_else(invalid)?;
            let (database, table) = match parts.as_slice() {
                [table] => (TypedValue::Null, table.clone()),
                [database, table] => (TypedValue::Text(database.clone()), table.clone()),
                _ => return Err(invalid()),
            };
            // 大小写规则交给当前实例；mode=0 精确匹配，mode=1/2 按服务端折叠名称比较。
            ("SELECT CAST(EXISTS (SELECT 1 FROM information_schema.TABLES WHERE TABLE_TYPE = 'BASE TABLE' AND ((@@lower_case_table_names = 0 AND BINARY TABLE_SCHEMA = BINARY COALESCE(?, DATABASE()) AND BINARY TABLE_NAME = BINARY ?) OR (@@lower_case_table_names <> 0 AND BINARY LOWER(TABLE_SCHEMA) = BINARY LOWER(COALESCE(?, DATABASE())) AND BINARY LOWER(TABLE_NAME) = BINARY LOWER(?)))) AS SIGNED)".into(),
             vec![database.clone(), TypedValue::Text(table.clone()), database, TypedValue::Text(table)])
        }
        _ => return Err(invalid()),
    };
    let result = lease.query_typed(&sql, &params).await?;
    if result.result.truncated || result.result.rows.len() != 1 || result.typed_rows.len() != 1 {
        return Err(DatabaseError::QueryError(
            "IMPORT_CATALOG_INCOMPLETE: CREATE 目标存在性检查未完整返回单行".into(),
        ));
    }
    match result.typed_rows[0].as_slice() {
        [Some(TypedValue::Integer(value))] if value == "0" => Ok(false),
        [Some(TypedValue::Integer(value))] if value == "1" => Ok(true),
        _ => Err(DatabaseError::TypeError(
            "IMPORT_CATALOG_INVALID: CREATE 目标存在性检查返回了无效类型".into(),
        )),
    }
}

/// 与 IPC 共用的导入入口，便于真实临时库验证会话连续性和错误清理。
pub async fn import_sql_file(
    manager: &ConnectionPoolManager,
    connection_id: String,
    database: Option<String>,
    path: String,
    on_conflict: String,
) -> Result<ImportSummary, String> {
    // 参数、文件可读性均在连接获取、目录访问和租约内 SQL 之前验证。
    if !matches!(on_conflict.as_str(), "error" | "skip" | "overwrite") {
        return Err(import_error(
            "IMPORT_CONFLICT_INVALID: on_conflict 只接受 error、skip 或 overwrite",
            1,
            0,
            0,
        ));
    }
    let file =
        File::open(&path).map_err(|e| import_error(format!("打开文件失败: {e}"), 1, 0, 0))?;
    if !file
        .metadata()
        .map_err(|e| import_error(format!("读取文件属性失败: {e}"), 1, 0, 0))?
        .is_file()
    {
        return Err(import_error("导入路径必须是可读普通文件", 1, 0, 0));
    }
    let mut reader = BufReader::with_capacity(64 * 1024, file);
    reader
        .fill_buf()
        .map_err(|e| import_error(format!("读取文件失败: {e}"), 1, 0, 0))?;

    let driver = manager
        .get_connection(&connection_id)
        .await
        .map_err(|e| import_error(e, 1, 0, 0))?;

    let mysql_mode = matches!(driver.driver_type(), "mysql" | "mariadb");

    let mut splitter = SqlSplitter::new(mysql_mode);

    let mut summary = ImportSummary {
        statements: 0,
        executed: 0,
        skipped: 0,
        cleanup_notice: IMPORT_CLEANUP_NOTICE.into(),
    };
    // 持有同一租约，目录检查也在这里执行，保留文件内 USE/search_path/临时对象的真实语义。
    // 语句执行成功不等于事务提交；归还清理会回滚未闭合事务，不能许诺全文件原子性。
    let mut lease = driver
        .lease_session(database.as_deref())
        .await
        .map_err(|e| import_error(e, 1, 0, 0))?;
    let outcome = async {
        loop {
            let stmt = splitter.next_statement(&mut reader).map_err(|e| {
                import_error(
                    format!("读取或拆分文件失败: {e}"),
                    summary.statements + 1,
                    summary.executed,
                    summary.skipped,
                )
            })?;
            let Some(mut sql) = stmt else { break };
            summary.statements += 1;

            if let Some((name_token, insert_pos, has_ine)) = parse_create_table(&sql) {
                if on_conflict == "skip"
                    && table_exists_in_session(&mut *lease, driver.driver_type(), &name_token)
                        .await
                        .map_err(|error| {
                            import_error(
                                error,
                                summary.statements,
                                summary.executed,
                                summary.skipped,
                            )
                        })?
                {
                    summary.skipped += 1;
                    continue;
                }
                sql = rewrite_create_sql(
                    sql,
                    insert_pos,
                    has_ine,
                    &on_conflict,
                    driver.driver_type(),
                )
                .map_err(|message| {
                    import_error(
                        message,
                        summary.statements,
                        summary.executed,
                        summary.skipped,
                    )
                })?;
                // 改写后的语句也先通过预算检查，再允许 overwrite 删除原表。
                if on_conflict == "overwrite" {
                    lease
                        .execute(&format!("DROP TABLE IF EXISTS {}", name_token))
                        .await
                        .map_err(|e| {
                            import_error(
                                format!("删除已存在表失败: {e}"),
                                summary.statements,
                                summary.executed,
                                summary.skipped,
                            )
                        })?;
                }
            }

            lease.execute(&sql).await.map_err(|e| {
                import_error(e, summary.statements, summary.executed, summary.skipped)
            })?;
            summary.executed += 1;
        }
        Ok::<_, String>(())
    }
    .await;
    let cleanup = lease.release().await;
    match (outcome, cleanup) {
        (Ok(()), Ok(())) => Ok(summary),
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(cleanup)) => Err(format!(
            "{error}；SESSION_RESULT_UNKNOWN: 会话清理未确认: {cleanup}"
        )),
        (Ok(()), Err(cleanup)) => Err(import_error(
            format!("SESSION_RESULT_UNKNOWN: 导入语句已执行，但会话清理无法确认: {cleanup}"),
            summary.statements + 1,
            summary.executed,
            summary.skipped,
        )),
    }
}

/// 转储 SQL 文件：采用同目录临时文件和流式原值编码。
#[tauri::command]
pub async fn sql_dump_file(
    connection_id: String,
    database: Option<String>,
    tables: Option<Vec<String>>,
    path: String,
    include_drop: bool,
    include_data: bool,
    task_id: Option<String>,
    table_sources: Option<Vec<crate::metadata::ObjectIdentity>>,
    manager: State<'_, ConnectionPoolManager>,
) -> Result<DumpSummary, String> {
    crate::export::export_job_sources(
        &manager,
        connection_id,
        database,
        tables,
        table_sources,
        path,
        true,
        include_drop,
        include_data,
        task_id,
    )
    .await
}

#[cfg(test)]
mod import_rewrite_tests {
    use super::*;
    #[test]
    fn create_rewrite_respects_dialect_and_original_budget() {
        let sql = "CREATE TABLE dbo.target(id INT)".to_string();
        let (_, insert, has_ine) = parse_create_table(&sql).unwrap();
        for conflict in ["skip", "overwrite"] {
            assert_eq!(
                rewrite_create_sql(sql.clone(), insert, has_ine, conflict, "mssql").unwrap(),
                sql
            );
            for driver in ["sqlite", "postgres", "mysql", "mariadb"] {
                assert!(
                    rewrite_create_sql(sql.clone(), insert, has_ine, conflict, driver)
                        .unwrap()
                        .contains("TABLE IF NOT EXISTS")
                );
            }
        }
        assert_eq!(
            rewrite_create_sql(sql.clone(), insert, false, "error", "sqlite").unwrap(),
            sql
        );
        let mut near_limit = sql.clone();
        near_limit.push_str(&" ".repeat(MAX_IMPORT_STATEMENT_BYTES - near_limit.len()));
        assert!(
            rewrite_create_sql(near_limit.clone(), insert, false, "overwrite", "sqlite")
                .unwrap_err()
                .contains("IMPORT_STATEMENT_LIMIT")
        );
        assert_eq!(
            rewrite_create_sql(near_limit, insert, false, "overwrite", "mssql")
                .unwrap()
                .len(),
            MAX_IMPORT_STATEMENT_BYTES
        );
        let existing = "CREATE TABLE IF NOT EXISTS target(id INT)".to_string();
        let (_, insert, has_ine) = parse_create_table(&existing).unwrap();
        assert_eq!(
            rewrite_create_sql(existing.clone(), insert, has_ine, "overwrite", "postgres").unwrap(),
            existing
        );
    }
}
