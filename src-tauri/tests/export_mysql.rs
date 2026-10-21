//! 仅在显式指定的独立 MySQL 测试库创建随机夹具；不读取应用保存连接。
use sqlconnect_lib::{
    database::{mysql::MySqlDriver, DatabaseDriver},
    export::{export_csv, ExportColumn, ExportSink},
    models::{ConnectionConfig, DatabaseError},
    typed_value::{TimeValue, TimestampValue, TimezoneKind, TypedValue},
};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, Mutex,
};

type TestResult<T = ()> = Result<T, String>;

async fn driver() -> TestResult<MySqlDriver> {
    let raw = std::env::var("SQLINK_TEST_MYSQL")
        .map_err(|_| "显式运行必须设置 SQLINK_TEST_MYSQL 独立测试库")?;
    let opts = mysql_async::Opts::from_url(&raw).map_err(|_| "MySQL 测试连接串无效")?;
    let database = opts
        .db_name()
        .filter(|name| !name.is_empty())
        .ok_or("必须明确指定独立测试数据库")?;
    let config: ConnectionConfig = serde_json::from_value(serde_json::json!({
        "name":"MySQL流式导出回归", "driverType":"mysql", "host":opts.ip_or_hostname(),
        "port":opts.tcp_port(), "user":opts.user().unwrap_or(""),
        "password":opts.pass().unwrap_or(""), "database":database, "savePassword":false,
    }))
    .map_err(|_| "测试配置构造失败")?;
    MySqlDriver::connect(&config)
        .await
        .map_err(|_| "独立测试库连接失败".into())
}

#[derive(Default)]
struct Capture {
    columns: Mutex<Vec<String>>,
    first: Mutex<Option<Vec<TypedValue>>>,
    count: AtomicUsize,
    fail_write: bool,
    cancel_after_row: bool,
    cancelled: AtomicBool,
}

impl ExportSink for Capture {
    fn columns(&self, columns: &[ExportColumn]) -> Result<(), DatabaseError> {
        *self.columns.lock().unwrap() = columns.iter().map(|c| c.name.clone()).collect();
        Ok(())
    }
    fn row(&self, row: &[TypedValue]) -> Result<(), DatabaseError> {
        if self.fail_write {
            return Err(DatabaseError::Unknown("模拟写入失败".into()));
        }
        if self.count.fetch_add(1, Ordering::SeqCst) == 0 {
            *self.first.lock().unwrap() = Some(row.to_vec());
        }
        if self.cancel_after_row {
            self.cancelled.store(true, Ordering::SeqCst);
        }
        Ok(())
    }
    fn check_cancelled(&self) -> Result<(), DatabaseError> {
        if self.cancelled.load(Ordering::SeqCst) {
            Err(DatabaseError::QueryCancelled)
        } else {
            Ok(())
        }
    }
}

fn numbers(end: usize) -> String {
    let digits = "(SELECT 0 d UNION ALL SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3 UNION ALL SELECT 4 UNION ALL SELECT 5 UNION ALL SELECT 6 UNION ALL SELECT 7 UNION ALL SELECT 8 UNION ALL SELECT 9)";
    format!("SELECT a.d*1000+b.d*100+c.d*10+d.d+1 AS n FROM {digits} a CROSS JOIN {digits} b CROSS JOIN {digits} c CROSS JOIN {digits} d ORDER BY n LIMIT {end}")
}

/// 用测试自己的 CSV 记录读取器核对嵌入换行、双引号和记录数。
fn csv_records(csv: &str) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut chars = csv.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '"' if quoted && chars.peek() == Some(&'"') => {
                chars.next();
                field.push('"');
            }
            '"' => quoted = !quoted,
            ',' if !quoted => row.push(std::mem::take(&mut field)),
            '\n' if !quoted => {
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
            }
            _ => field.push(ch),
        }
    }
    assert!(!quoted, "CSV 引号不闭合");
    assert!(
        field.is_empty() && row.is_empty(),
        "CSV 最后记录必须以 LF 结束"
    );
    rows
}

