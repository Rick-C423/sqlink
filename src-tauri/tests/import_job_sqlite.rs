//! 真实 SQLite：预检零写、冻结来源、事务清理、取消和保守恢复。
use serde_json::json;
use sqlconnect_lib::{
    database::pool::ConnectionPoolManager,
    import_jobs::{self, ImportExecuteRequest, ImportPlanRequest},
    models::ConnectionConfig,
};
use std::{path::PathBuf, time::Duration};

struct Fixture {
    manager: ConnectionPoolManager,
    file: PathBuf,
    id: String,
}
impl Fixture {
    async fn new(sql: &str) -> Self {
        let manager = ConnectionPoolManager::new(None);
        let id = uuid::Uuid::new_v4().to_string();
        let config: ConnectionConfig = serde_json::from_value(json!({"id":id,"name":"导入计划回归","driverType":"sqlite","host":"","port":0,"user":"","password":"","database":":memory:"})).unwrap();
        manager.create_connection(config).await.unwrap();
        let file =
            std::env::temp_dir().join(format!("sqlink-import-job-{}.sql", uuid::Uuid::new_v4()));
        std::fs::write(&file, sql).unwrap();
        Self { manager, file, id }
    }
    async fn plan(&self) -> import_jobs::ImportPlan {
        import_jobs::preview(
            &self.manager,
            ImportPlanRequest {
                connection_id: self.id.clone(),
                database: None,
                path: self.file.to_string_lossy().into(),
                on_conflict: "error".into(),
                task_id: uuid::Uuid::new_v4().to_string(),
            },
        )
        .await
        .unwrap()
    }
    async fn execute(&self, plan: &import_jobs::ImportPlan) -> import_jobs::ImportReport {
        import_jobs::execute(
            &self.manager,
            ImportExecuteRequest {
                plan_id: plan.plan_id.clone(),
                task_id: uuid::Uuid::new_v4().to_string(),
            },
        )
        .await
        .unwrap()
    }
    async fn count(&self, sql: &str) -> serde_json::Value {
        let result = self
            .manager
            .get_connection(&self.id)
            .await
            .unwrap()
            .query(sql, None, None)
            .await
            .unwrap();
        result.rows[0].values[0].clone().unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.file);
    }
}

#[tokio::test]
async fn preview_reads_complete_file_without_sql_and_freezes_content() {
    let f = Fixture::new("CREATE TABLE frozen(id); INSERT INTO frozen VALUES(1);").await;
    let plan = f.plan().await;
    assert_eq!(plan.statement_count, 2);
    assert_eq!(
        f.count("SELECT count(*) FROM sqlite_schema WHERE name='frozen'")
            .await,
        json!(0)
    );
    // 大小不变也必须依内容摘要拒绝。
    std::fs::write(
        &f.file,
        "CREATE TABLE frozen(id); INSERT INTO frozen VALUES(2);",
    )
    .unwrap();
    let report = f.execute(&plan).await;
    assert_eq!(report.error_code.as_deref(), Some("IMPORT_FILE_CHANGED"));
    assert_eq!(report.sent_statements, 0);
    assert_eq!(report.commit_state, "notStarted");
    assert_eq!(
        f.count("SELECT count(*) FROM sqlite_schema WHERE name='frozen'")
            .await,
        json!(0)
    );
}

#[tokio::test]
async fn successful_execution_counts_are_not_commit_claims_and_plan_cannot_replay() {
    let f = Fixture::new("CREATE TEMP TABLE src(id); INSERT INTO src VALUES(7); BEGIN; CREATE TABLE imported(id); INSERT INTO imported SELECT id FROM src; COMMIT;").await;
    let plan = f.plan().await;
    let report = f.execute(&plan).await;
    assert_eq!(report.task["phase"], "succeeded");
    assert_eq!(report.summary.executed, 6);
    assert_eq!(report.commit_state, "unknown");
    assert_eq!(report.cleanup_state, "succeeded");
    assert!(!report.durable);
    assert_eq!(report.recovery, "manualVerification");
    assert_eq!(f.count("SELECT id FROM imported").await, json!(7));
    assert!(import_jobs::execute(
        &f.manager,
        ImportExecuteRequest {
            plan_id: plan.plan_id,
            task_id: uuid::Uuid::new_v4().to_string()
        }
    )
    .await
    .unwrap_err()
    .contains("IMPORT_PLAN_USED"));
    let receipt = import_jobs::receipt(&f.manager, f.id.clone(), report.run_id).unwrap();
    assert_eq!(receipt.summary.executed, 6);
    assert!(import_jobs::receipt(&f.manager, "other".into(), receipt.run_id).is_err());
}

#[tokio::test]
async fn sql_error_preserves_prior_commit_and_cleans_unclosed_transaction() {
    let f = Fixture::new("CREATE TABLE imported(id); INSERT INTO imported VALUES(1); BEGIN; INSERT INTO imported VALUES(2); INSERT INTO missing_table VALUES(3);").await;
    let report = f.execute(&f.plan().await).await;
    assert_eq!(report.failed_statement, Some(5));
    assert_eq!(report.summary.executed, 4);
    assert_eq!(report.task["phase"], "failed");
    assert_eq!(f.count("SELECT count(*) FROM imported").await, json!(1));
    let driver = f.manager.get_connection(&f.id).await.unwrap();
    driver
        .execute("INSERT INTO imported VALUES(4)", None, None)
        .await
        .unwrap();
    assert_eq!(f.count("SELECT count(*) FROM imported").await, json!(2));
}

