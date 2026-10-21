//! 显式运行需要 SQLINK_TEST_MYSQL=mysql://user:password@host:port/database。
//! 测试只创建并清理本次随机命名的夹具表，不输出连接凭据。
use reqwest::Url;
use sqlconnect_lib::database::{mysql::MySqlDriver, DatabaseDriver};
use sqlconnect_lib::models::ConnectionConfig;
use sqlconnect_lib::typed_value::{TimeValue, TimestampValue, TimezoneKind, TypedValue};

fn decode(value: &str) -> Result<String, String> {
    let mut decoded = Vec::new();
    let mut bytes = value.bytes();
    while let Some(byte) = bytes.next() {
        if byte == b'%' {
            let high = bytes.next().and_then(|b| (b as char).to_digit(16));
            let low = bytes.next().and_then(|b| (b as char).to_digit(16));
            match (high, low) {
                (Some(high), Some(low)) => decoded.push((high * 16 + low) as u8),
                _ => return Err("连接串百分号编码无效".into()),
            }
        } else {
            decoded.push(byte);
        }
    }
    String::from_utf8(decoded).map_err(|_| "连接串字段不是 UTF-8".into())
}

fn config() -> Result<ConnectionConfig, String> {
    let raw = std::env::var("SQLINK_TEST_MYSQL")
        .map_err(|_| "显式运行必须设置 SQLINK_TEST_MYSQL".to_string())?;
    let url = Url::parse(&raw).map_err(|_| "MySQL 测试连接串无效".to_string())?;
    if url.scheme() != "mysql" || url.query().is_some() || url.fragment().is_some() {
        return Err("MySQL 测试连接串协议或附加参数无效".into());
    }
    let database = decode(url.path().trim_start_matches('/'))?;
    if database.is_empty() {
        return Err("必须指定独立测试数据库".into());
    }
    Ok(ConnectionConfig {
        id: None,
        name: "MySQL 类型化回环".into(),
        driver_type: "mysql".into(),
        host: url
            .host_str()
            .ok_or("缺少主机")?
            .trim_start_matches('[')
            .trim_end_matches(']')
            .into(),
        port: url.port().unwrap_or(3306),
        user: decode(url.username())?,
        password: decode(url.password().unwrap_or_default())?,
        database: Some(database),
        encrypt: None,
        save_password: None,
        group_name: None,
        color: None,
        mcp_allowed: None,
    })
}

async fn check(driver: &MySqlDriver, table: &str) -> Result<(), String> {
    let params = vec![
        TypedValue::Integer("9007199254740993".into()),
        TypedValue::UnsignedInteger(u64::MAX.to_string()),
        TypedValue::Decimal("12345678901234567890.12345678901234567890".into()),
        TypedValue::Text("'); DROP TABLE anything; -- 中文😀'\"\\\0".into()),
        TypedValue::binary(&[0, 255, 0, 39, 92]),
        TypedValue::Null,
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
    let insert = format!("INSERT INTO `{table}` (signed_value,unsigned_value,exact_value,text_value,binary_value,null_value,date_value,time_value,datetime_value) VALUES (?,?,?,?,?,?,?,?,?)");
    if driver
        .execute_bound(&insert, &params[..params.len() - 1], None)
        .await
        .is_ok()
    {
        return Err("参数数量不足应失败".into());
    }
    if driver
        .execute_bound(&insert, &params, None)
        .await
        .map_err(|_| "类型化 INSERT 失败")?
        != 1
    {
        return Err("INSERT 影响行数错误".into());
    }
    let result = driver.query_typed(&format!("SELECT signed_value,unsigned_value,exact_value,text_value,binary_value,null_value,date_value,time_value,datetime_value FROM `{table}` WHERE signed_value=?"), &[params[0].clone()], None).await.map_err(|_| "类型化 SELECT 失败")?;
    if result.typed_rows != vec![params.iter().cloned().map(Some).collect::<Vec<_>>()] {
        return Err("MySQL 类型化值未无损回环".into());
    }
    if result.result.rows[0].values
        != params
            .iter()
            .map(|v| Some(v.display_json()))
            .collect::<Vec<_>>()
    {
        return Err("显示值和类型化值未对齐".into());
    }
    let extra = driver.query_typed(&format!("SELECT unknown_bits, x'00FF', CAST('NULL' AS CHAR), CAST('' AS CHAR), x'' FROM `{table}`"), &[], None).await.map_err(|_| "特殊值 SELECT 失败")?;
    if extra.typed_rows[0]
        != vec![
            None,
            Some(TypedValue::binary(&[0, 255])),
            Some(TypedValue::Text("NULL".into())),
            Some(TypedValue::Text("".into())),
            Some(TypedValue::binary(&[])),
        ]
    {
        return Err("BIT/文本/二进制类型识别错误".into());
    }
    let multiple =
        format!("INSERT INTO `{table}` (signed_value) VALUES (?); DELETE FROM `{table}`");
    if driver
        .execute_bound(&multiple, &[TypedValue::Integer("1".into())], None)
        .await
        .is_ok()
    {
        return Err("多语句不应执行".into());
    }
    let count = driver
        .query_typed(&format!("SELECT COUNT(*) FROM `{table}`"), &[], None)
        .await
        .map_err(|_| "行数核对失败")?;
    if count.typed_rows[0][0]
        .as_ref()
        .map(TypedValue::display_json)
        != Some(serde_json::json!(1))
    {
        return Err("失败的绑定或多语句产生副作用".into());
    }
    Ok(())
}

#[tokio::test]
#[ignore = "需要 SQLINK_TEST_MYSQL 指定真实测试数据库"]
async fn mysql_native_binding_round_trip() {
    let config = config().unwrap_or_else(|message| panic!("{message}"));
    let driver = MySqlDriver::connect(&config)
        .await
        .unwrap_or_else(|_| panic!("MySQL 测试连接失败"));
    let table = format!("sqlink_typed_{}", uuid::Uuid::new_v4().simple());
    let create = format!("CREATE TABLE `{table}` (signed_value BIGINT, unsigned_value BIGINT UNSIGNED, exact_value DECIMAL(50,20), text_value LONGTEXT CHARACTER SET utf8mb4, binary_value LONGBLOB, null_value TEXT, date_value DATE, time_value TIME(6), datetime_value DATETIME(6), unknown_bits BIT(1) DEFAULT b'1')");
    let created = driver.execute_bound(&create, &[], None).await;
    if created.is_err() {
        let _ = driver.disconnect().await;
        panic!("创建 MySQL 测试夹具失败");
    }
    let result = check(&driver, &table).await;
    let cleanup = driver
        .execute_bound(&format!("DROP TABLE `{table}`"), &[], None)
        .await;
    let disconnected = driver.disconnect().await;
    assert!(cleanup.is_ok(), "清理 MySQL 测试夹具失败");
    assert!(disconnected.is_ok(), "关闭 MySQL 测试连接失败");
    assert!(result.is_ok(), "{}", result.err().unwrap_or_default());
}
