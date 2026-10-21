//! PostgreSQL 流式结果预算真实回归，只读取显式指定的独立测试库。
//! 显式执行：cargo test --test query_budget_postgres -- --ignored。
//! 缺少 SQLINK_TEST_POSTGRES 必须失败；测试不会打印连接串或创建业务表。
use sqlconnect_lib::database::postgres::PgDriver;
use sqlconnect_lib::database::result_budget::{
    MAX_QUERY_BYTES, MAX_QUERY_CELL_BYTES, MAX_QUERY_ROWS,
};
use sqlconnect_lib::database::DatabaseDriver;
use sqlconnect_lib::models::{ConnectionConfig, QueryResult};
use sqlconnect_lib::typed_value::TypedValue;
use std::time::Duration;
use tokio_postgres::{config::Host, Config};

type TestResult<T = ()> = Result<T, String>;

async fn connect() -> TestResult<PgDriver> {
    let raw = std::env::var("SQLINK_TEST_POSTGRES")
        .map_err(|_| "显式运行必须设置 SQLINK_TEST_POSTGRES 并指定独立测试数据库")?;
    let cfg: Config = raw.parse().map_err(|_| "PostgreSQL 测试连接串无效")?;
    let host = match cfg.get_hosts() {
        [Host::Tcp(host)] => host,
        _ => return Err("测试连接串必须包含单个 TCP 主机".into()),
    };
    let database = cfg
        .get_dbname()
        .filter(|value| !value.is_empty())
        .ok_or("必须明确指定独立测试数据库")?;
    let config: ConnectionConfig = serde_json::from_value(serde_json::json!({
        "name": "PostgreSQL 查询预算回归", "driverType": "postgres", "host": host,
        "port": cfg.get_ports().first().copied().unwrap_or(5432),
        "user": cfg.get_user().unwrap_or("postgres"),
        "password": String::from_utf8_lossy(cfg.get_password().unwrap_or_default()),
        "database": database, "savePassword": false, "mcpAllowed": false
    }))
    .map_err(|_| "测试连接配置构造失败")?;
    tokio::time::timeout(Duration::from_secs(10), PgDriver::connect(&config))
        .await
        .map_err(|_| "连接测试库超时")?
        .map_err(|_| "连接测试库失败".into())
}

fn number(result: &QueryResult) -> TestResult<i64> {
    let value = result
        .rows
        .first()
        .and_then(|row| row.values.first())
        .and_then(|value| value.as_ref())
        .ok_or("缺少结果值")?;
    value
        .as_i64()
        .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
        .ok_or_else(|| "结果值不是整数".into())
}