#[tokio::test]
async fn changed_connection_generation_cannot_rebind_a_plan() {
    let f = Fixture::new("CREATE TABLE prohibited(id);").await;
    let plan = f.plan().await;
    let config: ConnectionConfig=serde_json::from_value(json!({"id":f.id,"name":"新代次","driverType":"sqlite","host":"","port":0,"user":"","password":"","database":":memory:"})).unwrap();
    f.manager.create_connection(config).await.unwrap();
    let report = f.execute(&plan).await;
    assert_eq!(report.error_code.as_deref(), Some("CONNECTION_CHANGED"));
    assert_eq!(report.sent_statements, 0);
    assert_eq!(
        f.count("SELECT count(*) FROM sqlite_schema WHERE name='prohibited'")
            .await,
        json!(0)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_interrupts_running_sql_and_session_can_be_reused() {
    let f = Fixture::new("CREATE TABLE imported(id); INSERT INTO imported VALUES(1); BEGIN; INSERT INTO imported SELECT x FROM (WITH RECURSIVE values_to_import(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM values_to_import WHERE x<100000000) SELECT x FROM values_to_import); COMMIT;").await;
    let plan = f.plan().await;
    let id = uuid::Uuid::new_v4().to_string();
    let manager = f.manager.clone();
    let task_id = id.clone();
    let job = tokio::spawn(async move {
        import_jobs::execute(
            &manager,
            ImportExecuteRequest {
                plan_id: plan.plan_id,
                task_id,
            },
        )
        .await
        .unwrap()
    });
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(snapshot) = f.manager.tasks.snapshot(&id) {
                if snapshot
                    .progress
                    .as_ref()
                    .is_some_and(|p| p.processed_statements >= 3)
                {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("必须进入长 SQL");
    f.manager.tasks.request_cancel(&id).await.unwrap();
    let report = tokio::time::timeout(Duration::from_secs(10), job)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(report.task["phase"], "cancelled");
    assert_eq!(report.cleanup_state, "succeeded");
    assert!(report.sent_statements >= 4);
    assert_eq!(report.commit_state, "unknown");
    assert_eq!(f.count("SELECT count(*) FROM imported").await, json!(1));
}

#[tokio::test]
async fn unsupported_copy_and_incomplete_tail_fail_before_any_sql() {
    let f =
        Fixture::new("CREATE TABLE never_written(id); COPY never_written FROM STDIN;\n1\n\\.\n")
            .await;
    let request = |path: String| ImportPlanRequest {
        connection_id: f.id.clone(),
        database: None,
        path,
        on_conflict: "error".into(),
        task_id: uuid::Uuid::new_v4().to_string(),
    };
    assert!(
        import_jobs::preview(&f.manager, request(f.file.to_string_lossy().into()))
            .await
            .unwrap_err()
            .contains("IMPORT_CLIENT_FORMAT_UNSUPPORTED")
    );
    std::fs::write(
        &f.file,
        "CREATE TABLE never_written(id); INSERT INTO never_written VALUES('unterminated",
    )
    .unwrap();
    assert!(
        import_jobs::preview(&f.manager, request(f.file.to_string_lossy().into()))
            .await
            .unwrap_err()
            .contains("IMPORT_PARSE_ERROR")
    );
    assert_eq!(
        f.count("SELECT count(*) FROM sqlite_schema WHERE name='never_written'")
            .await,
        json!(0)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn preparing_queue_cancellation_finishes_without_releasing_the_original_owner() {
    let f = Fixture::new("CREATE TABLE prohibited_while_queued(id);").await;
    let plan = f.plan().await;
    let driver = f.manager.get_connection(&f.id).await.unwrap();
    let mut original = driver.lease_session(None).await.unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    let manager = f.manager.clone();
    let worker_id = id.clone();
    let job = tokio::spawn(async move {
        import_jobs::execute(
            &manager,
            ImportExecuteRequest {
                plan_id: plan.plan_id,
                task_id: worker_id,
            },
        )
        .await
        .unwrap()
    });
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if f.manager.tasks.snapshot(&id).is_ok_and(|task| {
                task.phase == sqlconnect_lib::task_registry::TaskPhase::Preparing
                    && task.progress.as_ref().is_some_and(|progress| {
                        progress.stage == "parsing" && progress.processed_statements == 1
                    })
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("执行必须进入等待租约的准备阶段");
    f.manager.tasks.request_cancel(&id).await.unwrap();
    let report = tokio::time::timeout(Duration::from_secs(1), job)
        .await
        .expect("准备阶段取消不能等待原租约归还")
        .unwrap();
    assert_eq!(report.task["phase"], "cancelled");
    assert_eq!(report.sent_statements, 0);
    assert_eq!(report.summary.executed, 0);
    assert_eq!(f.manager.tasks.active_count(), 0);
    // 取消排队者不会触碰仍持有物理会话的原任务。
    original
        .execute("CREATE TEMP TABLE original_owner_alive(id)")
        .await
        .unwrap();
    original.release().await.unwrap();
    assert_eq!(
        f.count("SELECT count(*) FROM sqlite_schema WHERE name='prohibited_while_queued'")
            .await,
        json!(0)
    );
}
