//! 真实 SQLite 验证任务登记、租约排队取消、事务取消与 owner abort 的最终数据库状态。
use serde_json::json;
use sqlconnect_lib::{
    database::pool::ConnectionPoolManager,
    models::ConnectionConfig,
    query_tasks,
    task_registry::{CleanupState, CommitState, TaskPhase},
};
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

async fn fixture() -> Arc<ConnectionPoolManager> {
    fixture_at(":memory:").await
}

async fn fixture_at(database: &str) -> Arc<ConnectionPoolManager> {
    let manager = Arc::new(ConnectionPoolManager::new(None));
    let config:ConnectionConfig=serde_json::from_value(json!({"id":"A","name":"任务租约回归","driverType":"sqlite","host":"","port":0,"user":"","password":"","database":database})).unwrap();
    manager.create_connection(config).await.unwrap();
    manager
        .get_connection("A")
        .await
        .unwrap()
        .execute("CREATE TABLE entries(id INTEGER PRIMARY KEY)", None, None)
        .await
        .unwrap();
    manager
}
async fn wait_phase(manager: &ConnectionPoolManager, id: &str, phase: TaskPhase) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if manager.tasks.snapshot(id).is_ok_and(|s| s.phase == phase) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("任务没有进入预期阶段");
}
async fn count(manager: &ConnectionPoolManager) -> u64 {
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        manager.get_connection("A").await.unwrap().query(
            "SELECT COUNT(*) FROM entries",
            None,
            None,
        ),
    )
    .await
    .unwrap()
    .unwrap();
    result.rows[0].values[0].as_ref().unwrap().as_u64().unwrap()
}

