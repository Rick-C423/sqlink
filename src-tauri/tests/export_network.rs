//! 流式导出的真实网络回归；仅使用显式配置的独立测试库，随机表在结束时清理。
//! 默认忽略；显式运行必须设置 SQLINK_TEST_POSTGRES 或 SQLINK_TEST_MSSQL。
use reqwest::Url;
use sqlconnect_lib::database::{create_and_connect_driver, DatabaseDriver};
use sqlconnect_lib::export::{ExportColumn, ExportSink};
use sqlconnect_lib::models::{ConnectionConfig, DatabaseError};
use sqlconnect_lib::typed_value::{TimeValue, TypedValue};
use std::sync::{Arc, Mutex};
use uuid::Uuid;

type TestResult<T = ()> = Result<T, String>;
fn require(condition: bool, message: &str) -> TestResult {
    if condition {
        Ok(())
    } else {
        Err(message.into())
    }
}
fn decode_component(encoded: &str) -> TestResult<String> {
    let mut decoded = Vec::new();
    let mut bytes = encoded.bytes();
    while let Some(byte) = bytes.next() {
        if byte == b'%' {
            let high = bytes.next().and_then(|byte| (byte as char).to_digit(16));
            let low = bytes.next().and_then(|byte| (byte as char).to_digit(16));
            match (high, low) {
                (Some(high), Some(low)) => decoded.push((high * 16 + low) as u8),
                _ => return Err("连接串含非法百分号编码".into()),
            }
        } else {
            decoded.push(byte);
        }
    }
    String::from_utf8(decoded).map_err(|_| "连接串字段不是有效 UTF-8".into())
}

