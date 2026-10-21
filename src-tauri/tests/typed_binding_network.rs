//! 类型化读取与原生绑定的真实网络回环测试；只读取 SQLINK_TEST_* 专用测试连接。
//! 所有检查以 Result 返回，先清理随机表和连接，再报告失败；不会读取生产默认配置。
use reqwest::Url;
use sqlconnect_lib::database::{create_and_connect_driver, DatabaseDriver};
use sqlconnect_lib::models::ConnectionConfig;
use sqlconnect_lib::typed_value::{TimeValue, TimestampValue, TimezoneKind, TypedValue};
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

async fn check_roundtrip(
    driver: &dyn DatabaseDriver,
    kind: &str,
    table: &str,
    database: &str,
) -> TestResult {
    let placeholder = |n| {
        if kind == "postgres" {
            format!("${n}")
        } else {
            format!("@P{n}")
        }
    };
    let insert = format!("INSERT INTO {table} (id, exact_decimal, raw_bytes, nullable_text, empty_text, unicode_text) VALUES ({})", (1..=6).map(placeholder).collect::<Vec<_>>().join(", "));
    let unusual = "中文😀 '单引号' \\反斜杠; DROP TABLE x; -- 仍然是数据";
    let decimal = "123456789012345678901.12345678901234567";
    let fixtures = [
        vec![
            TypedValue::Integer("9007199254740993".into()),
            TypedValue::Decimal(decimal.into()),
            TypedValue::binary(&[0, 1, 0, 255]),
            TypedValue::Null,
            TypedValue::Text("".into()),
            TypedValue::Text(unusual.into()),
        ],
        vec![
            TypedValue::Integer(i64::MIN.to_string()),
            TypedValue::Decimal(format!("-{decimal}")),
            TypedValue::binary(&[]),
            TypedValue::Text("NULL".into()),
            TypedValue::Text("".into()),
            TypedValue::Text(unusual.into()),
        ],
    ];
    for fixture in &fixtures {
        let count = driver
            .execute_bound(&insert, fixture, Some(database))
            .await
            .map_err(|_| "原生参数插入失败")?;
        require(count == 1, "单条插入影响行数应为 1")?;
    }
    for fixture in &fixtures {
        let result = driver.query_typed(&format!("SELECT id, exact_decimal, raw_bytes, nullable_text, empty_text, unicode_text FROM {table} WHERE id = {}", placeholder(1)), &fixture[..1], Some(database)).await.map_err(|_| "类型化回读失败")?;
        require(
            result.typed_rows.len() == 1 && result.result.rows.len() == 1,
            "类型化回读行数应为 1",
        )?;
        require(
            result.typed_rows[0] == fixture.iter().cloned().map(Some).collect::<Vec<_>>(),
            "BIGINT、38 位 decimal、二进制、空值或 Unicode 原值不一致",
        )?;
        for (i, value) in fixture.iter().enumerate() {
            require(
                result.result.rows[0].values[i] == Some(value.display_json()),
                "类型值和显示值必须一一对应",
            )?;
        }
    }
    let empty = driver
        .query_typed(
            &format!("SELECT id FROM {table} WHERE 1 = 0"),
            &[],
            Some(database),
        )
        .await
        .map_err(|_| "空结果读取失败")?;
    require(
        empty.result.columns.len() == 1
            && empty.result.rows.is_empty()
            && empty.typed_rows.is_empty(),
        "空结果必须保留列元数据",
    )?;
    let time = TypedValue::Time(TimeValue {
        value: "12:34:56.123456".into(),
        precision: 6,
    });
    let time_sql = if kind == "postgres" {
        "SELECT $1::time(6)"
    } else {
        "SELECT CAST(@P1 AS time(6))"
    };
    let time_result = driver
        .query_typed(time_sql, &[time.clone()], Some(database))
        .await
        .map_err(|_| "时间精度回环失败")?;
    require(
        time_result.typed_rows == vec![vec![Some(time)]],
        "时间小数精度必须保留",
    )?;
    if kind == "postgres" {
        require(
            driver
                .query_typed(
                    "SELECT $1::smallint",
                    &[TypedValue::Integer("32768".into())],
                    Some(database),
                )
                .await
                .is_err(),
            "PostgreSQL 窄整数必须先拒绝越界",
        )?;
        require(
            driver
                .query_typed(
                    "SELECT $1::uuid",
                    &[TypedValue::Text(
                        "00000000-0000-0000-0000-000000000001".into(),
                    )],
                    Some(database),
                )
                .await
                .is_err(),
            "PostgreSQL 不支持参数类型必须显式拒绝",
        )?;
    } else {
        let null_result = driver.query_typed(
            "SELECT CAST(@P1 AS date), CAST(@P2 AS time(7)), CAST(@P3 AS datetime2(7)), CAST(@P4 AS decimal(38,17)), CAST(@P5 AS nvarchar(200)), CAST(@P6 AS varbinary(max))",
            &vec![TypedValue::Null; 6], Some(database),
        ).await.map_err(|_| "SQL Server 日期、时间、decimal、文本、二进制原生 NULL 转换失败")?;
        require(
            null_result.typed_rows == vec![vec![Some(TypedValue::Null); 6]],
            "所有白名单标量 NULL 必须回读为 Some(Null)",
        )?;
        require(
            null_result.result.rows.len() == 1
                && null_result.result.rows[0].values == vec![Some(serde_json::Value::Null); 6],
            "白名单标量 NULL 的显示值必须与类型值一致",
        )?;
        let offset = TypedValue::Timestamp(TimestampValue {
            value: "2026-10-20T12:34:56+08:00".into(),
            precision: 0,
            timezone: TimezoneKind::Offset,
        });
        require(
            driver
                .query_typed("SELECT @P1", &[offset], Some(database))
                .await
                .is_err(),
            "SQL Server 无法保留的时区参数必须显式拒绝",
        )?;
        require(
            driver
                .query_typed(
                    "SELECT @P1",
                    &[TypedValue::Decimal(
                        "0.00000000000000000000000000000000000001".into(),
                    )],
                    Some(database),
                )
                .await
                .is_err(),
            "SQL Server 原生 decimal 不支持精度必须显式拒绝",
        )?;
        require(
            driver
                .execute_bound(
                    &format!("DELETE FROM {table}; DELETE FROM {table}"),
                    &[],
                    Some(database),
                )
                .await
                .is_err(),
            "单条执行必须拒绝显式批语句",
        )?;
    }
    let unsupported_sql = if kind == "postgres" {
        "SELECT '00000000-0000-0000-0000-000000000001'::uuid"
    } else {
        "SELECT CAST('00000000-0000-0000-0000-000000000001' AS uniqueidentifier)"
    };
    let unsupported = driver
        .query_typed(unsupported_sql, &[], Some(database))
        .await
        .map_err(|_| "不支持类型的只读查询失败")?;
    require(
        unsupported.typed_rows == vec![vec![None]],
        "不支持无损解码的类型必须返回 None",
    )?;
    Ok(())
}