async fn wait_sql_sent(manager: &ConnectionPoolManager, id: &str) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if manager.tasks.snapshot(id).is_ok_and(|s| {
                s.phase == TaskPhase::Running && s.commit_state == CommitState::Unknown
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("用户 SQL 未进入可能发送的阶段");
}

fn assert_partial_effect_warning(error: &str) {
    assert!(error.contains("BATCH_RESULT_UNKNOWN"), "{error}");
    assert!(error.contains("可能已部分生效"), "{error}");
    assert!(error.contains("人工核对"), "{error}");
    assert!(error.contains("禁止盲目重发"), "{error}");
}

fn assert_failed_batch_cleanup(manager: &ConnectionPoolManager, id: &str, phase: TaskPhase) {
    let status = manager.tasks.snapshot(id).unwrap();
    assert_eq!(status.phase, phase);
    assert_eq!(status.commit_state, CommitState::Unknown);
    assert_eq!(status.rollback_state, CleanupState::Succeeded);
    assert_eq!(status.cleanup_state, CleanupState::Succeeded);
    assert_eq!(manager.tasks.active_count(), 0);
}

#[tokio::test]
async fn task_success_keeps_source_session_and_confirmed_commit() {
    let manager = fixture().await;
    let id = Uuid::new_v4().to_string();
    let result = query_tasks::execute_query(
        &manager,
        "A".into(),
        "SELECT 1".into(),
        None,
        Some(1),
        Some(id.clone()),
        false,
    )
    .await
    .unwrap();
    assert_eq!(result.task.task_id, id);
    assert_eq!(result.task.connection_id, "A");
    assert_eq!(result.task.phase, TaskPhase::Succeeded);
    assert!(result.task.connection_generation.is_some());
    assert!(result.task.session_id.is_some());
    assert_eq!(
        result.task.capabilities.unwrap().reset_policy,
        "transactionOnly"
    );
    let batch_id = Uuid::new_v4().to_string();
    query_tasks::execute_batch(
        &manager,
        "A".into(),
        None,
        vec!["INSERT INTO entries VALUES(1)".into()],
        Some(2),
        Some(batch_id.clone()),
    )
    .await
    .unwrap();
    let status = manager.tasks.snapshot(&batch_id).unwrap();
    assert_eq!(status.phase, TaskPhase::Succeeded);
    assert_eq!(status.commit_state, CommitState::Committed);
    assert_eq!(count(&manager).await, 1);
    assert_eq!(manager.tasks.active_count(), 0);
}

#[tokio::test]
async fn queued_lease_cancellation_finishes_without_waiting_for_other_transaction() {
    let manager = fixture().await;
    let driver = manager.get_connection("A").await.unwrap();
    let mut owner = driver.lease_session(None).await.unwrap();
    owner.begin().await.unwrap();
    owner
        .execute("INSERT INTO entries VALUES(1)")
        .await
        .unwrap();
    let id = Uuid::new_v4().to_string();
    let worker = manager.clone();
    let task_id = id.clone();
    let join = tokio::spawn(async move {
        query_tasks::execute_batch(
            &worker,
            "A".into(),
            None,
            vec!["INSERT INTO entries VALUES(2)".into()],
            Some(3),
            Some(task_id),
        )
        .await
    });
    wait_phase(&manager, &id, TaskPhase::Preparing).await;
    let requested = manager.tasks.request_cancel(&id).await.unwrap();
    assert!(requested.cancel_requested);
    assert!(!requested.phase.terminal());
    assert!(tokio::time::timeout(Duration::from_secs(2), join)
        .await
        .unwrap()
        .unwrap()
        .is_err());
    let done = manager.tasks.snapshot(&id).unwrap();
    assert_eq!(done.phase, TaskPhase::Cancelled);
    assert_eq!(done.commit_state, CommitState::NotStarted);
    owner.rollback().await.unwrap();
    owner.release().await.unwrap();
    assert_eq!(count(&manager).await, 0);
    assert_eq!(manager.tasks.active_count(), 0);
}

const LONG_QUERY:&str="WITH RECURSIVE numbers(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM numbers WHERE n<1000000000) SELECT sum(n) FROM numbers";

#[tokio::test]
async fn running_batch_cancel_cleans_transaction_without_claiming_whole_script_rollback() {
    let manager = fixture().await;
    let id = Uuid::new_v4().to_string();
    let worker = manager.clone();
    let task_id = id.clone();
    let join = tokio::spawn(async move {
        query_tasks::execute_batch(
            &worker,
            "A".into(),
            None,
            vec!["INSERT INTO entries VALUES(1)".into(), LONG_QUERY.into()],
            Some(4),
            Some(task_id),
        )
        .await
    });
    wait_sql_sent(&manager, &id).await;
    manager.tasks.request_cancel(&id).await.unwrap();
    let error = tokio::time::timeout(Duration::from_secs(5), join)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert_partial_effect_warning(&error);
    assert!(error.contains("查询已取消"), "{error}");
    let done = manager.tasks.snapshot(&id).unwrap();
    assert_eq!(done.phase, TaskPhase::Cancelled);
    assert_eq!(done.commit_state, CommitState::Unknown);
    assert_eq!(done.rollback_state, CleanupState::Succeeded);
    assert_eq!(done.cleanup_state, CleanupState::Succeeded);
    assert_eq!(count(&manager).await, 0);
    assert_eq!(manager.tasks.active_count(), 0);
    assert!(manager.tasks.request_cancel(&id).await.is_err());
    query_tasks::execute_batch(
        &manager,
        "A".into(),
        None,
        vec!["INSERT INTO entries VALUES(2)".into()],
        Some(5),
        Some(Uuid::new_v4().to_string()),
    )
    .await
    .unwrap();
    assert_eq!(count(&manager).await, 1);
}

#[tokio::test]
async fn explicit_commit_before_failure_keeps_rows_and_reports_partial_effects() {
    let manager = fixture().await;
    let id = Uuid::new_v4().to_string();
    let error = query_tasks::execute_batch(
        &manager,
        "A".into(),
        None,
        vec![
            "INSERT INTO entries VALUES(1)".into(),
            "COMMIT".into(),
            "INSERT INTO missing_table VALUES(2)".into(),
        ],
        None,
        Some(id.clone()),
    )
    .await
    .unwrap_err();
    assert_partial_effect_warning(&error);
    assert!(error.contains("missing_table"), "{error}");
    assert_failed_batch_cleanup(&manager, &id, TaskPhase::Failed);
    assert_eq!(count(&manager).await, 1);
}

#[tokio::test]
async fn new_transaction_failure_only_rolls_back_rows_after_explicit_commit() {
    let manager = fixture().await;
    let id = Uuid::new_v4().to_string();
    let error = query_tasks::execute_batch(
        &manager,
        "A".into(),
        None,
        vec![
            "INSERT INTO entries VALUES(1)".into(),
            "COMMIT".into(),
            "BEGIN".into(),
            "INSERT INTO entries VALUES(2)".into(),
            "INSERT INTO missing_table VALUES(3)".into(),
        ],
        None,
        Some(id.clone()),
    )
    .await
    .unwrap_err();
    assert_partial_effect_warning(&error);
    assert!(error.contains("missing_table"), "{error}");
    assert_failed_batch_cleanup(&manager, &id, TaskPhase::Failed);
    let result = manager
        .get_connection("A")
        .await
        .unwrap()
        .query("SELECT id FROM entries ORDER BY id", None, None)
        .await
        .unwrap();
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0].values[0].as_ref().unwrap().as_u64(), Some(1));
}

