//! SQL Server 独占租约的真实网络验证；只读取显式 SQLINK_TEST_MSSQL 专用测试库。
//! 默认忽略；显式运行缺少环境变量时失败，绝不回退到应用默认连接。
use reqwest::Url;
use sqlconnect_lib::database::{create_and_connect_driver, DatabaseDriver};
use sqlconnect_lib::models::{ConnectionConfig, QueryResult};
use sqlconnect_lib::typed_value::TypedValue;
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

type TestResult<T = ()> = Result<T, String>;
fn require(condition: bool, message: &str) -> TestResult {
    if condition {
        Ok(())
    } else {
        Err(message.into())
    }
}
fn decode_component(encoded: &str) -> TestResult<String> {
    let mut decoded = Vec::new();
    let mut bytes = encoded.bytes();
    while let Some(byte) = bytes.next() {
        if byte == b'%' {
            let high = bytes.next().and_then(|byte| (byte as char).to_digit(16));
            let low = bytes.next().and_then(|byte| (byte as char).to_digit(16));
            match (high, low) {
                (Some(high), Some(low)) => decoded.push((high * 16 + low) as u8),
                _ => return Err("连接串含非法百分号编码".into()),
            }
        } else {
            decoded.push(byte);
        }
    }
    String::from_utf8(decoded).map_err(|_| "连接串字段不是有效 UTF-8".into())
}

fn config_from_env(driver: &str, variable: &str, id: &str) -> TestResult<ConnectionConfig> {
    let raw =
        std::env::var(variable).map_err(|_| format!("必须设置 {variable} 才能显式运行此测试"))?;
    // 不输出原始连接串或 URL 解析错误，避免失败日志泄露凭据。
    let url = Url::parse(&raw).map_err(|_| format!("{variable} 不是有效测试连接串"))?;
    let valid_scheme = match driver {
        "postgres" => matches!(url.scheme(), "postgres" | "postgresql"),
        "mssql" => matches!(url.scheme(), "mssql" | "sqlserver"),
        _ => url.scheme() == driver,
    };
    require(valid_scheme, "测试连接串协议与驱动不符")?;
    require(url.fragment().is_none(), "测试连接串不支持片段")?;
    let host = url.host_str().ok_or("测试连接串缺少主机")?;
    let database = decode_component(url.path().trim_start_matches('/'))?;
    require(!database.is_empty(), "测试连接串必须显式指定数据库")?;
    let mut encrypt = None;
    for (name, value) in url.query_pairs() {
        match (driver, name.as_ref(), value.as_ref()) {
            ("mssql", "encrypt", "true") => encrypt = Some(true),
            ("mssql", "encrypt", "false") => encrypt = Some(false),
            _ => return Err("测试连接串包含不支持的参数".into()),
        }
    }
    Ok(ConnectionConfig {
        id: Some(id.into()),
        name: format!("受控结果网络测试 {driver}"),
        driver_type: driver.into(),
        host: host.trim_start_matches('[').trim_end_matches(']').into(),
        port: url.port().unwrap_or(match driver {
            "postgres" => 5432,
            "mssql" => 1433,
            _ => 3306,
        }),
        user: decode_component(url.username())?,
        password: decode_component(url.password().unwrap_or_default())?,
        database: Some(database),
        encrypt,
        save_password: Some(false),
        group_name: None,
        color: None,
        mcp_allowed: Some(false),
    })
}

// 外层测试超时也须终止拥有数据库租约的任务，避免 JoinHandle drop 后继续后台运行。
struct AbortOnDrop(Option<tokio::task::JoinHandle<TestResult>>);
impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        if let Some(task) = self.0.take() {
            task.abort();
        }
    }
}

fn first_integer(result: &QueryResult) -> Option<i64> {
    result.rows.first()?.values.first()?.as_ref()?.as_i64()
}