#[tokio::test]
#[ignore = "需要 SQLINK_TEST_POSTGRES 指向独立 PostgreSQL 测试库"]
async fn postgres_row_limit_stops_before_slow_tail_and_quarantines_lease() -> TestResult {
    let driver = connect().await?;
    for typed in [false, true] {
        let mut lease = driver
            .lease_session(None)
            .await
            .map_err(|_| "获取旧租约失败")?;
        let old_backend = number(
            &lease
                .query("SELECT pg_backend_pid()")
                .await
                .map_err(|_| "旧物理会话读取失败")?,
        )?;
        let old_cancel = lease.cancel_handle().ok_or("租约不支持取消")?;
        let exact = format!("SELECT g FROM generate_series(1, {MAX_QUERY_ROWS}) g");
        let exact = lease
            .query(&exact)
            .await
            .map_err(|_| "恰好行上限查询失败")?;
        assert_eq!(exact.rows.len(), MAX_QUERY_ROWS);
        assert!(!exact.truncated);
        assert_eq!(
            number(
                &lease
                    .query("SELECT 1")
                    .await
                    .map_err(|_| "完整读取后租约不可复用")?
            )?,
            1
        );

        // 首条超限行用于探测；慢尾前留出网络缓冲刷新空间，排空仍会触发超时。
        let probe = MAX_QUERY_ROWS + 4096;
        let sql = format!("SELECT g, CASE WHEN g > {probe} THEN pg_sleep(30)::text ELSE '' END FROM generate_series(1, {}) g", probe + 1);
        let partial = tokio::time::timeout(Duration::from_secs(5), async {
            if typed {
                let result = lease
                    .query_typed(&sql, &[])
                    .await
                    .map_err(|_| "类型化截断查询失败")?;
                assert_eq!(result.typed_rows.len(), result.result.rows.len());
                assert!(result
                    .typed_rows
                    .iter()
                    .all(|row| row.len() == result.result.columns.len()));
                Ok::<_, String>(result.result)
            } else {
                lease
                    .query(&sql)
                    .await
                    .map_err(|_| "普通截断查询失败".into())
            }
        })
        .await
        .map_err(|_| "截断查询仍在读取慢尾部")??;
        assert!(partial.truncated);
        assert_eq!(partial.rows.len(), MAX_QUERY_ROWS);
        assert!(lease.query("SELECT 1").await.is_err());
        assert!(lease
            .query_typed("SELECT $1::int8", &[TypedValue::Integer("1".into())])
            .await
            .is_err());
        assert!(lease.execute("SELECT 1").await.is_err());
        assert!(old_cancel.cancel().await.is_err());
        lease.release().await.map_err(|_| "隔离租约归还失败")?;

        let mut next = driver
            .lease_session(None)
            .await
            .map_err(|_| "新租约创建失败")?;
        let new_backend = number(
            &next
                .query("SELECT pg_backend_pid()")
                .await
                .map_err(|_| "新物理会话读取失败")?,
        )?;
        assert_ne!(old_backend, new_backend);
        assert!(old_cancel.cancel().await.is_err());
        assert_eq!(
            number(
                &next
                    .query("SELECT 42")
                    .await
                    .map_err(|_| "旧取消影响新会话")?
            )?,
            42
        );
        next.release().await.map_err(|_| "新租约归还失败")?;
    }
    driver.disconnect().await.map_err(|_| "断开失败".into())
}

#[tokio::test]
#[ignore = "需要 SQLINK_TEST_POSTGRES 指向独立 PostgreSQL 测试库"]
async fn postgres_byte_budget_and_oversized_cells_keep_typed_rows_aligned() -> TestResult {
    let driver = connect().await?;
    let sql = "SELECT repeat('x', 65536) AS payload FROM generate_series(1, 1000)";
    let plain = driver
        .query(sql, None, None)
        .await
        .map_err(|_| "普通字节预算查询失败")?;
    assert!(plain.truncated);
    assert!(!plain.rows.is_empty() && plain.rows.len() < 1000);
    assert!(
        serde_json::to_vec(&plain)
            .map_err(|_| "结果序列化失败")?
            .len()
            <= MAX_QUERY_BYTES
    );
    let typed = driver
        .query_typed(sql, &[], None)
        .await
        .map_err(|_| "类型化字节预算查询失败")?;
    assert!(typed.result.truncated);
    assert!(!typed.result.rows.is_empty() && typed.result.rows.len() <= plain.rows.len());
    assert_eq!(typed.typed_rows.len(), typed.result.rows.len());
    assert!(
        serde_json::to_vec(
            &serde_json::json!({"result": &typed.result, "typedRows": &typed.typed_rows})
        )
        .map_err(|_| "类型化结果序列化失败")?
        .len()
            <= MAX_QUERY_BYTES
    );

    for expression in [
        format!("repeat('x', {})", MAX_QUERY_CELL_BYTES + 1),
        format!("decode(repeat('ff', {}), 'hex')", MAX_QUERY_CELL_BYTES + 1),
    ] {
        let sql = format!("SELECT {expression} AS oversized");
        let plain = driver
            .query(&sql, None, None)
            .await
            .map_err(|_| "超大单元格查询失败")?;
        assert!(plain.truncated && plain.rows.is_empty());
        assert_eq!(plain.columns.len(), 1);
        let typed = driver
            .query_typed(&sql, &[], None)
            .await
            .map_err(|_| "类型化超大单元格查询失败")?;
        assert!(
            typed.result.truncated && typed.result.rows.is_empty() && typed.typed_rows.is_empty()
        );
    }
    assert_eq!(
        number(
            &driver
                .query("SELECT 7", None, None)
                .await
                .map_err(|_| "截断后新查询失败")?
        )?,
        7
    );
    driver.disconnect().await.map_err(|_| "断开失败".into())
}
