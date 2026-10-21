//! 用 Windows 桌面主线程同量级的 1 MiB 栈覆盖预检及执行，不能用调大应用栈替代修复。
use sqlconnect_lib::database::pool::ConnectionPoolManager;
use sqlconnect_lib::import_jobs::{self, ImportExecuteRequest, ImportPlanRequest};
use sqlconnect_lib::models::ConnectionConfig;
use std::sync::Arc;

struct TemporaryFiles {
    database: std::path::PathBuf,
    sql: std::path::PathBuf,
}
impl Drop for TemporaryFiles {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.database);
        let _ = std::fs::remove_file(&self.sql);
    }
}
fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

#[test]
fn import_preview_and_execution_work_on_one_mib_thread_stack() {
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let files = TemporaryFiles {
        database: std::env::temp_dir().join(format!("sqlink-import-stack-{nonce}.db")),
        sql: std::env::temp_dir().join(format!("sqlink-import-stack-{nonce}.sql")),
    };
    // 超过多轮摘要缓冲，确保真正经过跨 await 的摘要/拆分，不是空文件快速返回。
    std::fs::write(&files.sql, format!("/*{}*/\nCREATE TABLE stack_probe(id INTEGER PRIMARY KEY,value TEXT); INSERT INTO stack_probe VALUES(1,'窄栈回归');", "x".repeat(256 * 1024))).unwrap();
    let manager = Arc::new(ConnectionPoolManager::new(None));
    let config: ConnectionConfig = serde_json::from_value(serde_json::json!({
        "id":"stack-test", "name":"独立窄栈验收", "driverType":"sqlite", "host":"",
        "port":0, "user":"", "password":"", "database":files.database.to_string_lossy()
    }))
    .unwrap();
    let parent_runtime = runtime();
    parent_runtime
        .block_on(manager.create_connection(config))
        .unwrap();
    let prepared = {
        let manager = manager.clone();
        let path = files.sql.to_string_lossy().into_owned();
        std::thread::Builder::new()
            .name("导入预检一MiB栈".into())
            .stack_size(1024 * 1024)
            .spawn(move || {
                runtime().block_on(import_jobs::preview(
                    &manager,
                    ImportPlanRequest {
                        connection_id: "stack-test".into(),
                        database: None,
                        path,
                        on_conflict: "error".into(),
                        task_id: uuid::Uuid::new_v4().to_string(),
                    },
                ))
            })
            .unwrap()
            .join()
            .unwrap()
            .unwrap()
    };
    assert_eq!(prepared.statement_count, 2);
    let driver = parent_runtime
        .block_on(manager.get_connection("stack-test"))
        .unwrap();
    let absent = parent_runtime
        .block_on(driver.query(
            "SELECT COUNT(*) FROM sqlite_schema WHERE name='stack_probe'",
            None,
            None,
        ))
        .unwrap();
    assert_eq!(
        absent.rows[0].values[0],
        Some(serde_json::json!(0)),
        "预检必须零写入"
    );
    let report = {
        let manager = manager.clone();
        std::thread::Builder::new()
            .name("导入执行一MiB栈".into())
            .stack_size(1024 * 1024)
            .spawn(move || {
                runtime().block_on(import_jobs::execute(
                    &manager,
                    ImportExecuteRequest {
                        plan_id: prepared.plan_id,
                        task_id: uuid::Uuid::new_v4().to_string(),
                    },
                ))
            })
            .unwrap()
            .join()
            .unwrap()
            .unwrap()
    };
    assert_eq!(report.task["phase"], "succeeded");
    assert_eq!(report.summary.executed, 2);
    assert_eq!(report.cleanup_state, "succeeded");
    let stored = parent_runtime
        .block_on(driver.query("SELECT value FROM stack_probe", None, None))
        .unwrap();
    assert_eq!(
        stored.rows[0].values[0],
        Some(serde_json::json!("窄栈回归"))
    );
    parent_runtime
        .block_on(manager.disconnect("stack-test"))
        .unwrap();
}