async fn exercise(driver: Arc<dyn DatabaseDriver>, table: &str, database: &str) -> TestResult {
    let caps = driver.session_capabilities();
    require(
        caps.exclusive_session && !caps.cancellation,
        "SQL Server 独占会话必须明确禁用取消",
    )?;
    require(
        caps.cancellation_reason
            .as_deref()
            .is_some_and(|text| text.contains("不代表服务端查询已经停止")),
        "取消限制原因必须明确",
    )?;
    let cancel_error = driver
        .cancel_query(1)
        .await
        .err()
        .ok_or("SQL Server 不得假装取消成功")?;
    require(
        cancel_error.to_string().contains("未实现可靠的服务端取消"),
        "取消失败信息不明确",
    )?;

    let mut first = driver
        .lease_session(Some(database))
        .await
        .map_err(|_| "第一个租约创建失败")?;
    let mut second = driver
        .lease_session(Some(database))
        .await
        .map_err(|_| "第二个租约创建失败")?;
    require(
        first.session_id() != second.session_id(),
        "每个租约应具有独立随机标识",
    )?;
    require(
        Uuid::parse_str(first.session_id()).is_ok(),
        "租约标识应为 UUID",
    )?;
    require(
        !first.capabilities().cancellation && first.cancel_handle().is_none(),
        "租约不得暴露虚假取消句柄",
    )?;
    let first_spid = first_integer(
        &first
            .query("SELECT @@SPID")
            .await
            .map_err(|_| "读取首租约会话失败")?,
    );
    let second_spid = first_integer(
        &second
            .query("SELECT @@SPID")
            .await
            .map_err(|_| "读取次租约会话失败")?,
    );
    require(
        first_spid.is_some() && first_spid != second_spid,
        "两个活跃租约不得共用物理会话",
    )?;

    first
        .query("CREATE TABLE #sqlink_session_local (id int); SET LOCK_TIMEOUT 1234")
        .await
        .map_err(|_| "创建会话临时对象失败")?;
    first
        .execute_bound(
            "INSERT INTO #sqlink_session_local VALUES (@P1)",
            &[TypedValue::Integer("12".into())],
        )
        .await
        .map_err(|_| "同租约绑定写入失败")?;
    let local = first
        .query_typed("SELECT id, @@LOCK_TIMEOUT FROM #sqlink_session_local", &[])
        .await
        .map_err(|_| "同租约临时对象读取失败")?;
    require(
        local.typed_rows.first()
            == Some(&vec![
                Some(TypedValue::Integer("12".into())),
                Some(TypedValue::Integer("1234".into())),
            ]),
        "同租约临时表或设置未连续保留",
    )?;
    let absent = second
        .query("SELECT OBJECT_ID('tempdb..#sqlink_session_local')")
        .await
        .map_err(|_| "次租约临时对象隔离检查失败")?;
    require(
        absent
            .rows
            .first()
            .and_then(|row| row.values.first())
            .and_then(Option::as_ref)
            .is_some_and(serde_json::Value::is_null),
        "临时对象泄漏到其他租约",
    )?;
    let empty = first
        .query_typed("SELECT id FROM #sqlink_session_local WHERE 1 = 0", &[])
        .await
        .map_err(|_| "零行结果读取失败")?;
    require(
        empty.result.columns.len() == 1 && empty.typed_rows.is_empty(),
        "零行类型化结果须保留列元数据",
    )?;
    let empty_legacy = first
        .query("SELECT id FROM #sqlink_session_local WHERE 1 = 0")
        .await
        .map_err(|_| "零行普通结果读取失败")?;
    require(
        empty_legacy.columns.len() == 1 && empty_legacy.rows.is_empty(),
        "零行普通结果须保留列元数据",
    )?;

    first.begin().await.map_err(|_| "首租约事务开启失败")?;
    driver
        .get_tables(Some(database))
        .await
        .map_err(|_| "元数据独立会话读取失败")?;
    let still_open = first
        .query("SELECT @@TRANCOUNT")
        .await
        .map_err(|_| "检查租约事务失败")?;
    require(
        first_integer(&still_open) == Some(1),
        "元数据调用不得进入或改变租约事务",
    )?;
    second.begin().await.map_err(|_| "次租约事务开启失败")?;
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let (committed, rolled_back) = tokio::join!(
        async {
            first
                .execute(&format!("INSERT INTO {table} VALUES (1)"))
                .await
                .map_err(|_| "首事务插入失败")?;
            barrier.wait().await;
            first.commit().await.map_err(|_| "首事务提交失败")?;
            first.release().await.map_err(|_| "首租约归还失败")
        },
        async {
            second
                .execute(&format!("INSERT INTO {table} VALUES (2)"))
                .await
                .map_err(|_| "次事务插入失败")?;
            barrier.wait().await;
            second.rollback().await.map_err(|_| "次事务回滚失败")?;
            second.release().await.map_err(|_| "次租约归还失败")
        }
    );
    committed?;
    rolled_back?;
    let state = driver
        .query(
            &format!("SELECT COUNT(*) FROM {table} WHERE id = 1"),
            Some(database),
            None,
        )
        .await
        .map_err(|_| "读取提交结果失败")?;
    require(
        first_integer(&state) == Some(1),
        "次租约回滚不得影响首租约提交",
    )?;
    let state = driver
        .query(
            &format!("SELECT COUNT(*) FROM {table} WHERE id = 2"),
            Some(database),
            None,
        )
        .await
        .map_err(|_| "读取回滚结果失败")?;
    require(
        first_integer(&state) == Some(0),
        "首租约提交不得提交次租约内容",
    )?;

    for (id, explicit_release) in [(3, false), (4, true)] {
        let mut lease = driver
            .lease_session(Some(database))
            .await
            .map_err(|_| "清理场景租约创建失败")?;
        lease.begin().await.map_err(|_| "清理场景事务开启失败")?;
        lease
            .execute(&format!("INSERT INTO {table} VALUES ({id})"))
            .await
            .map_err(|_| "清理场景插入失败")?;
        if explicit_release {
            lease.release().await.map_err(|_| "活动事务正常归还失败")?;
        } else {
            drop(lease);
        }
    }
    let mut failed = driver
        .lease_session(Some(database))
        .await
        .map_err(|_| "错误场景租约创建失败")?;
    failed.begin().await.map_err(|_| "错误场景事务开启失败")?;
    failed
        .execute(&format!("INSERT INTO {table} VALUES (5)"))
        .await
        .map_err(|_| "错误场景插入失败")?;
    require(
        failed
            .execute(&format!(
                "SELECT * FROM [missing_{}]",
                Uuid::new_v4().simple()
            ))
            .await
            .is_err(),
        "预期 SQL 错误未发生",
    )?;
    require(failed.query("SELECT 1").await.is_err(), "失败会话必须隔离")?;
    drop(failed);

    // 只丢弃借用操作的 future 时，也必须丢弃该操作独占的 socket。
    let mut borrowed = driver
        .lease_session(Some(database))
        .await
        .map_err(|_| "借用 future 场景租约创建失败")?;
    require(
        tokio::time::timeout(
            Duration::from_millis(150),
            borrowed.query("WAITFOR DELAY '00:00:20'; SELECT 1"),
        )
        .await
        .is_err(),
        "预期借用操作超时未发生",
    )?;
    require(
        borrowed.query("SELECT 1").await.is_err(),
        "被丢弃的操作不得继续使用不确定 TDS 流",
    )?;
    drop(borrowed);

    let abort_driver = driver.clone();
    let abort_table = table.to_string();
    let abort_database = database.to_string();
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let mut task = AbortOnDrop(Some(tokio::spawn(async move {
        let mut lease = abort_driver
            .lease_session(Some(&abort_database))
            .await
            .map_err(|_| "abort 租约创建失败")?;
        lease.begin().await.map_err(|_| "abort 事务开启失败")?;
        lease
            .execute(&format!("INSERT INTO {abort_table} VALUES (6)"))
            .await
            .map_err(|_| "abort 场景插入失败")?;
        ready_tx.send(()).map_err(|_| "abort 通知失败")?;
        lease
            .query("WAITFOR DELAY '00:00:20'; SELECT 1")
            .await
            .map_err(|_| "abort 等待查询失败")?;
        Ok::<(), String>(())
    })));
    ready_rx.await.map_err(|_| "abort 场景准备失败")?;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let task = task.0.take().ok_or("abort 任务句柄缺失")?;
    task.abort();
    require(
        task.await.is_err_and(|error| error.is_cancelled()),
        "任务 abort 未生效",
    )?;

    require(
        driver
            .execute_batch(
                vec![
                    format!("INSERT INTO {table} VALUES (7)"),
                    format!(
                        "INSERT INTO [missing_{}] VALUES (7)",
                        Uuid::new_v4().simple()
                    ),
                ],
                Some(database),
            )
            .await
            .is_err(),
        "失败批次应返回原始错误",
    )?;
    let mut next = driver
        .lease_session(Some(database))
        .await
        .map_err(|_| "下一任务租约创建失败")?;
    next.query("SET LOCK_TIMEOUT 5000")
        .await
        .map_err(|_| "后续检查设置失败")?;
    let clean = tokio::time::timeout(
        Duration::from_secs(10),
        next.query(&format!(
            "SELECT COUNT(*) FROM {table} WHERE id IN (2,3,4,5,6,7)"
        )),
    )
    .await
    .map_err(|_| "残留事务阻塞后续任务")?
    .map_err(|_| "后续任务读取失败")?;
    require(
        first_integer(&clean) == Some(0),
        "Drop、release、错误、abort 或批次失败遗留了写入",
    )?;
    let clean_transaction = next
        .query("SELECT @@TRANCOUNT")
        .await
        .map_err(|_| "后续事务状态读取失败")?;
    require(
        first_integer(&clean_transaction) == Some(0),
        "后续任务继承了事务",
    )?;
    next.release().await.map_err(|_| "后续任务归还失败")?;
    Ok(())
}