async fn check_fixture(driver: &MySqlDriver, table: &str, empty: &str, path: &str) -> TestResult {
    let text = "中文😀,\"引号\"\n第二行\\\0";
    let values = vec![
        TypedValue::Integer("1".into()),
        TypedValue::Integer(i64::MIN.to_string()),
        TypedValue::UnsignedInteger(u64::MAX.to_string()),
        TypedValue::Decimal("12345678901234567890.12345678901234567890".into()),
        TypedValue::Text(text.into()),
        TypedValue::binary(&[0, 255, 39, 92]),
        TypedValue::Null,
        TypedValue::Text(String::new()),
        TypedValue::Text("\\N".into()),
        TypedValue::Date("2026-10-20".into()),
        TypedValue::Time(TimeValue {
            value: "12:34:56.123456".into(),
            precision: 6,
        }),
        TypedValue::Timestamp(TimestampValue {
            value: "2026-10-20T12:34:56.123456".into(),
            precision: 6,
            timezone: TimezoneKind::None,
        }),
    ];
    driver
        .execute_bound(
            &format!("INSERT INTO `{table}` VALUES (?,?,?,?,?,?,?,?,?,?,?,?)"),
            &values,
            None,
        )
        .await
        .map_err(|_| "插入精确值失败")?;
    let all_columns = "signed_value,unsigned_value,exact_value,text_value,binary_value,null_value,empty_value,slash_n,date_value,time_value,datetime_value";
    driver.execute_bound(&format!("INSERT INTO `{table}` SELECT n+1,{all_columns} FROM ({}) seq CROSS JOIN `{table}` source WHERE source.id=1", numbers(10000)), &[], None)
        .await.map_err(|_| "创建超过一万行夹具失败")?;

    let mut lease = driver
        .lease_session(None)
        .await
        .map_err(|_| "导出租约失败")?;
    let capture = Arc::new(Capture::default());
    let count = lease
        .stream_export(
            &format!("SELECT * FROM `{table}` ORDER BY id"),
            capture.clone(),
        )
        .await
        .map_err(|_| "原生类型流式导出失败")?;
    assert_eq!(count, 10001);
    assert_eq!(capture.count.load(Ordering::SeqCst), 10001);
    assert_eq!(*capture.first.lock().unwrap(), Some(values));
    lease
        .query("SELECT 1")
        .await
        .map_err(|_| "完整读取后租约不能继续使用")?;
    let empty_capture = Arc::new(Capture::default());
    assert_eq!(
        lease
            .stream_export(
                &format!("SELECT * FROM `{table}` WHERE 1=0"),
                empty_capture.clone()
            )
            .await
            .map_err(|_| "空结果流式导出失败")?,
        0
    );
    assert_eq!(empty_capture.columns.lock().unwrap().len(), 12);
    assert_eq!(empty_capture.count.load(Ordering::SeqCst), 0);
    lease.release().await.map_err(|_| "导出租约归还失败")?;

    assert_eq!(
        export_csv(driver, None, table, path)
            .await
            .map_err(|_| "CSV 导出失败")?,
        10001
    );
    let csv = std::fs::read_to_string(path).map_err(|_| "CSV 读取失败")?;
    assert!(!csv.starts_with('\u{feff}'));
    let rows = csv_records(&csv);
    assert_eq!(rows.len(), 10002);
    assert_eq!(rows[0].len(), 12);
    let first = rows
        .iter()
        .skip(1)
        .find(|r| r[0] == "1")
        .ok_or("CSV 缺少第一行")?;
    assert_eq!(first[1], i64::MIN.to_string());
    assert_eq!(first[2], u64::MAX.to_string());
    assert_eq!(first[3], "12345678901234567890.12345678901234567890");
    assert_eq!(first[4], text);
    assert_eq!(first[5], "\\x00ff275c");
    assert_eq!(first[6], "\\N");
    assert_eq!(first[7], "");
    assert_eq!(first[8], "\\N");
    assert_eq!(first[9], "2026-10-20");
    assert_eq!(first[10], "12:34:56.123456");
    assert_eq!(first[11], "2026-10-20T12:34:56.123456");
    assert!(
        csv.contains(",\\N,\"\",\"\\N\","),
        "SQL NULL、空串和字面量空值标记必须区分"
    );
    assert_eq!(
        export_csv(driver, None, empty, path)
            .await
            .map_err(|_| "空表 CSV 导出失败")?,
        0
    );
    assert_eq!(
        csv_records(&std::fs::read_to_string(path).unwrap()).len(),
        1
    );
    // 恢复有效目标，再证明超限导出不会覆盖已有文件。
    std::fs::write(path, csv.as_bytes()).map_err(|_| "准备原子覆盖目标失败")?;
    driver
        .execute_bound(
            &format!("UPDATE `{table}` SET text_value=REPEAT('x',1048577) WHERE id=1"),
            &[],
            None,
        )
        .await
        .map_err(|_| "准备超大单元格失败")?;
    assert!(export_csv(driver, None, table, path).await.is_err());
    assert_eq!(std::fs::read(path).unwrap(), csv.as_bytes());
    driver
        .execute_bound(
            &format!("ALTER TABLE `{table}` ADD unknown_bits BIT(1) DEFAULT b'1'"),
            &[],
            None,
        )
        .await
        .map_err(|_| "准备未知类型失败")?;
    let mut lease = driver
        .lease_session(None)
        .await
        .map_err(|_| "未知类型租约失败")?;
    let error = lease
        .stream_export(
            &format!("SELECT unknown_bits FROM `{table}`"),
            Arc::new(Capture::default()),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("EXPORT_TYPE_UNSUPPORTED"));
    assert!(lease.query("SELECT 1").await.is_err());
    lease.release().await.map_err(|_| "未知类型租约归还失败")?;
    Ok(())
}

