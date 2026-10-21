//! 显式使用独立 MySQL 测试库；只执行 SELECT/会话变量，不接触应用保存连接或业务对象。
use sqlconnect_lib::{
    database::{
        mysql::MySqlDriver,
        result_budget::{MAX_QUERY_BYTES, MAX_QUERY_CELL_BYTES, MAX_QUERY_ROWS},
        DatabaseDriver,
    },
    models::{ConnectionConfig, QueryResult},
};
use std::time::Duration;

type TestResult<T = ()> = Result<T, String>;
async fn driver() -> TestResult<MySqlDriver> {
    let raw = std::env::var("SQLINK_TEST_MYSQL")
        .map_err(|_| "显式运行必须设置SQLINK_TEST_MYSQL独立测试库")?;
    let opts = mysql_async::Opts::from_url(&raw).map_err(|_| "MySQL测试连接串无效")?;
    let database = opts
        .db_name()
        .filter(|name| !name.is_empty())
        .ok_or("必须明确指定独立测试数据库")?;
    let config: ConnectionConfig = serde_json::from_value(serde_json::json!({
        "name":"MySQL查询预算回归", "driverType":"mysql", "host":opts.ip_or_hostname(), "port":opts.tcp_port(),
        "user":opts.user().unwrap_or(""), "password":opts.pass().unwrap_or(""), "database":database, "savePassword":false,
    })).map_err(|_| "测试配置构造失败")?;
    MySqlDriver::connect(&config)
        .await
        .map_err(|_| "独立测试库连接失败".into())
}
fn numbers(end: usize) -> String {
    let digits = "(SELECT 0 d UNION ALL SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3 UNION ALL SELECT 4 UNION ALL SELECT 5 UNION ALL SELECT 6 UNION ALL SELECT 7 UNION ALL SELECT 8 UNION ALL SELECT 9)";
    format!("SELECT a.d*10000+b.d*1000+c.d*100+d.d*10+e.d+1 AS n FROM {digits} a CROSS JOIN {digits} b CROSS JOIN {digits} c CROSS JOIN {digits} d CROSS JOIN {digits} e ORDER BY n LIMIT {end}")
}
fn first_number(result: &QueryResult) -> i64 {
    let value = result.rows[0].values[0].as_ref().unwrap();
    value
        .as_i64()
        .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
        .unwrap()
}

#[tokio::test]
#[ignore = "需要SQLINK_TEST_MYSQL独立测试库"]
async fn mysql_stream_limit_closes_unread_lease_without_draining_slow_tail() -> TestResult {
    let driver = driver().await?;
    for typed in [false, true] {
        let mut lease = driver
            .lease_session(None)
            .await
            .map_err(|_| "租约创建失败")?;
        let old = first_number(
            &lease
                .query("SELECT CONNECTION_ID()")
                .await
                .map_err(|_| "会话ID读取失败")?,
        );
        let cancel = lease.cancel_handle().ok_or("缺少取消句柄")?;
        lease
            .execute("SET @p2_marker=7")
            .await
            .map_err(|_| "会话变量设置失败")?;
        let exact = lease
            .query(&numbers(MAX_QUERY_ROWS))
            .await
            .map_err(|_| "行数边界查询失败")?;
        assert_eq!(exact.rows.len(), MAX_QUERY_ROWS);
        assert!(!exact.truncated);
        assert_eq!(
            first_number(
                &lease
                    .query("SELECT 42")
                    .await
                    .map_err(|_| "完整读后租约不可用")?
            ),
            42
        );
        // 在慢尾前留出网络缓冲刷新空间，不能要求服务端立即发送最后一条探测行。
        let slow_after = MAX_QUERY_ROWS + 4096;
        let sql = format!(
            "SELECT n, IF(n>{slow_after},SLEEP(30),0) AS slow FROM ({}) data",
            numbers(slow_after + 1)
        );
        let partial = tokio::time::timeout(Duration::from_secs(5), async {
            if typed {
                let read = lease
                    .query_typed(&sql, &[])
                    .await
                    .map_err(|_| "类型化截断失败")?;
                assert_eq!(read.typed_rows.len(), read.result.rows.len());
                Ok::<_, String>(read.result)
            } else {
                lease.query(&sql).await.map_err(|_| "普通截断失败".into())
            }
        })
        .await
        .map_err(|_| "截断仍等待慢尾部")??;
        assert!(partial.truncated);
        assert_eq!(partial.rows.len(), MAX_QUERY_ROWS);
        assert!(lease.query("SELECT 1").await.is_err());
        assert!(lease.execute("SELECT 1").await.is_err());
        assert!(cancel.cancel().await.is_err());
        lease.release().await.map_err(|_| "截断后归还失败")?;
        let mut next = driver.lease_session(None).await.map_err(|_| "新租约失败")?;
        assert_ne!(
            first_number(
                &next
                    .query("SELECT CONNECTION_ID()")
                    .await
                    .map_err(|_| "新会话ID失败")?
            ),
            old
        );
        assert!(next
            .query("SELECT @p2_marker")
            .await
            .map_err(|_| "新会话变量失败")?
            .rows[0]
            .values[0]
            .as_ref()
            .unwrap()
            .is_null());
        assert!(cancel.cancel().await.is_err());
        assert_eq!(
            first_number(
                &next
                    .query("SELECT 43")
                    .await
                    .map_err(|_| "旧取消污染新租约")?
            ),
            43
        );
        next.release().await.map_err(|_| "新租约归还失败")?;
    }
    driver.disconnect().await.map_err(|_| "断开失败".into())
}