async fn exercise(kind: &str, variable: &str) -> TestResult {
    let suffix = Uuid::new_v4().simple().to_string();
    let config = config_from_env(kind, variable, &suffix)?;
    let database = config.database.clone().ok_or("必须指定测试数据库")?;
    let driver = create_and_connect_driver(&config)
        .await
        .map_err(|_| "连接专用测试库失败")?;
    let table = if kind == "postgres" {
        format!("public.sqlink_typed_{suffix}")
    } else {
        format!("dbo.sqlink_typed_{suffix}")
    };
    let (binary, text) = if kind == "postgres" {
        ("BYTEA", "TEXT")
    } else {
        ("VARBINARY(MAX)", "NVARCHAR(MAX)")
    };
    let mut created = false;
    let outcome = async {
        driver.execute(&format!("CREATE TABLE {table} (id BIGINT PRIMARY KEY, exact_decimal DECIMAL(38,17) NOT NULL, raw_bytes {binary} NOT NULL, nullable_text {text} NULL, empty_text {text} NOT NULL, unicode_text {text} NOT NULL)"), Some(&database), None).await.map_err(|_| "创建随机测试表失败")?;
        created = true;
        check_roundtrip(driver.as_ref(), kind, &table, &database).await
    }.await;
    // 不使用 assert!/unwrap 提前 panic，任何执行或验证失败都先走清理。
    let cleanup = if created {
        driver
            .execute(&format!("DROP TABLE {table}"), Some(&database), None)
            .await
            .map(|_| ())
            .map_err(|_| "清理随机测试表失败".to_string())
    } else {
        Ok(())
    };
    let disconnect = driver
        .disconnect()
        .await
        .map_err(|_| "断开测试连接失败".to_string());
    let failures: Vec<_> = [outcome, cleanup, disconnect]
        .into_iter()
        .filter_map(Result::err)
        .collect();
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("；"))
    }
}

#[tokio::test]
#[ignore = "需要 SQLINK_TEST_POSTGRES 指向专用 PostgreSQL 测试库"]
async fn postgres_native_typed_binding_roundtrip() {
    exercise("postgres", "SQLINK_TEST_POSTGRES")
        .await
        .expect("PostgreSQL 真实类型化绑定测试失败");
}

#[tokio::test]
#[ignore = "需要 SQLINK_TEST_MSSQL 指向专用 SQL Server 测试库"]
async fn mssql_native_typed_binding_roundtrip() {
    exercise("mssql", "SQLINK_TEST_MSSQL")
        .await
        .expect("SQL Server 真实类型化绑定测试失败");
}