#[tokio::test]
#[ignore = "需要 SQLINK_TEST_MYSQL 指定独立真实测试数据库"]
async fn mysql_export_preserves_native_values_and_more_than_ten_thousand_rows() -> TestResult {
    let driver = driver().await?;
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let table = format!("sqlink_export_{suffix}");
    let empty = format!("sqlink_empty_{suffix}");
    let directory = std::env::temp_dir().join(format!("sqlink_export_{suffix}"));
    std::fs::create_dir(&directory).map_err(|_| "创建导出临时目录失败")?;
    let path = directory.join("data.csv");
    let result = async {
        driver.execute_bound(&format!("CREATE TABLE `{table}` (id BIGINT PRIMARY KEY, signed_value BIGINT, unsigned_value BIGINT UNSIGNED, exact_value DECIMAL(50,20), text_value LONGTEXT CHARACTER SET utf8mb4, binary_value LONGBLOB, null_value TEXT, empty_value TEXT, slash_n TEXT, date_value DATE, time_value TIME(6), datetime_value DATETIME(6))"), &[], None)
            .await.map_err(|_| "创建 MySQL 导出夹具失败")?;
        driver.execute_bound(&format!("CREATE TABLE `{empty}` LIKE `{table}`"), &[], None)
            .await.map_err(|_| "创建空表夹具失败")?;
        check_fixture(&driver, &table, &empty, path.to_str().ok_or("临时目录不是 UTF-8")?).await
    }.await;
    let cleanup = driver
        .execute_bound(
            &format!("DROP TABLE IF EXISTS `{table}`,`{empty}`"),
            &[],
            None,
        )
        .await;
    let disconnected = driver.disconnect().await;
    if path.exists() {
        std::fs::remove_file(&path).map_err(|_| "清理 CSV 失败")?;
    }
    assert_eq!(
        std::fs::read_dir(&directory).unwrap().count(),
        0,
        "不能遗留原子导出临时文件"
    );
    std::fs::remove_dir(&directory).map_err(|_| "清理临时目录失败")?;
    assert!(cleanup.is_ok(), "清理 MySQL 导出夹具失败");
    assert!(disconnected.is_ok(), "关闭测试驱动失败");
    result
}

#[tokio::test]
#[ignore = "需要 SQLINK_TEST_MYSQL 指定独立真实测试数据库"]
async fn mysql_export_errors_cancel_and_raw_limits_quarantine_lease() -> TestResult {
    let driver = driver().await?;
    for sink in [
        Capture {
            fail_write: true,
            ..Capture::default()
        },
        Capture {
            cancel_after_row: true,
            ..Capture::default()
        },
        Capture {
            cancelled: AtomicBool::new(true),
            ..Capture::default()
        },
    ] {
        let mut lease = driver
            .lease_session(None)
            .await
            .map_err(|_| "失败路径租约创建失败")?;
        let cancellation = lease.cancel_handle().ok_or("缺少取消句柄")?;
        assert!(lease
            .stream_export(&numbers(10000), Arc::new(sink))
            .await
            .is_err());
        assert!(lease.query("SELECT 1").await.is_err());
        assert!(cancellation.cancel().await.is_err());
        lease.release().await.map_err(|_| "失败路径租约归还失败")?;
    }
    let mut lease = driver
        .lease_session(None)
        .await
        .map_err(|_| "单元格边界租约失败")?;
    let capture = Arc::new(Capture::default());
    assert_eq!(
        lease
            .stream_export("SELECT REPEAT('x',1048576) AS payload", capture.clone())
            .await
            .map_err(|_| "恰好 1MiB 单元格应可导出")?,
        1
    );
    assert_eq!(
        *capture.first.lock().unwrap(),
        Some(vec![TypedValue::Text("x".repeat(1048576))])
    );
    lease
        .release()
        .await
        .map_err(|_| "单元格边界租约归还失败")?;
    let wide = (0..17)
        .map(|i| format!("REPEAT('x',1048576) AS c{i}"))
        .collect::<Vec<_>>()
        .join(",");
    for sql in [
        "SELECT REPEAT('x',1048577) AS payload".to_string(),
        format!("SELECT {wide}"),
        format!(
            "SELECT * FROM sqlink_missing_export_{}",
            uuid::Uuid::new_v4().simple()
        ),
    ] {
        let mut lease = driver
            .lease_session(None)
            .await
            .map_err(|_| "预算拒绝租约失败")?;
        assert!(lease
            .stream_export(&sql, Arc::new(Capture::default()))
            .await
            .is_err());
        assert!(lease.query("SELECT 1").await.is_err());
        lease.release().await.map_err(|_| "预算拒绝租约归还失败")?;
    }
    driver
        .disconnect()
        .await
        .map_err(|_| "关闭测试驱动失败".into())
}
