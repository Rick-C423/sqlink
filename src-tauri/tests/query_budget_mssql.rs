//! 只读取显式 SQLINK_TEST_MSSQL 专用测试实例；不使用应用保存的连接。
//! 默认忽略，显式运行但缺少测试连接串时必须失败。
use reqwest::Url;
use sqlconnect_lib::database::result_budget::{
    MAX_QUERY_BYTES, MAX_QUERY_CELL_BYTES, MAX_QUERY_ROWS,
};
use sqlconnect_lib::database::{create_and_connect_driver, DatabaseDriver};
use sqlconnect_lib::models::{ConnectionConfig, QueryResult};
use std::time::Duration;

type TestResult<T = ()> = Result<T, String>;

fn require(condition: bool, message: &str) -> TestResult {
    if condition {
        Ok(())
    } else {
        Err(message.into())
    }
}

fn decode_component(encoded: &str) -> TestResult<String> {
    let mut result = Vec::new();
    let mut bytes = encoded.bytes();
    while let Some(byte) = bytes.next() {
        if byte == b'%' {
            let high = bytes.next().and_then(|byte| (byte as char).to_digit(16));
            let low = bytes.next().and_then(|byte| (byte as char).to_digit(16));
            match (high, low) {
                (Some(high), Some(low)) => result.push((high * 16 + low) as u8),
                _ => return Err("测试连接串字段含非法百分号编码".into()),
            }
        } else {
            result.push(byte);
        }
    }
    String::from_utf8(result).map_err(|_| "测试连接串字段不是 UTF-8".into())
}

fn config_from_env() -> TestResult<ConnectionConfig> {
    let raw = std::env::var("SQLINK_TEST_MSSQL")
        .map_err(|_| "必须设置 SQLINK_TEST_MSSQL 才能显式运行此测试")?;
    let url = Url::parse(&raw).map_err(|_| "SQLINK_TEST_MSSQL 不是有效测试连接串")?;
    require(
        matches!(url.scheme(), "mssql" | "sqlserver"),
        "测试连接串必须使用 mssql 或 sqlserver 协议",
    )?;
    require(url.fragment().is_none(), "测试连接串不支持片段")?;
    let database = decode_component(url.path().trim_start_matches('/'))?;
    require(!database.is_empty(), "测试连接串必须显式指定数据库")?;
    let mut encrypt = None;
    for (name, value) in url.query_pairs() {
        match (name.as_ref(), value.as_ref()) {
            ("encrypt", "true") => encrypt = Some(true),
            ("encrypt", "false") => encrypt = Some(false),
            _ => return Err("测试连接串包含不支持的参数".into()),
        }
    }
    // 错误信息不输出原始连接串，避免失败日志泄露凭据。
    Ok(ConnectionConfig {
        id: Some("budget-mssql".into()),
        name: "SQL Server 查询预算专用测试".into(),
        driver_type: "mssql".into(),
        host: url
            .host_str()
            .ok_or("测试连接串缺少主机")?
            .trim_start_matches('[')
            .trim_end_matches(']')
            .into(),
        port: url.port().unwrap_or(1433),
        user: decode_component(url.username())?,
        password: decode_component(url.password().unwrap_or_default())?,
        database: Some(database),
        encrypt,
        save_password: Some(false),
        group_name: None,
        color: None,
        mcp_allowed: Some(false),
    })
}

fn rows_sql(rows: usize, value: &str) -> String {
    format!(
        "WITH q(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM q WHERE n < {rows}) \
         SELECT {value} AS budget_value FROM q OPTION (MAXRECURSION 0)"
    )
}

async fn assert_isolated_and_new_lease_works(
    driver: &dyn DatabaseDriver,
    database: &str,
    mut lease: Box<dyn sqlconnect_lib::session::SessionLease>,
) -> TestResult {
    require(
        lease.query("SELECT 1").await.is_err(),
        "提前结束的响应不得把 Client 放回原租约",
    )?;
    lease.release().await.map_err(|_| "隔离租约释放失败")?;
    let mut next = driver
        .lease_session(Some(database))
        .await
        .map_err(|_| "截断后新租约创建失败")?;
    let result = next
        .query("SELECT 1 AS alive")
        .await
        .map_err(|_| "截断后新租约查询失败")?;
    require(
        result.rows.len() == 1 && !result.truncated,
        "截断不得损坏后续独立租约",
    )?;
    next.release()
        .await
        .map_err(|_| "新租约释放失败".to_string())
}

