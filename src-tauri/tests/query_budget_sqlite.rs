//! 真实 SQLite 验证读入时预算、整行截断、资源释放与可信上下文拒绝发布。
use serde_json::json;
use sqlconnect_lib::{
    database::{
        pool::ConnectionPoolManager,
        result_budget::{MAX_QUERY_BYTES, MAX_QUERY_CELL_BYTES, MAX_QUERY_ROWS},
        sqlite::SqliteDriver,
        DatabaseDriver,
    },
    models::ConnectionConfig,
    table_result::{self, TablePageRequest},
};
use std::time::Duration;

fn config() -> ConnectionConfig {
    serde_json::from_value(json!({"id":"budget","name":"查询预算SQLite回归","driverType":"sqlite","host":"","port":0,"user":"","password":"","database":":memory:"})).unwrap()
}
fn numbers(end: usize, expression: &str) -> String {
    format!("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<{end}) SELECT {expression} FROM n")
}

#[tokio::test]
async fn row_boundary_and_early_stop_do_not_enumerate_the_entire_source() {
    let driver = SqliteDriver::connect(&config()).await.unwrap();
    for typed in [false, true] {
        let exact = driver
            .query(&numbers(MAX_QUERY_ROWS, "x"), None, None)
            .await
            .unwrap();
        assert_eq!(exact.rows.len(), MAX_QUERY_ROWS);
        assert!(!exact.truncated);
        let partial = tokio::time::timeout(Duration::from_secs(5), async {
            if typed {
                let read = driver
                    .query_typed(&numbers(100_000_000, "x, NULL"), &[], None)
                    .await
                    .unwrap();
                assert_eq!(read.typed_rows.len(), read.result.rows.len());
                for (row, typed) in read.result.rows.iter().zip(&read.typed_rows) {
                    assert_eq!(row.values.len(), typed.len());
                }
                read.result
            } else {
                driver
                    .query(&numbers(100_000_000, "x, NULL"), None, None)
                    .await
                    .unwrap()
            }
        })
        .await
        .expect("必须在枚举巨大数据源前停止读取");
        assert_eq!(partial.rows.len(), MAX_QUERY_ROWS);
        assert!(partial.truncated);
        assert_eq!(
            partial.rows.last().unwrap().values[0],
            Some(json!(MAX_QUERY_ROWS))
        );
    }
    driver.disconnect().await.unwrap();
}

#[tokio::test]
async fn utf8_and_escaped_text_hit_byte_budget_before_row_limit_without_cutting_cells() {
    let driver = SqliteDriver::connect(&config()).await.unwrap();
    for expression in [
        "x, replace(hex(zeroblob(65536)), '0', '中')",
        "x, CAST(zeroblob(131072) AS TEXT)",
    ] {
        for typed in [false, true] {
            let sql = numbers(10000, expression);
            let (result, typed_rows) = if typed {
                let read = driver.query_typed(&sql, &[], None).await.unwrap();
                assert_eq!(read.result.rows.len(), read.typed_rows.len());
                (read.result, Some(read.typed_rows))
            } else {
                (driver.query(&sql, None, None).await.unwrap(), None)
            };
            assert!(result.truncated);
            assert!(!result.rows.is_empty() && result.rows.len() < MAX_QUERY_ROWS);
            assert!(serde_json::to_vec(&result).unwrap().len() <= MAX_QUERY_BYTES);
            for row in &result.rows {
                let value = row.values[1].as_ref().unwrap().as_str().unwrap();
                if expression.contains("中") {
                    assert_eq!(value, "中".repeat(131072));
                } else {
                    assert_eq!(value.as_bytes(), vec![0; 131072]);
                }
            }
            if let Some(typed) = typed_rows {
                assert!(
                    serde_json::to_vec(&typed).unwrap().len()
                        + serde_json::to_vec(&result).unwrap().len()
                        <= MAX_QUERY_BYTES
                );
            }
        }
    }
    driver.disconnect().await.unwrap();
}