#[tokio::test]
async fn cancellation_after_explicit_commit_preserves_committed_rows() {
    let path = std::env::temp_dir().join(format!("sqlink-query-task-{}.db", Uuid::new_v4()));
    let manager = fixture_at(path.to_str().unwrap()).await;
    let observer =
        rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let id = Uuid::new_v4().to_string();
    let worker = manager.clone();
    let task_id = id.clone();
    let join = tokio::spawn(async move {
        query_tasks::execute_batch(
            &worker,
            "A".into(),
            None,
            vec![
                "INSERT INTO entries VALUES(1)".into(),
                "COMMIT".into(),
                LONG_QUERY.into(),
                "INSERT INTO entries VALUES(2)".into(),
            ],
            None,
            Some(task_id),
        )
        .await
    });
    // 独立只读连接看到持久行后才取消，避免取消落在显式 COMMIT 之前。
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if observer
                .query_row("SELECT COUNT(*) FROM entries", [], |row| {
                    row.get::<_, u64>(0)
                })
                .is_ok_and(|rows| rows == 1)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("独立连接未观察到显式 COMMIT 的持久行");
    manager.tasks.request_cancel(&id).await.unwrap();
    let error = tokio::time::timeout(Duration::from_secs(5), join)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert_partial_effect_warning(&error);
    assert!(error.contains("查询已取消"), "{error}");
    assert_failed_batch_cleanup(&manager, &id, TaskPhase::Cancelled);
    assert_eq!(count(&manager).await, 1);
    drop(observer);
    manager.disconnect("A").await.unwrap();
    std::fs::remove_file(path).unwrap();
}

#[tokio::test]
async fn outer_commit_failure_after_script_commit_stays_unknown_and_preserves_cause() {
    let manager = fixture().await;
    let id = Uuid::new_v4().to_string();
    let error = query_tasks::execute_batch(
        &manager,
        "A".into(),
        None,
        vec!["INSERT INTO entries VALUES(1)".into(), "COMMIT".into()],
        None,
        Some(id.clone()),
    )
    .await
    .unwrap_err();
    assert!(error.contains("COMMIT_UNKNOWN"), "{error}");
    assert!(error.contains("no transaction is active"), "{error}");
    assert!(error.contains("可能已部分生效"), "{error}");
    let status = manager.tasks.snapshot(&id).unwrap();
    assert_eq!(status.phase, TaskPhase::Unknown);
    assert_eq!(status.commit_state, CommitState::Unknown);
    assert_eq!(status.cleanup_state, CleanupState::Succeeded);
    assert_eq!(count(&manager).await, 1);
}

#[tokio::test]
async fn owner_abort_drops_active_task_and_rolls_back_before_reuse() {
    let manager = fixture().await;
    let id = Uuid::new_v4().to_string();
    let worker = manager.clone();
    let task_id = id.clone();
    let join = tokio::spawn(async move {
        query_tasks::execute_batch(
            &worker,
            "A".into(),
            None,
            vec!["INSERT INTO entries VALUES(1)".into(), LONG_QUERY.into()],
            Some(6),
            Some(task_id),
        )
        .await
    });
    wait_phase(&manager, &id, TaskPhase::Running).await;
    join.abort();
    assert!(join.await.unwrap_err().is_cancelled());
    assert_eq!(manager.tasks.active_count(), 0);
    let done = manager.tasks.snapshot(&id).unwrap();
    assert_eq!(done.phase, TaskPhase::Unknown);
    // 未收到提交确认不能报已回滚；实际清理结果通过随后读库单独验证。
    assert_eq!(done.commit_state, CommitState::Unknown);
    assert_eq!(count(&manager).await, 0);
}