#[tokio::test]
#[ignore = "需要SQLINK_TEST_MYSQL独立测试库"]
async fn mysql_byte_and_cell_limits_preserve_whole_aligned_rows() -> TestResult {
    let driver = driver().await?;
    for typed in [false, true] {
        let sql = format!(
            "SELECT n, REPEAT('中',100000) AS payload FROM ({}) data",
            numbers(1000)
        );
        let result = if typed {
            let read = driver
                .query_typed(&sql, &[], None)
                .await
                .map_err(|_| "类型化字节预算失败")?;
            assert_eq!(read.result.rows.len(), read.typed_rows.len());
            assert!(
                serde_json::to_vec(&read.typed_rows).unwrap().len()
                    + serde_json::to_vec(&read.result).unwrap().len()
                    <= MAX_QUERY_BYTES
            );
            read.result
        } else {
            driver
                .query(&sql, None, None)
                .await
                .map_err(|_| "普通字节预算失败")?
        };
        assert!(result.truncated);
        assert!(!result.rows.is_empty() && result.rows.len() < 1000);
        assert!(serde_json::to_vec(&result).unwrap().len() <= MAX_QUERY_BYTES);
        for row in &result.rows {
            assert_eq!(
                row.values[1].as_ref().unwrap().as_str().unwrap(),
                "中".repeat(100000)
            );
        }
        let sql = format!("SELECT REPEAT('x',{}) AS payload", MAX_QUERY_CELL_BYTES + 1);
        let result = if typed {
            driver
                .query_typed(&sql, &[], None)
                .await
                .map_err(|_| "超大类型化单元格失败")?
                .result
        } else {
            driver
                .query(&sql, None, None)
                .await
                .map_err(|_| "超大普通单元格失败")?
        };
        assert!(result.truncated);
        assert!(result.rows.is_empty());
        assert_eq!(result.columns.len(), 1);
    }
    let mut lease = driver
        .lease_session(None)
        .await
        .map_err(|_| "多结果租约失败")?;
    let result = lease
        .query("SELECT 1; SELECT 2")
        .await
        .map_err(|_| "多结果读取失败")?;
    assert_eq!(first_number(&result), 1);
    assert!(result.truncated);
    assert!(lease.query("SELECT 3").await.is_err());
    lease.release().await.map_err(|_| "多结果归还失败")?;
    driver.disconnect().await.map_err(|_| "断开失败".into())
}