fn config_from_env(driver: &str, variable: &str, id: &str) -> TestResult<ConnectionConfig> {
    let raw =
        std::env::var(variable).map_err(|_| format!("必须设置 {variable} 才能显式运行此测试"))?;
    // 不输出原始连接串或 URL 解析错误，避免失败日志泄露凭据。
    let url = Url::parse(&raw).map_err(|_| format!("{variable} 不是有效测试连接串"))?;
    let valid_scheme = match driver {
        "postgres" => matches!(url.scheme(), "postgres" | "postgresql"),
        "mssql" => matches!(url.scheme(), "mssql" | "sqlserver"),
        _ => url.scheme() == driver,
    };
    require(valid_scheme, "测试连接串协议与驱动不符")?;
    require(url.fragment().is_none(), "测试连接串不支持片段")?;
    let host = url.host_str().ok_or("测试连接串缺少主机")?;
    let database = decode_component(url.path().trim_start_matches('/'))?;
    require(!database.is_empty(), "测试连接串必须显式指定数据库")?;
    let mut encrypt = None;
    for (name, value) in url.query_pairs() {
        match (driver, name.as_ref(), value.as_ref()) {
            ("mssql", "encrypt", "true") => encrypt = Some(true),
            ("mssql", "encrypt", "false") => encrypt = Some(false),
            _ => return Err("测试连接串包含不支持的参数".into()),
        }
    }
    Ok(ConnectionConfig {
        id: Some(id.into()),
        name: format!("受控结果网络测试 {driver}"),
        driver_type: driver.into(),
        host: host.trim_start_matches('[').trim_end_matches(']').into(),
        port: url.port().unwrap_or(match driver {
            "postgres" => 5432,
            "mssql" => 1433,
            _ => 3306,
        }),
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

#[derive(Default)]
struct CountingSink {
    columns: Mutex<Vec<String>>,
    expected: Vec<TypedValue>,
    rows: Mutex<u64>,
    reject_row: bool,
}
impl ExportSink for CountingSink {
    fn columns(&self, columns: &[ExportColumn]) -> Result<(), DatabaseError> {
        *self.columns.lock().unwrap() = columns.iter().map(|column| column.name.clone()).collect();
        Ok(())
    }
    fn row(&self, row: &[TypedValue]) -> Result<(), DatabaseError> {
        if self.reject_row {
            return Err(DatabaseError::QueryError("测试 sink 主动停止".into()));
        }
        if row != self.expected {
            return Err(DatabaseError::TypeError("导出 typed 行与原始值不符".into()));
        }
        *self.rows.lock().unwrap() += 1;
        Ok(())
    }
    fn check_cancelled(&self) -> Result<(), DatabaseError> {
        Ok(())
    }
}

fn expected_values(kind: &str) -> Vec<TypedValue> {
    vec![
        TypedValue::Integer("9007199254740993".into()),
        TypedValue::Decimal("12345678901234567890.123456789012345678".into()),
        TypedValue::binary(&[0, 255, 16]),
        TypedValue::Null,
        TypedValue::Text("中文,\"引号\"\n第二行".into()),
        TypedValue::Time(TimeValue {
            value: if kind == "postgres" {
                "12:34:56.123456"
            } else {
                "12:34:56.1234567"
            }
            .into(),
            precision: if kind == "postgres" { 6 } else { 7 },
        }),
    ]
}

async fn check_stream_failures(
    driver: &dyn DatabaseDriver,
    database: &str,
    kind: &str,
) -> TestResult {
    let unsupported = if kind == "postgres" {
        "SELECT '{}'::jsonb"
    } else {
        "SELECT CAST('12345678-1234-1234-1234-123456789012' AS uniqueidentifier)"
    };
    let oversized = if kind == "postgres" {
        "SELECT repeat('x', 1048577)"
    } else {
        "SELECT REPLICATE(CAST(N'x' AS nvarchar(max)), 1048577)"
    };
    for (sql, expected_code) in [
        (unsupported, Some("EXPORT_TYPE_UNSUPPORTED")),
        ("SELECT 1; SELECT 2", None),
        (oversized, None),
    ] {
        let mut lease = driver
            .lease_session(Some(database))
            .await
            .map_err(|_| "获取错误检查租约失败")?;
        let sink = Arc::new(CountingSink {
            expected: vec![TypedValue::Integer("1".into())],
            ..Default::default()
        });
        let result = lease.stream_export(sql, sink).await;
        let error = result
            .err()
            .ok_or("不支持的导出必须显式失败，不能截断成功")?;
        if let Some(code) = expected_code {
            require(
                error.to_string().contains(code),
                "不支持类型必须返回 EXPORT_TYPE_UNSUPPORTED",
            )?;
        }
        require(
            lease.query("SELECT 1").await.is_err(),
            "提前失败必须隔离当前租约",
        )?;
        lease.release().await.map_err(|_| "释放错误检查租约失败")?;
    }
    Ok(())
}

async fn run_export_case(kind: &str, variable: &str) -> TestResult {
    let config = config_from_env(kind, variable, &format!("export-{kind}"))?;
    let database = config.database.as_deref().ok_or("必须明确指定测试库")?;
    let driver = create_and_connect_driver(&config)
        .await
        .map_err(|_| "连接专用测试库失败")?;
    let table = format!("sqlink_export_{}", Uuid::new_v4().simple());
    let restored = format!("sqlink_restore_{}", Uuid::new_v4().simple());
    let quoted = |name: &str| {
        if kind == "postgres" {
            format!("\"{name}\"")
        } else {
            format!("[{name}]")
        }
    };
    let path = std::env::temp_dir().join(format!("{table}.csv"));
    let empty_path = std::env::temp_dir().join(format!("{table}_empty.csv"));
    let result = async {
        let definition = if kind == "postgres" {
            "big_value bigint, decimal_value numeric(38,18), binary_value bytea, null_value integer, unicode_value text, time_value time(6)"
        } else {
            "big_value bigint, decimal_value decimal(38,18), binary_value varbinary(3), null_value int, unicode_value nvarchar(100), time_value time(7)"
        };
        driver.execute(&format!("CREATE TABLE {} ({definition})", quoted(&table)), Some(database), None)
            .await.map_err(|_| "创建随机测试表失败")?;
        let empty_count = sqlconnect_lib::export::export_csv(
            driver.as_ref(), Some(database), &table, empty_path.to_str().ok_or("测试文件路径无效")?,
        ).await.map_err(|_| "空表 CSV 导出失败")?;
        require(empty_count == 0, "空表导出行数必须为零")?;
        let empty_csv = std::fs::read_to_string(&empty_path).map_err(|_| "读取空表 CSV 失败")?;
        require(empty_csv.contains("big_value") && empty_csv.contains("time_value") && empty_csv.lines().count() == 1,
            "空表必须保留完整表头")?;

        let insert = if kind == "postgres" {
            format!("INSERT INTO {} SELECT 9007199254740993::bigint, 12345678901234567890.123456789012345678::numeric(38,18), decode('00ff10','hex'), NULL::integer, E'中文,\"引号\"\\n第二行', '12:34:56.123456'::time(6) FROM generate_series(1,10001)", quoted(&table))
        } else {
            format!("WITH q(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM q WHERE n < 10001) INSERT INTO {} SELECT CAST(9007199254740993 AS bigint), CAST(12345678901234567890.123456789012345678 AS decimal(38,18)), 0x00ff10, NULL, N'中文,\"引号\"' + NCHAR(10) + N'第二行', CAST('12:34:56.1234567' AS time(7)) FROM q OPTION(MAXRECURSION 0)", quoted(&table))
        };
        driver.execute(&insert, Some(database), None).await.map_err(|_| "插入流式导出测试数据失败")?;
        let count = sqlconnect_lib::export::export_csv(
            driver.as_ref(), Some(database), &table, path.to_str().ok_or("测试文件路径无效")?,
        ).await.map_err(|_| "CSV 流式导出失败")?;
        require(count == 10001, "CSV 导出不能套用交互查询 10000 行限制")?;
        let csv = std::fs::read_to_string(&path).map_err(|_| "读取 CSV 失败")?;
        require(!csv.starts_with('\u{feff}'), "CSV 约定为无 BOM 的 UTF-8")?;
        let (_, body) = csv.split_once('\n').ok_or("CSV 缺少表头换行")?;
        let time = if kind == "postgres" { "12:34:56.123456" } else { "12:34:56.1234567" };
        let expected = format!("9007199254740993,12345678901234567890.123456789012345678,\"\\x00ff10\",\\N,\"中文,\"\"引号\"\"\n第二行\",\"{time}\"\n");
        require(body == expected.repeat(10001), "CSV 必须逐行保留大整数、decimal、binary、NULL、Unicode 和时间精度")?;

        let expected = expected_values(kind);
        let mut lease = driver.lease_session(Some(database)).await.map_err(|_| "获取导出租约失败")?;
        let sink = Arc::new(CountingSink { expected: expected.clone(), ..Default::default() });
        let count = lease.stream_export(&format!("SELECT * FROM {}", quoted(&table)), sink.clone()).await
            .map_err(|_| "原始 typed 流式检查失败")?;
        require(count == 10001 && *sink.rows.lock().unwrap() == 10001, "typed 流必须完整写入 10001 行")?;
        require(sink.columns.lock().unwrap().len() == 6, "typed 流必须保留全部列")?;
        lease.query("SELECT 1").await.map_err(|_| "完整消费后租约必须可复用")?;
        lease.release().await.map_err(|_| "完整租约释放失败")?;

        // 兼容 String wrapper 的 SQL dump 也必须走 typed 数据流；在另一随机表实际回放。
        let (dump, count) = driver.dump_table(Some(database), &table, false, true).await
            .map_err(|_| "SQL dump 失败")?;
        require(count == 10001, "SQL dump 不能套用交互查询行数限制")?;
        let restored_dump = dump.replace(&quoted(&table), &quoted(&restored));
        let mut splitter = sqlconnect_lib::database::dump::SqlSplitter::new(false);
        let mut reader = std::io::BufReader::new(restored_dump.as_bytes());
        let mut replay = driver.lease_session(Some(database)).await.map_err(|_| "获取 SQL 回放租约失败")?;
        while let Some(statement) = splitter.next_statement(&mut reader).map_err(|_| "分割 SQL dump 失败")? {
            replay.execute(&statement).await.map_err(|_| "回放 SQL dump 失败")?;
        }
        replay.release().await.map_err(|_| "释放 SQL 回放租约失败")?;
        let mut lease = driver.lease_session(Some(database)).await.map_err(|_| "获取回放验证租约失败")?;
        let sink = Arc::new(CountingSink { expected, ..Default::default() });
        let restored_count = lease.stream_export(&format!("SELECT * FROM {}", quoted(&restored)), sink).await
            .map_err(|_| "SQL dump 回放未保留原始 typed 值")?;
        require(restored_count == 10001, "SQL dump 回放必须保留全部行")?;
        lease.release().await.map_err(|_| "回放租约释放失败")?;

        let mut lease = driver.lease_session(Some(database)).await.map_err(|_| "获取失败检查租约失败")?;
        let sink = Arc::new(CountingSink { reject_row: true, ..Default::default() });
        require(lease.stream_export(&format!("SELECT * FROM {}", quoted(&table)), sink).await.is_err(), "sink 失败必须中止导出")?;
        require(lease.query("SELECT 1").await.is_err(), "sink 失败后租约必须隔离")?;
        lease.release().await.map_err(|_| "失败租约释放失败")?;
        check_stream_failures(driver.as_ref(), database, kind).await?;
        Ok::<_, String>(())
    }.await;
    // 无论断言是否成功都先清理，且只删除本次生成的随机表和文件。
    let cleanup_restored = driver
        .execute(
            &format!("DROP TABLE IF EXISTS {}", quoted(&restored)),
            Some(database),
            None,
        )
        .await;
    let cleanup_source = driver
        .execute(
            &format!("DROP TABLE IF EXISTS {}", quoted(&table)),
            Some(database),
            None,
        )
        .await;
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&empty_path);
    let disconnected = driver.disconnect().await;
    result?;
    cleanup_restored.map_err(|_| "清理回放随机表失败")?;
    cleanup_source.map_err(|_| "清理源随机表失败")?;
    disconnected.map_err(|_| "断开专用测试连接失败")?;
    Ok(())
}

#[tokio::test]
#[ignore = "需要 SQLINK_TEST_POSTGRES 指向独立 PostgreSQL 测试库"]
async fn postgres_stream_csv_and_dump_preserve_values_above_query_limit() -> TestResult {
    run_export_case("postgres", "SQLINK_TEST_POSTGRES").await
}

#[tokio::test]
#[ignore = "需要 SQLINK_TEST_MSSQL 指向独立 SQL Server 测试库"]
async fn mssql_stream_csv_and_dump_preserve_values_above_query_limit() -> TestResult {
    run_export_case("mssql", "SQLINK_TEST_MSSQL").await
}
