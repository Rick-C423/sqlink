use sqlconnect_lib::database::{sqlite::SqliteDriver, DatabaseDriver};
use sqlconnect_lib::models::{ConnectionConfig, ResolvedTable};
use sqlconnect_lib::session::TargetSnapshot;
use std::time::Duration;

async fn driver(path: &str) -> SqliteDriver {
    let driver = SqliteDriver::connect(&ConnectionConfig {
        id: None,
        name: "SQLite 提交对象身份测试".into(),
        driver_type: "sqlite".into(),
        host: String::new(),
        port: 0,
        user: String::new(),
        password: String::new(),
        database: Some(path.into()),
        encrypt: None,
        save_password: None,
        group_name: None,
        color: None,
        mcp_allowed: None,
    })
    .await
    .unwrap();
    execute(
        &driver,
        &[
            "PRAGMA foreign_keys=ON",
            "CREATE TABLE samples(id INTEGER PRIMARY KEY, value TEXT)",
        ],
    )
    .await;
    driver
}

async fn execute(driver: &SqliteDriver, statements: &[&str]) {
    for sql in statements {
        driver.execute_bound(sql, &[], None).await.unwrap();
    }
}

async fn target(driver: &SqliteDriver, schema: &str, name: &str) -> ResolvedTable {
    driver
        .resolve_table("当前数据库", &[schema.into(), name.into()])
        .await
        .unwrap()
}

async fn snapshot(driver: &SqliteDriver, target: &ResolvedTable, write: bool) -> TargetSnapshot {
    let mut lease = driver.lease_session(None).await.unwrap();
    if write {
        lease.begin_write().await.unwrap();
    } else {
        lease.begin().await.unwrap();
    }
    let snapshot =
        tokio::time::timeout(Duration::from_secs(3), lease.inspect_target(target, write))
            .await
            .expect("检查不能再次等待自身持有的连接 gate")
            .unwrap();
    lease.rollback().await.unwrap();
    lease.release().await.unwrap();
    snapshot
}

#[tokio::test]
async fn inspection_requires_a_transaction_and_write_protection() {
    let driver = driver(":memory:").await;
    let target = target(&driver, "main", "samples").await;
    let mut lease = driver.lease_session(None).await.unwrap();
    assert!(lease.inspect_target(&target, false).await.is_err());
    lease.begin().await.unwrap();
    assert!(lease.inspect_target(&target, true).await.is_err());
    // 工作线程会在任何操作失败后回滚；后续只读检查必须重新建立事务。
    lease.begin().await.unwrap();
    assert!(lease.inspect_target(&target, false).await.is_ok());
    lease.rollback().await.unwrap();
    lease.release().await.unwrap();
}

#[tokio::test]
async fn complete_identity_is_stable_across_read_and_write_leases() {
    let driver = driver(":memory:").await;
    let target = target(&driver, "main", "samples").await;
    let first = snapshot(&driver, &target, false).await;
    let next = snapshot(&driver, &target, true).await;
    assert_eq!(first.identity, next.identity);
    assert!(first.write_reasons.is_empty(), "{:?}", first.write_reasons);
    assert_eq!(next.table.columns.len(), 2);
    assert!(next.table.columns[0].is_primary_key);
}

#[tokio::test]
async fn same_name_same_structure_replacement_invalidates_identity_even_if_cookie_is_reset() {
    let driver = driver(":memory:").await;
    let target = target(&driver, "main", "samples").await;
    let first = snapshot(&driver, &target, false).await;
    let cookie = driver
        .query("PRAGMA main.schema_version", None, None)
        .await
        .unwrap()
        .rows[0]
        .values[0]
        .clone()
        .unwrap();
    execute(
        &driver,
        &[
            "DROP TABLE main.samples",
            "CREATE TABLE samples(id INTEGER PRIMARY KEY, value TEXT)",
            &format!("PRAGMA main.schema_version={}", cookie.as_i64().unwrap()),
        ],
    )
    .await;
    let next = snapshot(&driver, &target, true).await;
    assert_eq!(first.table.columns.len(), next.table.columns.len());
    assert_ne!(first.identity, next.identity);
}

#[tokio::test]
async fn restored_connection_settings_still_invalidate_old_identity() {
    let driver = driver(":memory:").await;
    let target = target(&driver, "main", "samples").await;
    let first = snapshot(&driver, &target, false).await;
    execute(
        &driver,
        &[
            "PRAGMA ignore_check_constraints=ON",
            "PRAGMA ignore_check_constraints=OFF",
        ],
    )
    .await;
    let next = snapshot(&driver, &target, true).await;
    assert!(next.write_reasons.is_empty());
    assert_ne!(first.identity, next.identity);
}

