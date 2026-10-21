//! PG/MySQL 独占会话真实回归。只使用显式 SQLINK_TEST_* 连接，不读取应用配置。
//! 显式执行缺少连接串必须失败：cargo test --test session_pg_mysql -- --ignored。
use reqwest::Url;
use sqlconnect_lib::database::{mysql::MySqlDriver, postgres::PgDriver, DatabaseDriver};
use sqlconnect_lib::models::{ConnectionConfig, DatabaseError, QueryResult};
use sqlconnect_lib::typed_value::TypedValue;
use std::sync::Arc;
use std::time::Duration;

type TestResult<T = ()> = Result<T, String>;
fn require(value: bool, message: &str) -> TestResult {
    if value {
        Ok(())
    } else {
        Err(message.into())
    }
}
fn component(value: &str) -> TestResult<String> {
    let mut result = Vec::new();
    let mut bytes = value.bytes();
    while let Some(byte) = bytes.next() {
        if byte == b'%' {
            let high = bytes
                .next()
                .and_then(|c| (c as char).to_digit(16))
                .ok_or("连接串编码无效")?;
            let low = bytes
                .next()
                .and_then(|c| (c as char).to_digit(16))
                .ok_or("连接串编码无效")?;
            result.push((high * 16 + low) as u8);
        } else {
            result.push(byte);
        }
    }
    String::from_utf8(result).map_err(|_| "连接串字段必须为UTF-8".into())
}
fn config(kind: &str) -> TestResult<ConnectionConfig> {
    let variable = if kind == "postgres" {
        "SQLINK_TEST_POSTGRES"
    } else {
        "SQLINK_TEST_MYSQL"
    };
    let raw = std::env::var(variable)
        .map_err(|_| format!("显式运行必须设置{variable}，并指定独立测试数据库"))?;
    let url = Url::parse(&raw).map_err(|_| "测试连接串无效")?;
    require(
        url.scheme() == kind || (kind == "postgres" && url.scheme() == "postgresql"),
        "测试连接串协议不符",
    )?;
    require(
        url.query().is_none() && url.fragment().is_none(),
        "测试连接串不支持额外参数",
    )?;
    let database = component(url.path().trim_start_matches('/'))?;
    require(!database.is_empty(), "必须指定独立测试数据库")?;
    Ok(ConnectionConfig {
        id: None,
        name: format!("{kind}独占会话回归"),
        driver_type: kind.into(),
        host: url
            .host_str()
            .ok_or("缺少主机")?
            .trim_start_matches('[')
            .trim_end_matches(']')
            .into(),
        port: url
            .port()
            .unwrap_or(if kind == "postgres" { 5432 } else { 3306 }),
        user: component(url.username())?,
        password: component(url.password().unwrap_or_default())?,
        database: Some(database),
        encrypt: None,
        save_password: Some(false),
        group_name: None,
        color: None,
        mcp_allowed: Some(false),
    })
}
fn number(result: QueryResult) -> TestResult<i64> {
    let value = result
        .rows
        .first()
        .and_then(|r| r.values.first())
        .and_then(|v| v.as_ref())
        .ok_or("结果没有值")?;
    value
        .as_i64()
        .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
        .ok_or_else(|| "结果不是整数".into())
}
async fn checks(driver: Arc<dyn DatabaseDriver>, kind: &str, table: &str) -> TestResult {
    let count = format!("SELECT COUNT(*) FROM {table}");
    let sleep_sql = if kind == "postgres" {
        "SELECT pg_sleep(30)".to_string()
    } else {
        // MySQL 的 SLEEP 投影被中断时可返回成功值 1；放入表查询谓词才返回 1317。
        format!("SELECT 1 FROM {table} WHERE SLEEP(30) = 0")
    };
    let sleep = sleep_sql.as_str();
    let identity = if kind == "postgres" {
        "SELECT pg_backend_pid()"
    } else {
        "SELECT CONNECTION_ID()"
    };
    let mut a = driver.lease_session(None).await.map_err(|_| "租约A失败")?;
    let mut b = driver.lease_session(None).await.map_err(|_| "租约B失败")?;
    require(a.session_id() != b.session_id(), "租约UUID不得重复")?;
    require(
        number(a.query(identity).await.map_err(|_| "读取A物理会话失败")?)?
            != number(b.query(identity).await.map_err(|_| "读取B物理会话失败")?)?,
        "并行租约共享物理会话",
    )?;
    a.begin().await.map_err(|_| "A开始事务失败")?;
    b.begin().await.map_err(|_| "B开始事务失败")?;
    a.execute(&format!("INSERT INTO {table} VALUES (1)"))
        .await
        .map_err(|_| "A插入失败")?;
    b.execute(&format!("INSERT INTO {table} VALUES (2)"))
        .await
        .map_err(|_| "B插入失败")?;
    b.commit().await.map_err(|_| "B提交失败")?;
    a.rollback().await.map_err(|_| "A回滚失败")?;
    a.release().await.map_err(|_| "归还A失败")?;
    b.release().await.map_err(|_| "归还B失败")?;
    require(
        number(
            driver
                .query(&count, None, None)
                .await
                .map_err(|_| "事务结果读取失败")?,
        )? == 1,
        "A回滚影响了B提交",
    )?;

    let temp = format!("{table}_tmp");
    let mut lease = driver
        .lease_session(None)
        .await
        .map_err(|_| "临时会话租用失败")?;
    let old_id = lease.session_id().to_string();
    lease
        .execute(&format!("CREATE TEMPORARY TABLE {temp} (id BIGINT)"))
        .await
        .map_err(|_| "创建临时表失败")?;
    let placeholder = if kind == "postgres" { "$1" } else { "?" };
    lease
        .execute_bound(
            &format!("INSERT INTO {temp} VALUES ({placeholder})"),
            &[TypedValue::Integer("9007199254740993".into())],
        )
        .await
        .map_err(|_| "同会话原生参数执行失败")?;
    let typed = lease
        .query_typed(&format!("SELECT id FROM {temp}"), &[])
        .await
        .map_err(|_| "同会话类型化读取失败")?;
    require(
        typed.typed_rows == vec![vec![Some(TypedValue::Integer("9007199254740993".into()))]],
        "原生类型编解码变更",
    )?;
    let old_handle = lease.cancel_handle().ok_or("未提供取消句柄")?;
    lease.release().await.map_err(|_| "健康归还失败")?;
    require(
        old_handle.cancel().await.is_err(),
        "已归还的旧取消句柄仍有效",
    )?;
    let mut next = driver
        .lease_session(None)
        .await
        .map_err(|_| "再次租用失败")?;
    require(next.session_id() != old_id, "复用连接时未生成新UUID")?;
    require(
        next.query(&format!("SELECT * FROM {temp}")).await.is_err(),
        "临时表泄漏到下一租约",
    )?;
    next.release().await.map_err(|_| "异常会话隔离失败")?;

    // 正常收到SQL错误后，执行future已经结束，同租约仍允许显式回滚。
    for prepare in [false, true] {
        let mut lease = driver
            .lease_session(None)
            .await
            .map_err(|_| "错误路径租用失败")?;
        lease.begin().await.map_err(|_| "错误路径开始事务失败")?;
        lease
            .execute(&format!("INSERT INTO {table} VALUES (3)"))
            .await
            .map_err(|_| "错误路径插入失败")?;
        if prepare {
            require(
                lease.query_typed("SELECT (", &[]).await.is_err(),
                "prepare错误未返回",
            )?;
        } else {
            require(
                lease
                    .execute(&format!("INSERT INTO {table} VALUES (2)"))
                    .await
                    .is_err(),
                "约束错误未返回",
            )?;
        }
        lease
            .rollback()
            .await
            .map_err(|_| "正常SQL错误后显式回滚失败")?;
        lease.release().await.map_err(|_| "SQL错误回滚后归还失败")?;
        require(
            number(
                driver
                    .query(&count, None, None)
                    .await
                    .map_err(|_| "错误后的新任务失败")?,
            )? == 1,
            "错误路径事务泄漏",
        )?;
    }

    for panic in [false, true] {
        let worker_driver = driver.clone();
        let insert = format!("INSERT INTO {table} VALUES (4)");
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let worker = tokio::spawn(async move {
            let mut lease = worker_driver.lease_session(None).await.unwrap();
            lease.begin().await.unwrap();
            lease.execute(&insert).await.unwrap();
            let _ = ready_tx.send(());
            if panic {
                panic!("受控租约析构回归");
            }
            std::future::pending::<()>().await;
        });
        ready_rx.await.map_err(|_| "任务未就绪")?;
        if !panic {
            worker.abort();
        }
        require(worker.await.is_err(), "任务应中断或panic")?;
        require(
            number(
                driver
                    .query(&count, None, None)
                    .await
                    .map_err(|_| "任务析构后查询失败")?,
            )? == 1,
            "任务Drop污染新租约",
        )?;
    }

    let mut lease = driver
        .lease_session(None)
        .await
        .map_err(|_| "future丢弃租用失败")?;
    require(
        tokio::time::timeout(Duration::from_millis(150), lease.query(sleep))
            .await
            .is_err(),
        "sleep未持续到future丢弃",
    )?;
    async fn reject_interrupted<T>(
        operation: impl std::future::Future<Output = Result<T, DatabaseError>>,
    ) -> TestResult {
        let result = tokio::time::timeout(Duration::from_millis(150), operation)
            .await
            .map_err(|_| "中断租约的后续操作未立即拒绝")?;
        require(
            matches!(result, Err(DatabaseError::ConnectionError(ref message)) if message.contains("上次操作尚未完成或已中断")),
            "中断租约继续发送了SQL，或错误原因不明确",
        )
    }
    reject_interrupted(lease.query("SELECT 1")).await?;
    reject_interrupted(lease.execute("SELECT 1")).await?;
    reject_interrupted(lease.query_typed("SELECT 1", &[])).await?;
    reject_interrupted(lease.execute_bound("SELECT 1", &[])).await?;
    reject_interrupted(lease.begin()).await?;
    reject_interrupted(lease.commit()).await?;
    reject_interrupted(lease.rollback()).await?;
    lease.release().await.map_err(|_| "future丢弃隔离失败")?;
    require(
        number(
            driver
                .query("SELECT 7", None, None)
                .await
                .map_err(|_| "future丢弃后新查询失败")?,
        )? == 7,
        "future丢弃后结果污染",
    )?;

    let mut lease = driver
        .lease_session(None)
        .await
        .map_err(|_| "取消路径租用失败")?;
    let handle = lease.cancel_handle().ok_or("未提供取消句柄")?;
    let cancel = async {
        tokio::time::sleep(Duration::from_millis(250)).await;
        handle.cancel().await
    };
    let (result, sent) = tokio::time::timeout(Duration::from_secs(8), async {
        tokio::join!(lease.query(sleep), cancel)
    })
    .await
    .map_err(|_| "查询取消超时")?;
    sent.map_err(|_| "取消请求失败")?;
    require(
        matches!(result, Err(DatabaseError::QueryCancelled)),
        "未从执行结果确认查询已取消",
    )?;
    lease.release().await.map_err(|_| "取消后隔离失败")?;
    require(handle.cancel().await.is_err(), "取消后旧句柄仍有效")?;
    require(
        number(
            driver
                .query("SELECT 8", None, None)
                .await
                .map_err(|_| "取消后新查询失败")?,
        )? == 8,
        "取消影响新租约",
    )?;

    require(
        driver
            .execute_batch(
                vec![
                    format!("INSERT INTO {table} VALUES (5)"),
                    format!("INSERT INTO {table} VALUES (2)"),
                ],
                None,
            )
            .await
            .is_err(),
        "批量约束错误应返回失败",
    )?;
    require(
        number(
            driver
                .query(&count, None, None)
                .await
                .map_err(|_| "批量失败后查询失败")?,
        )? == 1,
        "批量失败未回滚",
    )?;
    Ok(())
}
async fn run(kind: &str) -> TestResult {
    let config = config(kind)?;
    let driver: Arc<dyn DatabaseDriver> = if kind == "postgres" {
        Arc::new(
            PgDriver::connect(&config)
                .await
                .map_err(|_| "PG测试连接失败")?,
        )
    } else {
        Arc::new(
            MySqlDriver::connect(&config)
                .await
                .map_err(|_| "MySQL测试连接失败")?,
        )
    };
    let table = format!("sqlink_session_{}", uuid::Uuid::new_v4().simple());
    driver
        .execute(
            &format!("CREATE TABLE {table} (id BIGINT PRIMARY KEY)"),
            None,
            None,
        )
        .await
        .map_err(|_| "创建夹具失败")?;
    let result = async {
        // 同一连接池经历USE其他库后，缺省租用仍必须恢复配置的数据库。
        if kind == "mysql" {
            let other = driver
                .lease_session(Some("information_schema"))
                .await
                .map_err(|_| "切库租用失败")?;
            other.release().await.map_err(|_| "切库归还失败")?;
            let mut seen = std::collections::HashMap::new();
            let mut reused = false;
            for _ in 0..12 {
                let mut lease = driver
                    .lease_session(None)
                    .await
                    .map_err(|_| "复用租用失败")?;
                let physical = number(
                    lease
                        .query("SELECT CONNECTION_ID()")
                        .await
                        .map_err(|_| "复用会话标识失败")?,
                )?;
                let session_id = lease.session_id().to_string();
                if let Some(previous) = seen.insert(physical, session_id.clone()) {
                    require(previous != session_id, "物理连接复用时UUID未更新")?;
                    reused = true;
                }
                let database = lease
                    .query("SELECT DATABASE()")
                    .await
                    .map_err(|_| "默认库读取失败")?;
                let actual = database
                    .rows
                    .first()
                    .and_then(|r| r.values.first())
                    .and_then(|v| v.as_ref())
                    .and_then(|v| v.as_str());
                require(actual == config.database.as_deref(), "USE泄漏到缺省租约")?;
                lease.release().await.map_err(|_| "复用会话归还失败")?;
            }
            require(reused, "健康MySQL连接未发生物理复用")?;
        }
        checks(driver.clone(), kind, &table).await
    }
    .await;
    let cleanup = driver
        .execute(&format!("DROP TABLE {table}"), None, None)
        .await;
    let mut active = driver
        .lease_session(None)
        .await
        .map_err(|_| "断开回归租用失败")?;
    driver.disconnect().await.map_err(|_| "断开失败")?;
    require(driver.lease_session(None).await.is_err(), "断开后仍可租用")?;
    require(
        active.query("SELECT 1").await.is_err(),
        "断开后活动租约仍发布结果",
    )?;
    active.release().await.map_err(|_| "断开后隔离失败")?;
    result?;
    cleanup.map_err(|_| "夹具清理失败")?;
    Ok(())
}
#[tokio::test]
#[ignore = "需要显式SQLINK_TEST_POSTGRES独立测试数据库"]
async fn postgres_session_isolation_cancellation() {
    run("postgres").await.unwrap();
}
#[tokio::test]
#[ignore = "需要显式SQLINK_TEST_MYSQL独立测试数据库"]
async fn mysql_session_isolation_cancellation() {
    run("mysql").await.unwrap();
}