async fn exercise(driver: &dyn DatabaseDriver, database: &str) -> TestResult {
    require(
        !driver.session_capabilities().cancellation,
        "预算截断不得宣称支持服务端取消",
    )?;
    for typed in [false, true] {
        let mut complete = driver
            .lease_session(Some(database))
            .await
            .map_err(|_| "完整结果租约创建失败")?;
        let zero_sql = "SELECT CAST(NULL AS int) AS empty_value WHERE 1 = 0";
        let empty = if typed {
            complete.query_typed(zero_sql, &[]).await.map(|r| r.result)
        } else {
            complete.query(zero_sql).await
        }
        .map_err(|_| "零行结果读取失败")?;
        require(
            empty.columns.len() == 1 && empty.rows.is_empty() && !empty.truncated,
            "零行结果必须保留列元数据",
        )?;
        let exact_sql = rows_sql(MAX_QUERY_ROWS, "n");
        let exact = if typed {
            complete
                .query_typed(&exact_sql, &[])
                .await
                .map(|r| r.result)
        } else {
            complete.query(&exact_sql).await
        }
        .map_err(|_| "恰好行上限的结果读取失败")?;
        require(
            exact.rows.len() == MAX_QUERY_ROWS && !exact.truncated,
            "恰好行上限不能误报截断",
        )?;
        complete
            .query("SELECT 1")
            .await
            .map_err(|_| "完整流读完后同租约应可继续使用")?;
        complete.release().await.map_err(|_| "完整租约释放失败")?;

        let cases = [
            (rows_sql(MAX_QUERY_ROWS + 50_000, "n"), Some(MAX_QUERY_ROWS)),
            (
                format!(
                    "SELECT REPLICATE(CAST('x' AS varchar(max)), {}) AS too_large",
                    MAX_QUERY_CELL_BYTES + 1
                ),
                Some(0),
            ),
            (
                rows_sql(1000, "REPLICATE(CAST('x' AS varchar(max)), 262144)"),
                None,
            ),
        ];
        for (sql, expected_rows) in cases {
            let mut lease = driver
                .lease_session(Some(database))
                .await
                .map_err(|_| "预算场景租约创建失败")?;
            let result: QueryResult = if typed {
                let result = lease
                    .query_typed(&sql, &[])
                    .await
                    .map_err(|_| "类型化预算查询失败")?;
                require(
                    result.typed_rows.len() == result.result.rows.len(),
                    "类型行和显示行必须同步截断",
                )?;
                require(
                    serde_json::to_vec(&(&result.result, &result.typed_rows))
                        .map_err(|_| "类型结果编码失败")?
                        .len()
                        < MAX_QUERY_BYTES,
                    "类型化结果编码超过字节预算",
                )?;
                result.result
            } else {
                lease.query(&sql).await.map_err(|_| "普通预算查询失败")?
            };
            require(result.truncated, "预算用尽必须返回截断标记")?;
            require(
                result.columns.len() == 1 && result.total_count == Some(result.rows.len()),
                "截断结果的元数据或计数不一致",
            )?;
            if let Some(expected_rows) = expected_rows {
                require(result.rows.len() == expected_rows, "截断行数与预算不符")?;
            } else {
                require(
                    !result.rows.is_empty() && result.rows.len() < 1000,
                    "大行结果应按字节预算提前停止",
                )?;
            }
            require(
                serde_json::to_vec(&result)
                    .map_err(|_| "结果编码失败")?
                    .len()
                    < MAX_QUERY_BYTES,
                "查询结果编码超过字节预算",
            )?;
            assert_isolated_and_new_lease_works(driver, database, lease).await?;
        }
    }
    let mut multiple = driver
        .lease_session(Some(database))
        .await
        .map_err(|_| "多结果租约创建失败")?;
    let first = multiple
        .query("SELECT 1 AS first_result; SELECT 2 AS later_result")
        .await
        .map_err(|_| "多结果查询失败")?;
    require(
        first.rows.len() == 1 && first.columns[0].name == "first_result" && first.truncated,
        "多结果应只保留首结果并明确提前结束",
    )?;
    assert_isolated_and_new_lease_works(driver, database, multiple).await
}

#[tokio::test]
#[ignore = "需要 SQLINK_TEST_MSSQL 指向专用 SQL Server 测试实例"]
async fn mssql_query_budget_streams_and_isolates_unread_responses() -> TestResult {
    let config = config_from_env()?;
    let database = config.database.as_deref().ok_or("测试数据库缺失")?;
    let driver = create_and_connect_driver(&config)
        .await
        .map_err(|_| "专用 SQL Server 测试连接失败")?;
    let result = tokio::time::timeout(Duration::from_secs(60), exercise(driver.as_ref(), database))
        .await
        .map_err(|_| "SQL Server 查询预算测试超过 60 秒".to_string())
        .and_then(|result| result);
    let disconnected = driver
        .disconnect()
        .await
        .map_err(|_| "专用测试连接关闭失败".to_string());
    result?;
    disconnected
}