#[tokio::test]
async fn unsafe_pragmas_keep_reads_available_and_refuse_writes() {
    let driver = driver(":memory:").await;
    let target = target(&driver, "main", "samples").await;
    for (name, unsafe_value, safe_value) in [
        ("foreign_keys", 0, 1),
        ("ignore_check_constraints", 1, 0),
        ("writable_schema", 1, 0),
        ("read_uncommitted", 1, 0),
    ] {
        execute(&driver, &[&format!("PRAGMA {}={}", name, unsafe_value)]).await;
        let read = snapshot(&driver, &target, false).await;
        let write = snapshot(&driver, &target, true).await;
        for inspected in [&read, &write] {
            assert!(
                inspected
                    .write_reasons
                    .iter()
                    .any(|reason| reason.contains(name)),
                "{}: {:?}",
                name,
                inspected.write_reasons
            );
        }
        execute(&driver, &[&format!("PRAGMA {}={}", name, safe_value)]).await;
    }
}

#[tokio::test]
async fn attached_alias_rebinding_and_temp_targets_never_receive_write_permission() {
    let driver = driver(":memory:").await;
    execute(
        &driver,
        &[
            "ATTACH ':memory:' AS another",
            "CREATE TABLE another.samples(id INTEGER PRIMARY KEY, value TEXT)",
            "CREATE TEMP TABLE temp_samples(id INTEGER PRIMARY KEY)",
        ],
    )
    .await;
    let attached = target(&driver, "another", "samples").await;
    let first = snapshot(&driver, &attached, false).await;
    assert!(!first.write_reasons.is_empty());
    execute(
        &driver,
        &[
            "DETACH another",
            "ATTACH ':memory:' AS another",
            "CREATE TABLE another.samples(id INTEGER PRIMARY KEY, value TEXT)",
        ],
    )
    .await;
    let rebound = snapshot(&driver, &attached, true).await;
    assert!(!rebound.write_reasons.is_empty());
    assert_ne!(first.identity, rebound.identity);
    let temporary = target(&driver, "temp", "temp_samples").await;
    assert!(!snapshot(&driver, &temporary, true)
        .await
        .write_reasons
        .is_empty());
}

#[tokio::test]
async fn views_virtual_shadow_tables_and_temp_triggers_are_conservatively_readonly() {
    let driver = driver(":memory:").await;
    execute(
        &driver,
        &[
            "CREATE VIEW sample_view AS SELECT * FROM samples",
            "CREATE VIRTUAL TABLE virtual_rows USING fts5(content)",
        ],
    )
    .await;
    for name in ["sample_view", "virtual_rows", "virtual_rows_data"] {
        let target = target(&driver, "main", name).await;
        assert!(
            !snapshot(&driver, &target, false)
                .await
                .write_reasons
                .is_empty(),
            "{}",
            name
        );
    }
    execute(
        &driver,
        &["CREATE TEMP TRIGGER samples_changed AFTER UPDATE ON main.samples BEGIN SELECT 1; END"],
    )
    .await;
    let target = target(&driver, "main", "samples").await;
    assert!(snapshot(&driver, &target, true)
        .await
        .write_reasons
        .iter()
        .any(|reason| reason.contains("TRIGGER")));
}

#[tokio::test]
async fn immediate_transaction_blocks_external_ddl_until_release() {
    let path = std::env::temp_dir().join(format!(
        "sqlconnect-identity-{}.sqlite",
        uuid::Uuid::new_v4()
    ));
    let driver = driver(path.to_str().unwrap()).await;
    let target = target(&driver, "main", "samples").await;
    let before = snapshot(&driver, &target, false).await;
    let external = rusqlite::Connection::open(&path).unwrap();
    external.busy_timeout(Duration::ZERO).unwrap();
    let mut lease = driver.lease_session(None).await.unwrap();
    lease.begin_write().await.unwrap();
    assert_eq!(
        before.identity,
        lease.inspect_target(&target, true).await.unwrap().identity
    );
    let error = external.execute_batch("DROP TABLE samples").unwrap_err();
    assert!(matches!(
        error.sqlite_error_code(),
        Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
    ));
    assert_eq!(
        before.identity,
        lease.inspect_target(&target, true).await.unwrap().identity
    );
    lease.rollback().await.unwrap();
    lease.release().await.unwrap();
    external
        .execute_batch(
            "DROP TABLE samples; CREATE TABLE samples(id INTEGER PRIMARY KEY, value TEXT)",
        )
        .unwrap();
    assert_ne!(
        before.identity,
        snapshot(&driver, &target, true).await.identity
    );
    drop(external);
    driver.disconnect().await.unwrap();
    let _ = std::fs::remove_file(path);
}
