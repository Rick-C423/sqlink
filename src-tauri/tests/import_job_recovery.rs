//! 记录持久化与崩溃恢复只承诺人工核对，不自动重放任意 SQL。
use serde_json::json;
use sqlconnect_lib::{
    database::pool::ConnectionPoolManager,
    import_jobs::{self, ImportExecuteRequest, ImportPlanRequest},
    models::ConnectionConfig,
};
use std::{path::PathBuf, time::Duration};
struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("sqlink-import-journal-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        assert!(self.0.starts_with(std::env::temp_dir()));
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
async fn connect(manager: &ConnectionPoolManager) -> String {
    let id = uuid::Uuid::new_v4().to_string();
    let config:ConnectionConfig=serde_json::from_value(json!({"id":id,"name":"执行记录回归","driverType":"sqlite","host":"","port":0,"user":"","password":"","database":":memory:"})).unwrap();
    manager.create_connection(config).await.unwrap();
    id
}
async fn plan(
    manager: &ConnectionPoolManager,
    id: &str,
    path: &std::path::Path,
) -> import_jobs::ImportPlan {
    import_jobs::preview(
        manager,
        ImportPlanRequest {
            connection_id: id.into(),
            database: None,
            path: path.to_string_lossy().into(),
            on_conflict: "error".into(),
            task_id: uuid::Uuid::new_v4().to_string(),
        },
    )
    .await
    .unwrap()
}
#[tokio::test]
async fn finished_journal_is_bounded_private_and_can_be_read_by_a_fresh_manager() {
    let directory = Directory::new();
    let manager = ConnectionPoolManager::new(Some(directory.0.clone()));
    let id = connect(&manager).await;
    let path = directory.0.join("sensitive-source-filename.sql");
    std::fs::write(&path,"CREATE TABLE private_name(id); INSERT INTO private_name VALUES('sensitive_literal_value');").unwrap();
    let prepared = plan(&manager, &id, &path).await;
    let report = import_jobs::execute(
        &manager,
        ImportExecuteRequest {
            plan_id: prepared.plan_id,
            task_id: uuid::Uuid::new_v4().to_string(),
        },
    )
    .await
    .unwrap();
    assert!(report.durable);
    assert_eq!(report.task["phase"], "succeeded");
    let journal = manager
        .import_journal_dir()
        .unwrap()
        .join(format!("{}.json", report.run_id));
    let bytes = std::fs::read(&journal).unwrap();
    assert!(bytes.len() <= 64 * 1024);
    let text = String::from_utf8(bytes).unwrap();
    for secret in [
        "sensitive-source-filename",
        "private_name",
        "sensitive_literal_value",
        "CREATE TABLE",
    ] {
        assert!(!text.contains(secret), "执行记录不能含来源路径或SQL内容");
    }
    let restarted = ConnectionPoolManager::new(Some(directory.0.clone()));
    let task_id = report.task["taskId"].as_str().unwrap().to_string();
    let by_task = import_jobs::receipt(&manager, id.clone(), task_id.clone()).unwrap();
    assert_eq!(by_task.run_id, report.run_id);
    assert!(import_jobs::receipt(&manager, "other-connection".into(), task_id.clone()).is_err());
    let recovered_by_task = import_jobs::receipt(&restarted, id.clone(), task_id.clone()).unwrap();
    assert_eq!(recovered_by_task.run_id, report.run_id);
    assert!(import_jobs::receipt(&restarted, "other-connection".into(), task_id.clone()).is_err());
    let receipt = import_jobs::receipt(&restarted, id.clone(), report.run_id.clone()).unwrap();
    assert!(receipt.recovered);
    assert_eq!(receipt.task["phase"], "succeeded");
    assert_eq!(receipt.summary.executed, 2);
    assert_eq!(receipt.commit_state, "unknown");
    // 同一taskId多记录时不能猜测；精确runId仍可查询。
    let mut duplicate = receipt.clone();
    duplicate.run_id = uuid::Uuid::new_v4().to_string();
    std::fs::write(
        manager
            .import_journal_dir()
            .unwrap()
            .join(format!("{}.json", duplicate.run_id)),
        serde_json::to_vec(&duplicate).unwrap(),
    )
    .unwrap();
    assert!(import_jobs::receipt(&restarted, id.clone(), task_id)
        .unwrap_err()
        .contains("IMPORT_RECEIPT_AMBIGUOUS"));
    assert_eq!(
        import_jobs::receipt(&restarted, id, report.run_id)
            .unwrap()
            .summary
            .executed,
        2
    );
}
#[tokio::test]
async fn interrupted_journal_does_not_claim_not_started_or_allow_replay() {
    let directory = Directory::new();
    let manager = ConnectionPoolManager::new(Some(directory.0.clone()));
    let run = uuid::Uuid::new_v4().to_string();
    let journal = manager.import_journal_dir().unwrap();
    std::fs::create_dir_all(&journal).unwrap();
    let record = json!({"runId":run,"planId":uuid::Uuid::new_v4().to_string(),"task":{"connectionId":"original-connection","phase":"running","commitState":"notStarted","cleanupState":"notStarted"},"summary":{"statements":4,"executed":3,"skipped":0,"cleanupNotice":"执行不等于提交"},"failedStatement":4,"sentStatements":4,"commitState":"notStarted","cleanupState":"notStarted","errorCode":null,"message":null,"durable":true,"recovery":"safeToRetry","recovered":false});
    std::fs::write(
        journal.join(format!("{run}.json")),
        serde_json::to_vec(&record).unwrap(),
    )
    .unwrap();
    let receipt =
        import_jobs::receipt(&manager, "original-connection".into(), run.clone()).unwrap();
    assert!(receipt.recovered);
    assert_eq!(receipt.task["phase"], "unknown");
    assert_eq!(receipt.commit_state, "unknown");
    assert_eq!(receipt.cleanup_state, "unknown");
    assert_eq!(receipt.recovery, "manualVerification");
    assert_eq!(receipt.summary.executed, 3);
    assert!(import_jobs::receipt(&manager, "other-connection".into(), run).is_err());
    assert!(
        import_jobs::receipt(&manager, "original-connection".into(), "../outside".into()).is_err()
    );
}
#[tokio::test]
async fn journal_failure_before_execution_sends_no_sql() {
    let directory = Directory::new();
    let manager = ConnectionPoolManager::new(Some(directory.0.clone()));
    let id = connect(&manager).await;
    let path = directory.0.join("source.sql");
    std::fs::write(&path, "CREATE TABLE must_not_exist(id);").unwrap();
    let prepared = plan(&manager, &id, &path).await;
    std::fs::write(manager.import_journal_dir().unwrap(), "阻止创建记录目录").unwrap();
    let report = import_jobs::execute(
        &manager,
        ImportExecuteRequest {
            plan_id: prepared.plan_id,
            task_id: uuid::Uuid::new_v4().to_string(),
        },
    )
    .await
    .unwrap();
    assert_eq!(report.sent_statements, 0);
    assert_eq!(report.summary.executed, 0);
    assert_eq!(report.error_code.as_deref(), Some("IMPORT_JOURNAL_FAILED"));
    let result = manager
        .get_connection(&id)
        .await
        .unwrap()
        .query(
            "SELECT count(*) FROM sqlite_schema WHERE name='must_not_exist'",
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(result.rows[0].values[0], Some(json!(0)));
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn journal_failure_after_sql_stops_later_statements_and_remains_unknown() {
    let directory = Directory::new();
    let manager = ConnectionPoolManager::new(Some(directory.0.clone()));
    let id = connect(&manager).await;
    let path = directory.0.join("source.sql");
    std::fs::write(&path,"CREATE TABLE imported(id); INSERT INTO imported WITH RECURSIVE numbers(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM numbers WHERE x<1000000) SELECT x FROM numbers; CREATE TABLE prohibited_after_failure(id);").unwrap();
    let prepared = plan(&manager, &id, &path).await;
    let task_id = uuid::Uuid::new_v4().to_string();
    let worker = manager.clone();
    let worker_id = task_id.clone();
    let job = tokio::spawn(async move {
        import_jobs::execute(
            &worker,
            ImportExecuteRequest {
                plan_id: prepared.plan_id,
                task_id: worker_id,
            },
        )
        .await
        .unwrap()
    });
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let ready = std::fs::read_dir(manager.import_journal_dir().unwrap())
                .ok()
                .is_some_and(|entries| {
                    entries.filter_map(Result::ok).any(|entry| {
                        entry
                            .path()
                            .extension()
                            .is_some_and(|extension| extension == "json")
                            && std::fs::read(entry.path())
                                .ok()
                                .and_then(|bytes| {
                                    serde_json::from_slice::<serde_json::Value>(&bytes).ok()
                                })
                                .is_some_and(|value| {
                                    value["task"]["taskId"] == task_id
                                        && value["sentStatements"] == 2
                                        && value["summary"]["executed"] == 1
                                })
                    })
                });
            if ready {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("必须进入第二条长SQL");
    let journal = manager.import_journal_dir().unwrap();
    let moved = directory.0.join("journal-before-fault");
    // 第二条 SQL 执行期间移动目录并用普通文件阻挡后续记录；不修改源 SQL。
    // 等待读写句柄短暂关闭，避免 Windows rename 与原子记录更新的瞬时竞争。
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if std::fs::rename(&journal, &moved).is_ok() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    std::fs::write(&journal, "执行记录故障").unwrap();
    let report = tokio::time::timeout(Duration::from_secs(15), job)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(report.task["phase"], "unknown");
    assert_eq!(report.error_code.as_deref(), Some("IMPORT_JOURNAL_FAILED"));
    assert_eq!(report.recovery, "manualVerification");
    assert!(report.summary.executed <= 2);
    let driver = manager.get_connection(&id).await.unwrap();
    let result = driver
        .query(
            "SELECT count(*) FROM sqlite_schema WHERE name='prohibited_after_failure'",
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(result.rows[0].values[0], Some(json!(0)));
    // 恢复原磁盘目录后，以新manager模拟重启，旧活动记录必须转为unknown。
    std::fs::remove_file(&journal).unwrap();
    std::fs::rename(&moved, &journal).unwrap();
    let restarted = ConnectionPoolManager::new(Some(directory.0.clone()));
    let recovered = import_jobs::receipt(&restarted, id, report.run_id).unwrap();
    assert_eq!(recovered.task["phase"], "unknown");
    assert_eq!(recovered.recovery, "manualVerification");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn aborted_execution_reconciles_cached_running_receipt_with_abandoned_task() {
    let directory = Directory::new();
    let manager = ConnectionPoolManager::new(Some(directory.0.clone()));
    let id = connect(&manager).await;
    let source = directory.0.join("source.sql");
    std::fs::write(&source,"CREATE TABLE imported(id); BEGIN; INSERT INTO imported WITH RECURSIVE numbers(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM numbers WHERE x<100000000) SELECT x FROM numbers; COMMIT;").unwrap();
    let prepared = plan(&manager, &id, &source).await;
    let task_id = uuid::Uuid::new_v4().to_string();
    let worker_id = task_id.clone();
    let worker = manager.clone();
    let job = tokio::spawn(async move {
        import_jobs::execute(
            &worker,
            ImportExecuteRequest {
                plan_id: prepared.plan_id,
                task_id: worker_id,
            },
        )
        .await
    });
    // 等待已持久记录第三条SQL的发送边界；没有按固定时长猜测工作线程状态。
    let run_id = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let entry = std::fs::read_dir(manager.import_journal_dir().unwrap())
                .ok()
                .and_then(|entries| {
                    entries.filter_map(Result::ok).find_map(|entry| {
                        if !entry
                            .path()
                            .extension()
                            .is_some_and(|extension| extension == "json")
                        {
                            return None;
                        }
                        let value: serde_json::Value =
                            serde_json::from_slice(&std::fs::read(entry.path()).ok()?).ok()?;
                        (value["task"]["taskId"] == task_id
                            && value["sentStatements"] == 3
                            && value["summary"]["executed"] == 2)
                            .then(|| value["runId"].as_str().unwrap().to_string())
                    })
                });
            if let Some(id) = entry {
                break id;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("必须到达长SQL发送边界");
    job.abort();
    assert!(job.await.unwrap_err().is_cancelled());
    let snapshot = manager.tasks.snapshot(&task_id).unwrap();
    assert_eq!(
        snapshot.phase,
        sqlconnect_lib::task_registry::TaskPhase::Unknown
    );
    assert_eq!(manager.tasks.active_count(), 0);
    let receipt = import_jobs::receipt(&manager, id.clone(), task_id.clone()).unwrap();
    assert_eq!(receipt.run_id, run_id);
    assert_eq!(receipt.task["phase"], "unknown");
    assert_eq!(receipt.commit_state, "unknown");
    assert_eq!(receipt.cleanup_state, "unknown");
    assert_eq!(receipt.recovery, "manualVerification");
    assert!(receipt.recovered);
    // 再次查询同一缓存和新manager磁盘回执都不能回到running。
    assert_eq!(
        import_jobs::receipt(&manager, id.clone(), run_id.clone())
            .unwrap()
            .task["phase"],
        "unknown"
    );
    let restarted = ConnectionPoolManager::new(Some(directory.0.clone()));
    assert_eq!(
        import_jobs::receipt(&restarted, id.clone(), run_id)
            .unwrap()
            .task["phase"],
        "unknown"
    );
    let driver = manager.get_connection(&id).await.unwrap();
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        driver.query("SELECT COUNT(*) FROM imported", None, None),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(result.rows[0].values[0], Some(json!(0)));
}
