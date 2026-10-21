use sqlconnect_lib::database::{sqlite::SqliteDriver, DatabaseDriver};
use sqlconnect_lib::models::{ConnectionConfig, DatabaseError};
use sqlconnect_lib::typed_value::{TimeValue, TimestampValue, TimezoneKind, TypedValue};

async fn driver() -> SqliteDriver {
    SqliteDriver::connect(&ConnectionConfig {
        id: None,
        name: "类型化回环".into(),
        driver_type: "sqlite".into(),
        host: String::new(),
        port: 0,
        user: String::new(),
        password: String::new(),
        database: Some(":memory:".into()),
        encrypt: None,
        save_password: None,
        group_name: None,
        color: None,
        mcp_allowed: None,
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn integers_round_trip_without_javascript_precision_loss() {
    let driver = driver().await;
    driver
        .execute_bound("CREATE TABLE samples(value INTEGER)", &[], None)
        .await
        .unwrap();
    for integer in [
        i64::MIN,
        -9_007_199_254_740_993,
        0,
        9_007_199_254_740_993,
        i64::MAX,
    ] {
        let typed = TypedValue::Integer(integer.to_string());
        driver
            .execute_bound(
                "INSERT INTO samples(value) VALUES (?)",
                &[typed.clone()],
                None,
            )
            .await
            .unwrap();
        let result = driver
            .query_typed(
                "SELECT value FROM samples WHERE rowid=last_insert_rowid()",
                &[],
                None,
            )
            .await
            .unwrap();
        assert_eq!(result.typed_rows, vec![vec![Some(typed.clone())]]);
        assert_eq!(
            result.result.rows[0].values,
            vec![Some(typed.display_json())]
        );
    }
}

#[tokio::test]
async fn text_binding_preserves_injection_quotes_backslashes_and_unicode() {
    let driver = driver().await;
    driver
        .execute_bound("CREATE TABLE samples(value TEXT)", &[], None)
        .await
        .unwrap();
    for text in [
        "'); DROP TABLE samples; --",
        "单引号'双引号\"反斜杠\\中文😀",
        "前\0后",
        "NULL",
        "",
    ] {
        let typed = TypedValue::Text(text.into());
        assert_eq!(
            driver
                .execute_bound("INSERT INTO samples VALUES (?)", &[typed.clone()], None)
                .await
                .unwrap(),
            1
        );
        let result = driver
            .query_typed(
                "SELECT value FROM samples WHERE value = ?",
                &[typed.clone()],
                None,
            )
            .await
            .unwrap();
        assert_eq!(result.typed_rows, vec![vec![Some(typed)]]);
    }
    let count = driver
        .query_typed("SELECT count(*) FROM samples", &[], None)
        .await
        .unwrap();
    assert_eq!(
        count.typed_rows[0][0],
        Some(TypedValue::Integer("5".into()))
    );
}

#[tokio::test]
async fn null_literal_empty_text_empty_blob_and_zero_bytes_are_distinct() {
    let driver = driver().await;
    let params = vec![
        TypedValue::Null,
        TypedValue::Text("NULL".into()),
        TypedValue::Text("".into()),
        TypedValue::binary(&[]),
        TypedValue::binary(&[0, 255, 0, 39, 92]),
    ];
    let result = driver
        .query_typed("SELECT ?, ?, ?, ?, ?", &params, None)
        .await
        .unwrap();
    assert_eq!(
        result.typed_rows,
        vec![params.iter().cloned().map(Some).collect::<Vec<_>>()]
    );
    assert_eq!(
        result.result.rows[0].values,
        params
            .iter()
            .map(|value| Some(value.display_json()))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        result.typed_rows[0][4]
            .as_ref()
            .unwrap()
            .binary_bytes()
            .unwrap(),
        vec![0, 255, 0, 39, 92]
    );
}

#[tokio::test]
async fn sqlite_reads_storage_class_and_boolean_binding_is_integer() {
    let driver = driver().await;
    driver
        .execute_bound(
            "CREATE TABLE samples(declared_boolean BOOLEAN, declared_decimal DECIMAL, arbitrary)",
            &[],
            None,
        )
        .await
        .unwrap();
    driver
        .execute_bound(
            "INSERT INTO samples VALUES (?, ?, ?)",
            &[
                TypedValue::Boolean(true),
                TypedValue::Text("12.50".into()),
                TypedValue::Float(1.25),
            ],
            None,
        )
        .await
        .unwrap();
    let result = driver
        .query_typed(
            "SELECT declared_boolean, declared_decimal, arbitrary, ? FROM samples",
            &[TypedValue::Boolean(false)],
            None,
        )
        .await
        .unwrap();
    // NUMERIC 亲和性将文本转 REAL；驱动必须报告实际存储类，不能伪称原生 DECIMAL。
    assert_eq!(
        result.typed_rows[0],
        vec![
            Some(TypedValue::Integer("1".into())),
            Some(TypedValue::Float(12.5)),
            Some(TypedValue::Float(1.25)),
            Some(TypedValue::Integer("0".into()))
        ]
    );
    let within_range = driver
        .query_typed(
            "SELECT ?",
            &[TypedValue::UnsignedInteger(i64::MAX.to_string())],
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        within_range.typed_rows[0][0],
        Some(TypedValue::Integer(i64::MAX.to_string()))
    );
}

#[tokio::test]
async fn unsupported_or_invalid_bindings_fail_before_writing() {
    let driver = driver().await;
    driver
        .execute_bound("CREATE TABLE samples(value)", &[], None)
        .await
        .unwrap();
    let unsupported = [
        TypedValue::Decimal("12345678901234567890.1200".into()),
        TypedValue::UnsignedInteger(u64::MAX.to_string()),
        TypedValue::Date("2026-10-20".into()),
        TypedValue::Time(TimeValue {
            value: "12:34:56".into(),
            precision: 0,
        }),
        TypedValue::Timestamp(TimestampValue {
            value: "2026-10-20T12:34:56".into(),
            precision: 0,
            timezone: TimezoneKind::None,
        }),
        TypedValue::Float(f64::INFINITY),
        TypedValue::Integer("01".into()),
        TypedValue::Binary("%%%".into()),
    ];
    for value in unsupported {
        assert!(matches!(
            driver
                .execute_bound("INSERT INTO samples VALUES (?)", &[value], None)
                .await,
            Err(DatabaseError::TypeError(_))
        ));
    }
    let count = driver
        .query_typed("SELECT count(*) FROM samples", &[], None)
        .await
        .unwrap();
    assert_eq!(
        count.typed_rows[0][0],
        Some(TypedValue::Integer("0".into()))
    );
}

#[tokio::test]
async fn parameter_mismatch_and_multiple_statements_have_no_side_effects() {
    let driver = driver().await;
    driver
        .execute_bound("CREATE TABLE samples(value)", &[], None)
        .await
        .unwrap();
    for (sql, params) in [
        ("INSERT INTO samples VALUES (?)", vec![]),
        (
            "INSERT INTO samples VALUES (?)",
            vec![TypedValue::Null, TypedValue::Null],
        ),
        (
            "INSERT INTO samples VALUES (?); DELETE FROM samples",
            vec![TypedValue::Null],
        ),
        (
            "INSERT INTO samples VALUES (?); invalid_tail",
            vec![TypedValue::Null],
        ),
    ] {
        assert!(driver.execute_bound(sql, &params, None).await.is_err());
    }
    assert!(driver.query_typed("SELECT ?", &[], None).await.is_err());
    assert!(driver
        .query_typed("SELECT 1; DELETE FROM samples", &[], None)
        .await
        .is_err());
    let count = driver
        .query_typed("SELECT count(*) FROM samples", &[], None)
        .await
        .unwrap();
    assert_eq!(
        count.typed_rows[0][0],
        Some(TypedValue::Integer("0".into()))
    );
    // 注释和引号内的分号不应被误判为多语句。
    driver
        .execute_bound(
            "INSERT INTO samples VALUES (';'); -- 尾注释\n /* ; */",
            &[],
            None,
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn invalid_utf8_is_unknown_but_legacy_display_is_preserved() {
    let driver = driver().await;
    let result = driver
        .query_typed("SELECT CAST(x'FF00' AS TEXT), NULL, x'FF00'", &[], None)
        .await
        .unwrap();
    let legacy = driver
        .query("SELECT CAST(x'FF00' AS TEXT)", None, None)
        .await
        .unwrap();
    assert_eq!(result.typed_rows[0][0], None);
    assert_eq!(result.result.rows[0].values[0], legacy.rows[0].values[0]);
    assert_eq!(result.typed_rows[0][1], Some(TypedValue::Null));
    assert_eq!(result.typed_rows[0][2], Some(TypedValue::binary(&[255, 0])));
    assert_eq!(
        result.result.rows[0].values.len(),
        result.typed_rows[0].len()
    );
}

#[tokio::test]
async fn truncation_preserves_row_and_column_alignment() {
    let driver = driver().await;
    let result = driver.query_typed("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<10001) SELECT x, NULL FROM n", &[], None).await.unwrap();
    assert!(result.result.truncated);
    assert_eq!(result.result.rows.len(), 10_000);
    assert_eq!(result.typed_rows.len(), result.result.rows.len());
    for (display, typed) in result.result.rows.iter().zip(result.typed_rows.iter()) {
        assert_eq!(display.values.len(), typed.len());
        assert_eq!(
            display.values,
            typed
                .iter()
                .map(|value| value.as_ref().map(TypedValue::display_json))
                .collect::<Vec<_>>()
        );
    }
}
