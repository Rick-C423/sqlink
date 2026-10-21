use sqlconnect_lib::database::{sqlite::SqliteDriver, DatabaseDriver};
use sqlconnect_lib::models::{ConnectionConfig, DatabaseError};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::timeout;

const LONG_QUERY: &str = "WITH RECURSIVE n(x) AS (VALUES(0) UNION ALL SELECT x+1 FROM n WHERE x<1000000000) SELECT sum(x) FROM n";
const DEADLINE: Duration = Duration::from_secs(3);

async fn driver() -> Arc<SqliteDriver> {
    Arc::new(
        SqliteDriver::connect(&ConnectionConfig {
            id: None,
            name: "SQLite 独占会话测试".into(),
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
        .unwrap(),
    )
}

async fn scalar(driver: &SqliteDriver, sql: &str) -> i64 {
    timeout(DEADLINE, driver.query(sql, None, None))
        .await
        .expect("查询不应等待被遗弃的事务")
        .unwrap()
        .rows[0]
        .values[0]
        .as_ref()
        .unwrap()
        .as_i64()
        .unwrap()
}

async fn running(driver: &SqliteDriver) {
    timeout(DEADLINE, async {
        while driver.session_stats().operations == 0 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("工作线程应开始执行查询");
}

async fn baseline(driver: &SqliteDriver) {
    timeout(DEADLINE, async {
        loop {
            let stats = driver.session_stats();
            if stats.leases == 0 && stats.operations == 0 && stats.registered_queries == 0 {
                assert_eq!(stats.workers, 1);
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("租约、查询登记与操作必须回到基线");
}

#[tokio::test]
async fn concurrent_transactions_and_metadata_wait_for_the_complete_lease() {
    let driver = driver().await;
    assert_eq!(
        driver.session_capabilities().reset_policy,
        "transactionOnly"
    );
    driver
        .execute_bound("CREATE TABLE samples(value INTEGER)", &[], None)
        .await
        .unwrap();
    let mut first = driver.lease_session(None).await.unwrap();
    assert_eq!(first.capabilities().reset_policy, "transactionOnly");
    let first_id = first.session_id().to_owned();
    first.begin().await.unwrap();
    first
        .execute_bound("INSERT INTO samples VALUES (1)", &[])
        .await
        .unwrap();

    let second_driver = driver.clone();
    let mut second = tokio::spawn(async move {
        let mut lease = second_driver.lease_session(None).await.unwrap();
        assert_ne!(lease.session_id(), first_id);
        lease.begin().await.unwrap();
        lease
            .execute_bound("INSERT INTO samples VALUES (2)", &[])
            .await
            .unwrap();
        lease.commit().await.unwrap();
        lease.release().await.unwrap();
    });
    let metadata_driver = driver.clone();
    let mut metadata = tokio::spawn(async move { metadata_driver.get_tables(None).await });
    assert!(timeout(Duration::from_millis(40), &mut second)
        .await
        .is_err());
    assert!(timeout(Duration::from_millis(40), &mut metadata)
        .await
        .is_err());
    first.rollback().await.unwrap();
    first.release().await.unwrap();
    timeout(DEADLINE, second).await.unwrap().unwrap();
    assert_eq!(
        timeout(DEADLINE, metadata)
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(scalar(&driver, "SELECT sum(value) FROM samples").await, 2);
    baseline(&driver).await;
}

#[tokio::test]
async fn syntax_prepare_and_bound_errors_clear_the_transaction() {
    let driver = driver().await;
    driver
        .execute_bound("CREATE TABLE samples(value INTEGER)", &[], None)
        .await
        .unwrap();
    for sql in [
        "SELECT FROM",
        "SELECT * FROM absent",
        "SELECT 1; SELECT 2",
        "",
    ] {
        let mut lease = driver.lease_session(None).await.unwrap();
        lease.begin().await.unwrap();
        lease
            .execute_bound("INSERT INTO samples VALUES (1)", &[])
            .await
            .unwrap();
        assert!(lease.query_typed(sql, &[]).await.is_err());
        lease.release().await.unwrap();
        assert_eq!(scalar(&driver, "SELECT count(*) FROM samples").await, 0);
    }
    assert!(driver
        .execute_batch(
            vec![
                "INSERT INTO samples VALUES (3)".into(),
                "INVALID SQL".into()
            ],
            None
        )
        .await
        .is_err());
    assert_eq!(scalar(&driver, "SELECT count(*) FROM samples").await, 0);
    baseline(&driver).await;
}

#[tokio::test]
async fn cancellation_interrupts_running_sql_and_preserves_the_memory_database() {
    let driver = driver().await;
    driver
        .execute_bound("CREATE TABLE marker(value INTEGER)", &[], None)
        .await
        .unwrap();
    driver
        .execute_bound("INSERT INTO marker VALUES (42)", &[], None)
        .await
        .unwrap();
    let mut lease = driver.lease_session(None).await.unwrap();
    lease.begin().await.unwrap();
    lease
        .execute_bound("INSERT INTO marker VALUES (99)", &[])
        .await
        .unwrap();
    let handle = lease.cancel_handle().unwrap();
    let task = tokio::spawn(async move {
        let result = lease.query(LONG_QUERY).await;
        (lease, result)
    });
    running(&driver).await;
    timeout(DEADLINE, handle.cancel()).await.unwrap().unwrap();
    let (mut lease, result) = timeout(DEADLINE, task).await.unwrap().unwrap();
    assert!(matches!(result, Err(DatabaseError::QueryCancelled)));
    // 批任务取消后必须能确认回滚，不能把取消门禁当成回滚状态未知。
    timeout(DEADLINE, lease.rollback()).await.unwrap().unwrap();
    assert!(matches!(
        lease.query("SELECT 1").await,
        Err(DatabaseError::QueryCancelled)
    ));
    lease.release().await.unwrap();
    assert_eq!(scalar(&driver, "SELECT sum(value) FROM marker").await, 42);
    baseline(&driver).await;
}

#[tokio::test]
async fn stale_handle_cannot_interrupt_a_new_lease() {
    let driver = driver().await;
    let first = driver.lease_session(None).await.unwrap();
    let first_id = first.session_id().to_owned();
    let stale = first.cancel_handle().unwrap();
    first.release().await.unwrap();
    let mut second = driver.lease_session(None).await.unwrap();
    assert_ne!(first_id, second.session_id());
    let current = second.cancel_handle().unwrap();
    let mut task = tokio::spawn(async move {
        let result = second.query(LONG_QUERY).await;
        second.release().await.unwrap();
        result
    });
    running(&driver).await;
    assert!(stale.cancel().await.is_err());
    assert!(timeout(Duration::from_millis(40), &mut task).await.is_err());
    current.cancel().await.unwrap();
    assert!(matches!(
        timeout(DEADLINE, task).await.unwrap().unwrap(),
        Err(DatabaseError::QueryCancelled)
    ));
    assert_eq!(scalar(&driver, "SELECT 7").await, 7);
    baseline(&driver).await;
}

#[tokio::test]
async fn aborting_or_panicking_the_owner_rolls_back_before_next_lease() {
    let driver = driver().await;
    driver
        .execute_bound("CREATE TABLE samples(value INTEGER)", &[], None)
        .await
        .unwrap();
    let mut lease = driver.lease_session(None).await.unwrap();
    lease.begin().await.unwrap();
    lease
        .execute_bound("INSERT INTO samples VALUES (1)", &[])
        .await
        .unwrap();
    let task = tokio::spawn(async move { lease.query(LONG_QUERY).await });
    running(&driver).await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert_eq!(scalar(&driver, "SELECT count(*) FROM samples").await, 0);

    let task_driver = driver.clone();
    let task = tokio::spawn(async move {
        let mut lease = task_driver.lease_session(None).await.unwrap();
        lease.begin().await.unwrap();
        lease
            .execute_bound("INSERT INTO samples VALUES (2)", &[])
            .await
            .unwrap();
        panic!("模拟租约持有者 panic");
    });
    assert!(task.await.unwrap_err().is_panic());
    assert_eq!(scalar(&driver, "SELECT count(*) FROM samples").await, 0);
    baseline(&driver).await;
}

#[tokio::test]
async fn dropping_only_the_query_future_also_cancels_the_worker() {
    let driver = driver().await;
    let mut lease = driver.lease_session(None).await.unwrap();
    lease.begin().await.unwrap();
    assert!(timeout(Duration::from_millis(40), lease.query(LONG_QUERY))
        .await
        .is_err());
    timeout(DEADLINE, lease.release()).await.unwrap().unwrap();
    assert_eq!(scalar(&driver, "SELECT 8").await, 8);
    baseline(&driver).await;
}

#[tokio::test]
async fn query_id_cancel_is_scoped_and_registration_is_raii() {
    let driver = driver().await;
    let task_driver = driver.clone();
    let mut task =
        tokio::spawn(async move { task_driver.query(LONG_QUERY, None, Some(100)).await });
    running(&driver).await;
    assert_eq!(driver.session_stats().registered_queries, 1);
    assert!(driver.cancel_query(101).await.is_err());
    assert!(timeout(Duration::from_millis(40), &mut task).await.is_err());
    driver.cancel_query(100).await.unwrap();
    assert!(matches!(
        timeout(DEADLINE, task).await.unwrap().unwrap(),
        Err(DatabaseError::QueryCancelled)
    ));
    assert!(driver.cancel_query(100).await.is_err());
    baseline(&driver).await;

    let task_driver = driver.clone();
    let task = tokio::spawn(async move { task_driver.query(LONG_QUERY, None, Some(102)).await });
    running(&driver).await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert_eq!(scalar(&driver, "SELECT 9").await, 9);
    baseline(&driver).await;

    // 前一操作的登记必须在 gate 放行前消失，排队后重用编号不能误报占用。
    let (first, second) = tokio::join!(
        driver.query("SELECT 1", None, Some(104)),
        driver.query("SELECT 2", None, Some(104))
    );
    assert!(first.is_ok());
    assert!(second.is_ok());
    assert!(driver.query("SELECT FROM", None, Some(105)).await.is_err());
    baseline(&driver).await;
}

#[tokio::test]
async fn truncation_releases_statements_without_contaminating_the_connection() {
    let driver = driver().await;
    for _ in 0..4 {
        let mut lease = driver.lease_session(None).await.unwrap();
        let result = lease.query_typed("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<10005) SELECT x FROM n", &[]).await.unwrap();
        assert!(result.result.truncated);
        assert_eq!(result.result.rows.len(), 10_000);
        assert_eq!(result.typed_rows.len(), 10_000);
        lease.release().await.unwrap();
        assert_eq!(scalar(&driver, "SELECT 10").await, 10);
    }
    baseline(&driver).await;
}

#[tokio::test]
async fn disconnect_interrupts_the_worker_and_never_reopens_the_database() {
    let driver = driver().await;
    let task_driver = driver.clone();
    let task = tokio::spawn(async move { task_driver.query(LONG_QUERY, None, Some(103)).await });
    running(&driver).await;
    timeout(DEADLINE, driver.disconnect())
        .await
        .unwrap()
        .unwrap();
    assert!(timeout(DEADLINE, task).await.unwrap().unwrap().is_err());
    assert!(!driver.is_connected());
    assert!(driver.lease_session(None).await.is_err());
    assert!(driver.query("SELECT 1", None, None).await.is_err());
    timeout(DEADLINE, async {
        while driver.session_stats().workers != 0 || driver.session_stats().leases != 0 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(driver.session_stats().registered_queries, 0);
}