#[tokio::test]
async fn oversized_cell_returns_columns_and_no_partial_or_fabricated_value() {
    let driver = SqliteDriver::connect(&config()).await.unwrap();
    for typed in [false, true] {
        let sql = format!(
            "SELECT zeroblob({}) AS payload, 42 AS marker",
            MAX_QUERY_CELL_BYTES + 1
        );
        let result = if typed {
            let read = driver.query_typed(&sql, &[], None).await.unwrap();
            assert!(read.typed_rows.is_empty());
            read.result
        } else {
            driver.query(&sql, None, None).await.unwrap()
        };
        assert!(result.truncated);
        assert_eq!(result.columns.len(), 2);
        assert!(result.rows.is_empty());
        assert_eq!(result.total_count, Some(0));
    }
    driver.disconnect().await.unwrap();
}

#[tokio::test]
async fn binary_expansion_and_typed_payload_are_both_budgeted() {
    let driver = SqliteDriver::connect(&config()).await.unwrap();
    let sql = numbers(10000, &format!("zeroblob({MAX_QUERY_CELL_BYTES})"));
    let read = driver.query_typed(&sql, &[], None).await.unwrap();
    assert!(read.result.truncated);
    assert!(!read.typed_rows.is_empty());
    assert_eq!(read.typed_rows.len(), read.result.rows.len());
    assert!(
        serde_json::to_vec(&read.result).unwrap().len()
            + serde_json::to_vec(&read.typed_rows).unwrap().len()
            <= MAX_QUERY_BYTES
    );
    for (display, typed) in read.result.rows.iter().zip(&read.typed_rows) {
        assert_eq!(
            typed[0].as_ref().unwrap().binary_bytes().unwrap(),
            vec![0; MAX_QUERY_CELL_BYTES]
        );
        assert_eq!(
            display.values[0].as_ref().unwrap().as_str().unwrap().len(),
            2 + MAX_QUERY_CELL_BYTES * 2
        );
    }
    driver.disconnect().await.unwrap();
}

#[tokio::test]
async fn truncation_finalizes_statement_and_keeps_sqlite_session_and_next_lease_healthy() {
    let driver = SqliteDriver::connect(&config()).await.unwrap();
    let mut lease = driver.lease_session(None).await.unwrap();
    lease
        .execute("CREATE TEMP TABLE marker(value INTEGER)")
        .await
        .unwrap();
    lease.execute("INSERT INTO marker VALUES(7)").await.unwrap();
    let partial = lease.query(&numbers(100_000_000, "x")).await.unwrap();
    assert!(partial.truncated);
    assert_eq!(
        lease.query("SELECT value FROM marker").await.unwrap().rows[0].values[0],
        Some(json!(7))
    );
    lease.release().await.unwrap();
    let mut next = driver.lease_session(None).await.unwrap();
    assert_eq!(
        next.query("SELECT value FROM marker").await.unwrap().rows[0].values[0],
        Some(json!(7))
    );
    next.release().await.unwrap();
    driver.disconnect().await.unwrap();
}

#[tokio::test]
async fn truncated_controlled_page_never_publishes_editable_context_or_handles() {
    let manager = ConnectionPoolManager::new(None);
    manager.create_connection(config()).await.unwrap();
    let driver = manager.get_connection("budget").await.unwrap();
    driver
        .execute(
            "CREATE TABLE samples(id INTEGER PRIMARY KEY, payload TEXT)",
            None,
            None,
        )
        .await
        .unwrap();
    driver
        .execute(
            "INSERT INTO samples VALUES(1,CAST(zeroblob(1048577) AS TEXT))",
            None,
            None,
        )
        .await
        .unwrap();
    let request: TablePageRequest = serde_json::from_value(json!({"protocolVersion":1,"connectionId":"budget","database":"main","tableParts":["samples"],"page":1,"pageSize":20,"sort":null})).unwrap();
    let error = table_result::read_table_page(&manager, request)
        .await
        .unwrap_err();
    assert_eq!(error.code, "RESULT_BUDGET_EXCEEDED");
    assert_eq!(
        manager
            .result_contexts
            .stats(chrono::Utc::now().timestamp_millis())
            .unwrap()
            .contexts,
        0
    );
    assert_eq!(
        driver
            .query("SELECT count(*) FROM samples", None, None)
            .await
            .unwrap()
            .rows[0]
            .values[0],
        Some(json!(1))
    );
    manager.disconnect("budget").await.unwrap();
}
