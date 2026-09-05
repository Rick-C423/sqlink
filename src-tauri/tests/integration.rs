//! 四数据库集成测试骨架。
//!
//! 默认只运行 SQLite 用例（临时文件，零配置）；MySQL / PostgreSQL / SQL Server
//! 通过环境变量提供连接串后自动启用，未设置则跳过：
//!
//! ```text
//! SQLINK_TEST_MYSQL=mysql://user:password@127.0.0.1:3306/sqlink_test
//! SQLINK_TEST_POSTGRES=postgres://user:password@127.0.0.1:5432/sqlink_test
//! SQLINK_TEST_MSSQL=mssql://user:password@127.0.0.1:1433/master
//! ```
//!
//! 每个驱动覆盖：连接、查询、建表/写入/回读（引号、换行、中文）、事务批量
//! 执行与回滚、Schema 元数据、标识符转义、CSV 导出与单表转储。
//!
//! 运行：`cargo test --manifest-path src-tauri/Cargo.toml --test integration`

use std::sync::Arc;

use sqlconnect_lib::database::{create_and_connect_driver, DatabaseDriver};
use sqlconnect_lib::models::ConnectionConfig;

const TABLE: &str = "sqlink_it";
/// 含单引号、双引号、中文；note 含换行——字面量转义与二进制安全的冒烟值
const NAME_VALUE: &str = "it's 中文 \"quoted\"";
const NOTE_VALUE: &str = "line1\nline2";

/// 解析 driver://user:password@host:port/database 形式的连接串
fn parse_conn(driver: &str, url: &str) -> ConnectionConfig {
    let prefix = format!("{driver}://");
    let rest = url
        .strip_prefix(&prefix)
        .unwrap_or_else(|| panic!("连接串需以 {prefix} 开头"));
    let (authority, database) = match rest.split_once('/') {
        Some((a, d)) => (a, d.to_string()),
        None => (rest, String::new()),
    };
    let (userinfo, hostport) = match authority.split_once('@') {
        Some((u, h)) => (u, h),
        None => ("", authority),
    };
    let (user, password) = userinfo.split_once(':').unwrap_or((userinfo, ""));
    let (host, port_str) = hostport.split_once(':').unwrap_or((hostport, ""));
    let port: u16 = if port_str.is_empty() {
        match driver {
            "postgres" => 5432,
            "mssql" => 1433,
            _ => 3306,
        }
    } else {
        port_str.parse().expect("端口非法")
    };

    ConnectionConfig {
        id: None,
        name: format!("integration-{driver}"),
        driver_type: driver.to_string(),
        host: host.to_string(),
        port,
        user: user.to_string(),
        password: password.to_string(),
        database: if database.is_empty() { None } else { Some(database) },
        encrypt: None,
        save_password: None,
        group_name: None,
        color: None,
        mcp_allowed: None,
    }
}

/// 按驱动生成测试表的 DDL/写入/清理语句
fn dialect(driver: &str) -> (&'static str, &'static str, &'static str) {
    match driver {
        "mysql" => (
            "CREATE TABLE sqlink_it (id INT PRIMARY KEY, name VARCHAR(200), note TEXT)",
            "DROP TABLE IF EXISTS sqlink_it",
            "LIMIT 1 OFFSET 0",
        ),
        "postgres" => (
            "CREATE TABLE sqlink_it (id INT PRIMARY KEY, name VARCHAR(200), note TEXT)",
            "DROP TABLE IF EXISTS sqlink_it",
            "LIMIT 1 OFFSET 0",
        ),
        "mssql" => (
            "CREATE TABLE sqlink_it (id INT PRIMARY KEY, name NVARCHAR(200), note NVARCHAR(MAX))",
            "IF OBJECT_ID(N'sqlink_it', N'U') IS NOT NULL DROP TABLE sqlink_it",
            "OFFSET 0 ROWS FETCH NEXT 1 ROWS ONLY",
        ),
        _ => (
            "CREATE TABLE sqlink_it (id INTEGER PRIMARY KEY, name TEXT, note TEXT)",
            "DROP TABLE IF EXISTS sqlink_it",
            "LIMIT 1 OFFSET 0",
        ),
    }
}

