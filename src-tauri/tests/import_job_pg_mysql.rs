//! 显式独立实验库：预检/冻结执行同会话、脚本事务与出错清理。
//! 不读取应用保存连接；缺 SQLINK_TEST_* 时显式运行必须失败。
use serde_json::json;
use sqlconnect_lib::{
    database::pool::ConnectionPoolManager,
    import_jobs::{self, ImportExecuteRequest, ImportPlanRequest},
    models::ConnectionConfig,
};
use std::path::PathBuf;
struct SqlFile(PathBuf);
impl Drop for SqlFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}
fn config(kind: &str) -> ConnectionConfig {
    let variable = if kind == "postgres" {
        "SQLINK_TEST_POSTGRES"
    } else {
        "SQLINK_TEST_MYSQL"
    };
    let raw = std::env::var(variable)
        .unwrap_or_else(|_| panic!("显式运行必须设置 {variable} 独立实验库"));
    let (host, port, user, password, database) = if kind == "mysql" {
        let options = mysql_async::Opts::from_url(&raw).expect("MySQL实验连接串无效");
        (
            options.ip_or_hostname().to_string(),
            options.tcp_port(),
            options.user().unwrap_or("").to_owned(),
            options.pass().unwrap_or("").to_owned(),
            options
                .db_name()
                .filter(|name| !name.is_empty())
                .expect("必须指定独立实验数据库")
                .to_owned(),
        )
    } else {
        let options = raw
            .parse::<tokio_postgres::Config>()
            .expect("PostgreSQL实验连接串无效");
        let host = match options.get_hosts().first() {
            Some(tokio_postgres::config::Host::Tcp(host)) => host.clone(),
            _ => panic!("实验连接必须明确指定TCP主机"),
        };
        (
            host,
            options.get_ports().first().copied().unwrap_or(5432),
            options.get_user().unwrap_or("").to_owned(),
            String::from_utf8(options.get_password().unwrap_or(b"").to_vec())
                .expect("实验密码必须是UTF-8"),
            options
                .get_dbname()
                .filter(|name| !name.is_empty())
                .expect("必须指定独立实验数据库")
                .to_owned(),
        )
    };
    serde_json::from_value(json!({"name":"导入计划同会话回归","driverType":kind,"host":host,"port":port,"user":user,"password":password,"database":database,"savePassword":false,"mcpAllowed":false})).unwrap()
}
async fn run(kind: &str) {
    let manager = ConnectionPoolManager::new(None);
    let config = config(kind);
    let database = config
        .database
        .clone()
        .filter(|name| !name.is_empty())
        .expect("必须明确指定独立实验数据库");
    let id = manager
        .create_connection(config)
        .await
        .expect("连接独立实验库失败");
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let temporary = format!("import_job_temp_{nonce}");
    let target = format!("import_job_probe_{nonce}");
    let file = SqlFile(std::env::temp_dir().join(format!("sqlink-import-job-{nonce}.sql")));
    let sql=format!("CREATE TEMPORARY TABLE {temporary}(id BIGINT); INSERT INTO {temporary} VALUES(1); BEGIN; INSERT INTO {temporary} VALUES(2); ROLLBACK; CREATE TABLE {target} AS SELECT COUNT(*) AS imported_count FROM {temporary};");
    std::fs::write(&file.0, sql).unwrap();
    let make_request = || ImportPlanRequest {
        connection_id: id.clone(),
        database: Some(database.clone()),
        path: file.0.to_string_lossy().into(),
        on_conflict: "error".into(),
        task_id: uuid::Uuid::new_v4().to_string(),
    };
    let prepared = import_jobs::preview(&manager, make_request())
        .await
        .expect("预检失败");
    let driver = manager.get_connection(&id).await.unwrap();
    let report = import_jobs::execute(
        &manager,
        ImportExecuteRequest {
            plan_id: prepared.plan_id,
            task_id: uuid::Uuid::new_v4().to_string(),
        },
    )
    .await
    .expect("执行入口失败");
    let outcome = async {
        assert_eq!(report.task["phase"], "succeeded", "{:?}", report.error_code);
        assert_eq!(report.summary.executed, 6);
        assert_eq!(report.cleanup_state, "succeeded");
        assert_eq!(report.commit_state, "unknown");
        let result = driver
            .query(
                &format!("SELECT imported_count FROM {target}"),
                Some(&database),
                None,
            )
            .await
            .unwrap();
        // MySQL BIGINT/COUNT 保留为精确十进制文本；PG 的安全小整数仍可为 JSON 数字。
        let expected_count = if kind == "mysql" {
            json!("1")
        } else {
            json!(1)
        };
        assert_eq!(result.rows[0].values[0], Some(expected_count.clone()));
        std::fs::write(&file.0,format!("BEGIN; INSERT INTO {target} VALUES(2); INSERT INTO nonexistent_import_target_{nonce} VALUES(3);")).unwrap();
        let prepared = import_jobs::preview(&manager, make_request())
            .await
            .unwrap();
        let failed = import_jobs::execute(
            &manager,
            ImportExecuteRequest {
                plan_id: prepared.plan_id,
                task_id: uuid::Uuid::new_v4().to_string(),
            },
        )
        .await
        .unwrap();
        assert_eq!(failed.task["phase"], "failed");
        assert_eq!(failed.failed_statement, Some(3));
        assert_eq!(failed.cleanup_state, "succeeded");
        let result = driver
            .query(
                &format!("SELECT COUNT(*) FROM {target}"),
                Some(&database),
                None,
            )
            .await
            .unwrap();
        assert_eq!(result.rows[0].values[0], Some(expected_count));
    };
    // 即使断言失败也先尝试清理本次随机名称对象，不依赖用户数据。
    use futures_util::FutureExt;
    let result = std::panic::AssertUnwindSafe(outcome).catch_unwind().await;
    let _ = driver
        .execute(
            &format!("DROP TABLE IF EXISTS {target}"),
            Some(&database),
            None,
        )
        .await;
    manager.disconnect(&id).await.unwrap();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}
#[tokio::test]
#[ignore = "需要 SQLINK_TEST_POSTGRES 指向独立实验数据库"]
async fn postgres_import_plan_uses_one_session_and_cleans_failure() {
    run("postgres").await;
}
#[tokio::test]
#[ignore = "需要 SQLINK_TEST_MYSQL 指向独立实验数据库"]
async fn mysql_import_plan_uses_one_session_and_cleans_failure() {
    run("mysql").await;
}
