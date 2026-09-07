//! SQL 转储导入：流式拆分 .sql 文件并逐条执行
//!
//! 冲突处理（on_conflict）：
//! - "error"（默认）：原样执行，遇到已存在的表由服务器报错
//! - "skip"：CREATE TABLE 的目标表已存在时跳过该语句（数据语句仍会照常执行）
//! - "overwrite"：每条 CREATE TABLE 前先 DROP TABLE IF EXISTS（数据被清掉重建）

use std::collections::HashSet;
use std::fs::File;
use std::io::{BufReader, BufWriter, Write};

use tauri::State;

use crate::database::dump::{normalize_table_name, parse_create_table, SqlSplitter};
use crate::database::pool::ConnectionPoolManager;
use crate::models::{DumpSummary, ImportSummary};

#[tauri::command]
pub async fn sql_import_file(
    connection_id: String,
    database: Option<String>,
    path: String,
    on_conflict: String,
    manager: State<'_, ConnectionPoolManager>,
) -> Result<ImportSummary, String> {
    let driver = manager
        .get_connection(&connection_id)
        .await
        .map_err(|e| e.to_string())?;

    let mysql_mode = matches!(driver.driver_type(), "mysql" | "mariadb");

    // skip 模式需要已存在表清单（小写归一化）
    let existing: HashSet<String> = if on_conflict == "skip" {
        driver
            .get_tables(database.as_deref())
            .await
            .map_err(|e| format!("获取表清单失败: {}", e))?
            .into_iter()
            .map(|t| t.name.to_lowercase())
            .collect()
    } else {
        HashSet::new()
    };

    let file = File::open(&path).map_err(|e| format!("打开文件失败: {}", e))?;
    let mut reader = BufReader::with_capacity(1 << 20, file);
    let mut splitter = SqlSplitter::new(mysql_mode);

    let mut summary = ImportSummary {
        statements: 0,
        executed: 0,
        skipped: 0,
    };

    loop {
        let stmt = splitter
            .next_statement(&mut reader)
            .map_err(|e| format!("读取文件失败: {}", e))?;
        let Some(mut sql) = stmt else { break };
        summary.statements += 1;

        if let Some((name_token, insert_pos, has_ine)) = parse_create_table(&sql) {
            match on_conflict.as_str() {
                "skip" => {
                    if existing.contains(&normalize_table_name(&name_token)) {
                        summary.skipped += 1;
                        continue;
                    }
                }
                "overwrite" => {
                    driver
                        .execute(
                            &format!("DROP TABLE IF EXISTS {}", name_token),
                            database.as_deref(),
                            None,
                        )
                        .await
                        .map_err(|e| {
                            format!("第 {} 条语句前删除已存在表失败: {}", summary.statements, e)
                        })?;
                }
                _ => {}
            }
            // 转储没带 IF NOT EXISTS 时补上，避免同文件内重复建表语句报错
            if on_conflict != "error" && !has_ine {
                sql = format!("{} IF NOT EXISTS{}", &sql[..insert_pos], &sql[insert_pos..]);
            }
        }

        driver
            .execute(&sql, database.as_deref(), None)
            .await
            .map_err(|e| format!("第 {} 条语句执行失败: {}", summary.statements, e))?;
        summary.executed += 1;
    }

    Ok(summary)
}

/// 转储 SQL 文件：tables 为空则导出整个库
#[tauri::command]
pub async fn sql_dump_file(
    connection_id: String,
    database: Option<String>,
    tables: Option<Vec<String>>,
    path: String,
    include_drop: bool,
    include_data: bool,
    manager: State<'_, ConnectionPoolManager>,
) -> Result<DumpSummary, String> {
    let driver = manager
        .get_connection(&connection_id)
        .await
        .map_err(|e| e.to_string())?;
    let db = database.ok_or("请先选择目标数据库")?;

    let table_list: Vec<String> = match tables {
        Some(t) if !t.is_empty() => t,
        _ => driver
            .get_tables(Some(&db))
            .await
            .map_err(|e| format!("获取表清单失败: {}", e))?
            .into_iter()
            .map(|t| t.name)
            .collect(),
    };

    let file = File::create(&path).map_err(|e| format!("创建文件失败: {}", e))?;
    let mut writer = BufWriter::new(file);

    writeln!(writer, "-- SQLink SQL Dump").map_err(|e| e.to_string())?;
    writeln!(writer, "-- Database: {}", db).map_err(|e| e.to_string())?;
    writeln!(writer).map_err(|e| e.to_string())?;

    let mut total_rows = 0u64;
    for t in &table_list {
        let (sql, rows) = driver
            .dump_table(Some(&db), t, include_drop, include_data)
            .await
            .map_err(|e| format!("导出表 {} 失败: {}", t, e))?;
        writer
            .write_all(sql.as_bytes())
            .map_err(|e| format!("写入文件失败: {}", e))?;
        total_rows += rows;
    }
    writer.flush().map_err(|e| format!("写入文件失败: {}", e))?;

    Ok(DumpSummary {
        tables: table_list.len() as u64,
        rows: total_rows,
    })
}