/// 单驱动的完整冒烟场景
async fn exercise(driver: Arc<dyn DatabaseDriver>) {
    let kind = driver.driver_type();
    let (create, drop, _paging) = dialect(kind);

    // 1) 基础查询
    let r = driver.query("SELECT 1", None, None).await.expect("SELECT 1 失败");
    assert_eq!(r.rows.len(), 1, "SELECT 1 应返回一行");
    assert!(!r.columns.is_empty());

    // 2) 清理旧表 → 建表 → 写入特殊值 → 回读
    driver.execute(drop, None, None).await.ok();
    driver.execute(create, None, None).await.expect("建表失败");
    driver
        .execute(
            &format!(
                "INSERT INTO {} (id, name, note) VALUES (1, '{}', '{}')",
                driver.escape_identifier(TABLE),
                NAME_VALUE.replace('\'', "''"),
                NOTE_VALUE.replace('\'', "''")
            ),
            None,
            None,
        )
        .await
        .expect("写入失败");

    let read = driver
        .query(
            &format!(
                "SELECT name, note FROM {} WHERE id = 1",
                driver.escape_identifier(TABLE)
            ),
            None,
            None,
        )
        .await
        .expect("回读失败");
    assert_eq!(read.rows.len(), 1);
    let got = |i: usize| -> String {
        match &read.rows[0].values[i] {
            Some(serde_json::Value::String(s)) => s.clone(),
            Some(serde_json::Value::Number(n)) => n.to_string(),
            Some(serde_json::Value::Bool(b)) => b.to_string(),
            _ => String::new(),
        }
    };
    assert_eq!(got(0), NAME_VALUE, "引号/中文回读不一致");
    assert_eq!(got(1).replace("\r\n", "\n"), NOTE_VALUE, "换行回读不一致");

    // 3) Schema 元数据：表清单包含测试表，列信息包含 name
    let tables = driver.get_tables(None).await.expect("get_tables 失败");
    assert!(
        tables.iter().any(|t| t.name == TABLE),
        "{kind} get_tables 未包含 {TABLE}: {:?}",
        tables.iter().map(|t| &t.name).collect::<Vec<_>>()
    );
    let columns = driver.get_columns(None, TABLE).await.expect("get_columns 失败");
    assert!(columns.iter().any(|c| c.name == "name"), "get_columns 缺少 name 列");

    // 4) 事务批处理：两条 INSERT 成功提交
    let inserted = driver
        .execute_batch(
            vec![
                format!(
                    "INSERT INTO {} (id, name, note) VALUES (2, 'batch-a', 'a')",
                    driver.escape_identifier(TABLE)
                ),
                format!(
                    "INSERT INTO {} (id, name, note) VALUES (3, 'batch-b', 'b')",
                    driver.escape_identifier(TABLE)
                ),
            ],
            None,
        )
        .await
        .expect("事务批处理失败");
    assert_eq!(inserted, 2, "批处理应影响 2 行");

    // 5) 事务回滚：主键冲突使整批回滚，行数不变
    let conflict = format!(
        "INSERT INTO {} (id, name, note) VALUES (2, 'dup', 'dup')",
        driver.escape_identifier(TABLE)
    );
    let result = driver
        .execute_batch(
            vec![
                format!(
                    "INSERT INTO {} (id, name, note) VALUES (4, 'should-rollback', 'x')",
                    driver.escape_identifier(TABLE)
                ),
                conflict,
            ],
            None,
        )
        .await;
    assert!(result.is_err(), "主键冲突应使批处理失败");
    let count = driver
        .query(&format!("SELECT COUNT(*) FROM {}", driver.escape_identifier(TABLE)), None, None)
        .await
        .expect("计数失败");
    let count_value = match &count.rows[0].values[0] {
        Some(serde_json::Value::Number(n)) => n.as_u64().unwrap_or(0),
        Some(serde_json::Value::String(s)) => s.parse().unwrap_or(0),
        _ => 0,
    };
    assert_eq!(count_value, 3, "回滚后应只剩 3 行（id 1/2/3）");

    // 6) CSV 导出：3 行数据 + 表头
    let csv_path = std::env::temp_dir()
        .join(format!("sqlink-it-{}-{}.csv", kind, uuid::Uuid::new_v4()))
        .to_string_lossy()
        .into_owned();
    let csv_rows = driver
        .export_table_csv(None, TABLE, &csv_path)
        .await
        .expect("CSV 导出失败");
    assert_eq!(csv_rows, 3, "CSV 应导出 3 行");

    // 7) 单表转储：包含 CREATE TABLE
    let (dump_sql, dump_rows) = driver
        .dump_table(None, TABLE, true, true)
        .await
        .expect("转储失败");
    assert_eq!(dump_rows, 3);
    assert!(
        dump_sql.to_uppercase().contains("CREATE TABLE"),
        "转储应包含建表语句"
    );

    // 清理
    driver.execute(drop, None, None).await.ok();
    let _ = std::fs::remove_file(&csv_path);
    driver.disconnect().await.ok();
}

/// 读取环境变量连接串；未设置返回 None（跳过）
async fn driver_from_env(env: &str, driver: &str) -> Option<Arc<dyn DatabaseDriver>> {
    let url = std::env::var(env).ok()?;
    match create_and_connect_driver(&parse_conn(driver, &url)).await {
        Ok(d) => Some(d),
        Err(e) => panic!("{env} 已设置但连接失败: {e}"),
    }
}

/// SQLite：零配置，始终运行
#[tokio::test]
async fn sqlite_full_flow() {
    let path = std::env::temp_dir().join(format!("sqlink-it-{}.db", uuid::Uuid::new_v4()));
    let driver = create_and_connect_driver(&ConnectionConfig {
        id: None,
        name: "integration-sqlite".into(),
        driver_type: "sqlite".into(),
        host: String::new(),
        port: 0,
        user: String::new(),
        password: String::new(),
        database: Some(path.to_string_lossy().into_owned()),
        encrypt: None,
        save_password: None,
        group_name: None,
        color: None,
        mcp_allowed: None,
    })
    .await
    .expect("SQLite 连接失败");
    exercise(driver).await;
    std::fs::remove_file(&path).ok();
}

#[tokio::test]
async fn mysql_full_flow() {
    let Some(driver) = driver_from_env("SQLINK_TEST_MYSQL", "mysql").await else {
        eprintln!("跳过 MySQL 集成测试：未设置 SQLINK_TEST_MYSQL");
        return;
    };
    exercise(driver).await;
}

#[tokio::test]
async fn postgres_full_flow() {
    let Some(driver) = driver_from_env("SQLINK_TEST_POSTGRES", "postgres").await else {
        eprintln!("跳过 PostgreSQL 集成测试：未设置 SQLINK_TEST_POSTGRES");
        return;
    };
    exercise(driver).await;
}

#[tokio::test]
async fn mssql_full_flow() {
    let Some(driver) = driver_from_env("SQLINK_TEST_MSSQL", "mssql").await else {
        eprintln!("跳过 SQL Server 集成测试：未设置 SQLINK_TEST_MSSQL");
        return;
    };
    exercise(driver).await;
}
