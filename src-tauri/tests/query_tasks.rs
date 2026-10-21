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
    let manager = Arc::new(ConnectionPoolManager::new(None));
    let config:ConnectionConfig=serde_json::from_value(json!({"id":"A","name":"任务租约回归","driverType":"sqlite","host":"","port":0,"user":"","password":"","database":":memory:"})).unwrap();
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
async fn running_batch_cancel_confirms_rollback_and_next_task_is_healthy() {
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
    wait_phase(&manager, &id, TaskPhase::Running).await;
    manager.tasks.request_cancel(&id).await.unwrap();
    assert!(tokio::time::timeout(Duration::from_secs(5), join)
        .await
        .unwrap()
        .unwrap()
        .is_err());
    let done = manager.tasks.snapshot(&id).unwrap();
    assert_eq!(done.phase, TaskPhase::Cancelled);
    assert_eq!(done.commit_state, CommitState::RolledBack);
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
