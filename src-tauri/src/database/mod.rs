pub mod pool;
pub mod mysql;
pub mod postgres;
pub mod sqlite;
pub mod mssql;
pub mod dump;

use async_trait::async_trait;
use std::sync::Arc;
use crate::models::*;

/// 数据库驱动 trait —— 所有方法都只需要 &self（内部可变性）
#[async_trait]
pub trait DatabaseDriver: Send + Sync {
    fn driver_type(&self) -> &'static str;
    fn is_connected(&self) -> bool;

    /// query_id 用于查询取消（KILL QUERY）；database 为查询上下文库（执行前 USE）
    async fn query(&self, sql: &str, database: Option<&str>, query_id: Option<u64>) -> Result<QueryResult, DatabaseError>;
    async fn execute(&self, sql: &str, database: Option<&str>, query_id: Option<u64>) -> Result<QueryResult, DatabaseError>;
    async fn cancel_query(&self, query_id: u64) -> Result<(), DatabaseError>;

    async fn get_databases(&self) -> Result<Vec<String>, DatabaseError>;
    async fn get_tables(&self, database: Option<&str>) -> Result<Vec<TableInfo>, DatabaseError>;
    /// 视图列表（不含表；row_count/engine 一般无值）
    async fn get_views(&self, database: Option<&str>) -> Result<Vec<TableInfo>, DatabaseError>;
    /// 视图的创建定义 SQL（只读展示用）
    async fn get_view_definition(&self, database: Option<&str>, view: &str) -> Result<String, DatabaseError>;
    /// 账号列表（服务器级；database 仅用于选择连接上下文）
    async fn get_users(&self, database: Option<&str>) -> Result<Vec<DbUserInfo>, DatabaseError>;
    /// 当前会话用户及其授权（可读文本列表）
    async fn get_current_user(&self, database: Option<&str>) -> Result<CurrentUserInfo, DatabaseError>;
    async fn get_columns(&self, database: Option<&str>, table: &str) -> Result<Vec<ColumnInfo>, DatabaseError>;
    async fn get_indexes(&self, database: Option<&str>, table: &str) -> Result<Vec<IndexInfo>, DatabaseError>;
    async fn get_foreign_keys(&self, database: Option<&str>, table: &str) -> Result<Vec<ForeignKeyInfo>, DatabaseError>;

    /// 在一个事务里批量执行写语句（数据网格编辑提交用），返回总影响行数，失败整体回滚
    async fn execute_batch(&self, sqls: Vec<String>, database: Option<&str>) -> Result<u64, DatabaseError>;

    /// 整表导出 CSV 到指定路径，返回导出行数（不受查询行数上限限制）
    async fn export_table_csv(&self, database: Option<&str>, table: &str, path: &str) -> Result<u64, DatabaseError>;

    /// 生成单表转储 SQL（DROP + CREATE + 可选 INSERT），返回 (SQL 文本, 数据行数)
    async fn dump_table(&self, database: Option<&str>, table: &str, include_drop: bool, include_data: bool) -> Result<(String, u64), DatabaseError>;

    async fn disconnect(&self) -> Result<(), DatabaseError>;

    fn escape_identifier(&self, identifier: &str) -> String;
}

/// CSV 写出工具（手动转义：含逗号/引号/换行的值加双引号包裹）
pub fn write_csv(
    path: &str,
    columns: &[String],
    rows: Vec<Vec<Option<serde_json::Value>>>,
) -> Result<u64, DatabaseError> {
    let mut out = String::new();
    out.push_str(&columns.iter().map(|c| csv_escape(c)).collect::<Vec<_>>().join(","));
    out.push('\n');

    let mut count = 0u64;
    for row in rows {
        let line = row.iter().map(|v| {
            match v {
                None => String::new(),
                Some(serde_json::Value::Null) => String::new(),
                Some(serde_json::Value::Number(n)) => n.to_string(),
                Some(serde_json::Value::Bool(b)) => b.to_string(),
                Some(serde_json::Value::String(s)) => csv_escape(s),
                Some(other) => csv_escape(&other.to_string()),
            }
        }).collect::<Vec<_>>().join(",");
        out.push_str(&line);
        out.push('\n');
        count += 1;
    }

    std::fs::write(path, out)
        .map_err(|e| DatabaseError::Unknown(format!("写入 CSV 失败: {}", e)))?;
    Ok(count)
}

fn csv_escape(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') || s.contains('\r') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// 返回行集的语句首关键字（判断走 query 还是 execute 分支）
const QUERY_KEYWORDS: &[&str] = &[
    "SELECT", "SHOW", "EXPLAIN", "DESC", "DESCRIBE", "WITH", "VALUES", "TABLE", "ANALYZE",
];

/// 判断一条 SQL 是否返回结果集（跳过空白与 `--`、`#`、`/* */` 注释）
pub fn returns_rows(sql: &str) -> bool {
    let keyword = first_keyword(sql);
    QUERY_KEYWORDS.contains(&keyword.as_str())
}

/// 提取 SQL 的第一个关键字（跳过前导空白和注释），返回大写形式
pub fn first_keyword(sql: &str) -> String {
    let bytes = sql.as_bytes();
    let n = bytes.len();
    let mut i = 0;

    loop {
        // 跳过空白
        while i < n && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= n {
            return String::new();
        }
        // -- 行注释（MySQL 要求 -- 后跟空白，这里宽松处理）
        if i + 1 < n && bytes[i] == b'-' && bytes[i + 1] == b'-' {
            while i < n && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        // # 行注释
        if bytes[i] == b'#' {
            while i < n && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        // /* */ 块注释
        if i + 1 < n && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < n && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(n);
            continue;
        }
        break;
    }

    let start = i;
    while i < n && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
        i += 1;
    }
    sql[start..i].to_uppercase()
}

/// 测试连接（独立函数，不需要创建 driver 实例）
pub async fn test_connection(config: &ConnectionConfig) -> Result<(), DatabaseError> {
    match config.driver_type.as_str() {
        "mysql" | "mariadb" => mysql::test_connection(config).await,
        "postgres" | "postgresql" => postgres::test_connection(config).await,
        "sqlite" => sqlite::test_connection(config).await,
        "sqlserver" | "mssql" => mssql::test_connection(config).await,
        _ => Err(DatabaseError::DriverNotSupported(config.driver_type.clone())),
    }
}

/// 创建并连接驱动
pub async fn create_and_connect_driver(
    config: &ConnectionConfig
) -> Result<Arc<dyn DatabaseDriver>, DatabaseError> {
    match config.driver_type.as_str() {
        "mysql" | "mariadb" => {
            let driver = mysql::MySqlDriver::connect(config).await?;
            Ok(Arc::new(driver))
        }
        "postgres" | "postgresql" => {
            let driver = postgres::PgDriver::connect(config).await?;
            Ok(Arc::new(driver))
        }
        "sqlite" => {
            let driver = sqlite::SqliteDriver::connect(config).await?;
            Ok(Arc::new(driver))
        }
        "sqlserver" | "mssql" => {
            let driver = mssql::MsSqlDriver::connect(config).await?;
            Ok(Arc::new(driver))
        }
        _ => Err(DatabaseError::DriverNotSupported(config.driver_type.clone())),
    }
}
