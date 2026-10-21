//! 普通查询经过任务服务与 JSON 序列化后保持大整数；SQL 转储仍保留原生存储类。
use serde_json::{json, Value};
use sqlconnect_lib::{
    database::{
        dump::SqlSplitter, pool::ConnectionPoolManager, sqlite::SqliteDriver, DatabaseDriver,
    },
    models::ConnectionConfig,
    query_tasks,
};

fn config() -> ConnectionConfig {
    serde_json::from_value(json!({
        "id": "plain-integers", "name": "普通查询整数回归", "driverType": "sqlite",
        "host": "", "port": 0, "user": "", "password": "", "database": ":memory:"
    }))
    .unwrap()
}

#[tokio::test]
async fn plain_query_task_serializes_exact_integers_without_granting_edit_context() {
    let fixture: Value = serde_json::from_str(include_str!(
        "../../tests/fixtures/plain-query-integers.json"
    ))
    .unwrap();
    let manager = ConnectionPoolManager::new(None);
    manager.create_connection(config()).await.unwrap();
    let driver = manager.get_connection("plain-integers").await.unwrap();
    driver
        .execute(
            "CREATE TABLE samples(integer_value INTEGER, text_value TEXT)",
            None,
            None,
        )
        .await
        .unwrap();
    for row in fixture["rows"].as_array().unwrap() {
        // 夹具是受版本控制的规范 i64 字符串，不接收用户 SQL 或连接配置。
        let integer = row["values"][1].as_str().unwrap().parse::<i64>().unwrap();
        driver
            .execute(
                &format!("INSERT INTO samples VALUES ({integer}, '{integer}')"),
                None,
                None,
            )
            .await
            .unwrap();
    }
    let result = query_tasks::execute_query(
        &manager,
        "plain-integers".into(),
        "SELECT integer_value, text_value FROM samples ORDER BY rowid".into(),
        None,
        None,
        None,
        false,
    )
    .await
    .unwrap();
    let wire: Value = serde_json::from_str(&serde_json::to_string(&result).unwrap()).unwrap();
    assert_eq!(wire["rows"], fixture["rows"]);
    assert_eq!(wire["task"]["phase"], "succeeded");
    for field in ["resultContext", "rowHandles", "typedRows"] {
        assert!(wire.get(field).is_none(), "普通结果不得获得 {field}");
    }
    assert_eq!(
        result.result.rows[9].values[0].as_ref().unwrap().as_i64(),
        Some(9_007_199_254_740_993)
    );
    manager.disconnect("plain-integers").await.unwrap();
}

#[tokio::test]
async fn sqlite_integer_expressions_are_protected_even_without_declared_column_types() {
    let driver = SqliteDriver::connect(&config()).await.unwrap();
    let result = driver
        .query(
            "SELECT 9007199254740993 AS large, -9223372036854775808 AS minimum, \
             9223372036854775807 AS maximum, 9007199254740991 AS safe, \
             '9007199254740993' AS text_value, NULL AS missing",
            None,
            None,
        )
        .await
        .unwrap();
    assert!(result
        .columns
        .iter()
        .all(|column| column.data_type.is_empty()));
    let wire = serde_json::to_value(&result).unwrap();
    assert_eq!(
        wire["rows"][0]["values"],
        json!([
            "9007199254740993",
            "-9223372036854775808",
            "9223372036854775807",
            9_007_199_254_740_991i64,
            "9007199254740993",
            null
        ])
    );
    driver.disconnect().await.unwrap();
}

#[tokio::test]
async fn sql_dump_roundtrip_preserves_integer_and_numeric_text_in_columns_without_affinity() {
    let source = SqliteDriver::connect(&config()).await.unwrap();
    source
        .execute("CREATE TABLE samples(value)", None, None)
        .await
        .unwrap();
    source
        .execute(
            "INSERT INTO samples VALUES (9007199254740993), ('9007199254740993'), \
             (-9223372036854775808), (9223372036854775807)",
            None,
            None,
        )
        .await
        .unwrap();
    // 先序列化给前端，再转储；显示转换不得修改后端值或导出字面量。
    let result = source
        .query("SELECT value FROM samples ORDER BY rowid", None, None)
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(&result).unwrap()["rows"][0]["values"][0],
        "9007199254740993"
    );
    let (dump, count) = source
        .dump_table(None, "samples", false, true)
        .await
        .unwrap();
    assert_eq!(count, 4);
    assert!(dump.contains("(9007199254740993),\n('9007199254740993'),"));
    assert!(dump.contains("(-9223372036854775808),\n(9223372036854775807);"));
    let destination = SqliteDriver::connect(&config()).await.unwrap();
    // 沿用导入拆分器并固定租约；单语句 execute 接口不能代替完整文件导入。
    let mut reader = std::io::Cursor::new(dump.as_bytes());
    let mut splitter = SqlSplitter::new(false);
    let mut lease = destination.lease_session(None).await.unwrap();
    while let Some(statement) = splitter.next_statement(&mut reader).unwrap() {
        lease.execute(&statement).await.unwrap();
    }
    lease.release().await.unwrap();
    let restored = destination
        .query(
            "SELECT value, typeof(value) FROM samples ORDER BY rowid",
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        restored
            .rows
            .iter()
            .map(|row| row.values[1].clone())
            .collect::<Vec<_>>(),
        vec![
            Some(json!("integer")),
            Some(json!("text")),
            Some(json!("integer")),
            Some(json!("integer"))
        ]
    );
    assert_eq!(
        restored.rows[0].values[0],
        Some(json!(9_007_199_254_740_993i64))
    );
    assert_eq!(restored.rows[1].values[0], Some(json!("9007199254740993")));
    assert_eq!(restored.rows[2].values[0], Some(json!(i64::MIN)));
    assert_eq!(restored.rows[3].values[0], Some(json!(i64::MAX)));
    source.disconnect().await.unwrap();
    destination.disconnect().await.unwrap();
}
