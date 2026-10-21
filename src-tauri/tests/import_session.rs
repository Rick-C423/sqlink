//! SQL 文件逐句解析仍应共用整份文件的租约，错误后清理事务但不许诺已提交语句回滚。
use serde_json::json;
use sqlconnect_lib::{
    commands::import::import_sql_file, database::pool::ConnectionPoolManager,
    models::ConnectionConfig,
};
use std::path::PathBuf;

struct SqlFile(PathBuf);
impl SqlFile {
    fn new(sql: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("sqlink-p103-import-{}.sql", uuid::Uuid::new_v4()));
        std::fs::write(&path, sql).unwrap();
        Self(path)
    }
}
impl Drop for SqlFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}
async fn manager() -> ConnectionPoolManager {
    let manager = ConnectionPoolManager::new(None);
    let config:ConnectionConfig=serde_json::from_value(json!({"id":"A","name":"导入租约回归","driverType":"sqlite","host":"","port":0,"user":"","password":"","database":":memory:"})).unwrap();
    manager.create_connection(config).await.unwrap();
    manager
}
#[tokio::test]
async fn import_uses_one_session_for_temporary_objects_and_transactions() {
    let manager = manager().await;
    let file=SqlFile::new("CREATE TEMP TABLE source_values(id); INSERT INTO source_values VALUES(9007199254740993); BEGIN; CREATE TABLE imported(id INTEGER PRIMARY KEY); INSERT INTO imported SELECT id FROM source_values; COMMIT;");
    let summary = import_sql_file(
        &manager,
        "A".into(),
        None,
        file.0.to_string_lossy().into(),
        "error".into(),
    )
    .await
    .unwrap();
    assert_eq!(summary.executed, 6);
    let driver = manager.get_connection("A").await.unwrap();
    let result = driver
        .query_typed("SELECT id FROM imported", &[], None)
        .await
        .unwrap();
    assert_eq!(
        result.typed_rows[0][0],
        Some(sqlconnect_lib::typed_value::TypedValue::Integer(
            "9007199254740993".into()
        ))
    );
}
#[tokio::test]
async fn import_failure_rolls_back_open_transaction_and_preserves_prior_commits() {
    let manager = manager().await;
    let file=SqlFile::new("CREATE TABLE imported(id INTEGER PRIMARY KEY); BEGIN; INSERT INTO imported VALUES(1); INSERT INTO missing_table VALUES(2);");
    let error = import_sql_file(
        &manager,
        "A".into(),
        None,
        file.0.to_string_lossy().into(),
        "error".into(),
    )
    .await
    .unwrap_err();
    assert!(error.contains("第 4 条"));
    assert!(error.contains("可能已提交"));
    let driver = manager.get_connection("A").await.unwrap();
    let result = driver
        .query("SELECT count(*) FROM imported", None, None)
        .await
        .unwrap();
    assert_eq!(result.rows[0].values[0], Some(json!(0)));
    driver
        .execute("INSERT INTO imported VALUES(3)", None, None)
        .await
        .unwrap();
    let result = driver
        .query("SELECT count(*) FROM imported", None, None)
        .await
        .unwrap();
    assert_eq!(result.rows[0].values[0], Some(json!(1)));
}