#[tokio::test]
#[ignore = "需要 SQLINK_TEST_MSSQL 指向专用 SQL Server 测试库"]
async fn mssql_session_isolation_and_lifecycle() -> TestResult {
    let config = config_from_env("mssql", "SQLINK_TEST_MSSQL", "session-mssql")?;
    let driver = create_and_connect_driver(&config)
        .await
        .map_err(|_| "专用测试连接创建失败")?;
    let database = config.database.as_deref().ok_or("专用测试数据库缺失")?;
    let table = format!("[sqlink_session_{}]", Uuid::new_v4().simple());
    driver
        .execute(
            &format!("CREATE TABLE {table} (id int NOT NULL)"),
            Some(database),
            None,
        )
        .await
        .map_err(|_| "创建随机测试表失败")?;
    let result = tokio::time::timeout(
        Duration::from_secs(60),
        exercise(driver.clone(), &table, database),
    )
    .await
    .map_err(|_| "租约网络测试超过 60 秒".to_string())
    .and_then(|result| result);
    let cleanup = driver
        .execute(&format!("DROP TABLE {table}"), Some(database), None)
        .await
        .map_err(|_| "随机测试表清理失败");
    let disconnected = driver.disconnect().await.map_err(|_| "测试连接关闭失败");
    let rejected = driver.lease_session(Some(database)).await.is_err();
    result?;
    cleanup?;
    disconnected?;
    require(rejected, "断开后的驱动必须拒绝新租约")
}
