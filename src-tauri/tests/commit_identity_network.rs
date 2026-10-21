//! 对象身份真实网络验证：仅使用 SQLINK_TEST_* 显式专用测试库。
//! 默认忽略；显式运行缺配置必须失败。测试仅创建和清理随机 schema 中的自有表。
//! MySQL 需 8.0/8.4、CREATE DATABASE 和读取 INNODB_TABLES 所需权限。
use reqwest::Url;
use sqlconnect_lib::database::{create_and_connect_driver, DatabaseDriver};
use sqlconnect_lib::models::{ConnectionConfig, ResolvedTable};
use std::sync::Arc;
use uuid::Uuid;

type TestResult<T = ()> = Result<T, String>;

// 校验失败通过 Result 退出，保证外围先清理测试表和连接再报告失败。
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

fn quoted(kind: &str, name: &str) -> String {
    match kind {
        "mysql" => format!("`{name}`"),
        "mssql" => format!("[{name}]"),
        _ => format!("\"{name}\""),
    }
}

async fn run(kind: &str, variable: &str) -> TestResult {
    let config = config_from_env(kind, variable, "对象身份测试")?;
    let database = config.database.clone().ok_or("测试库缺失")?;
    let driver = create_and_connect_driver(&config)
        .await
        .map_err(|_| "无法连接显式测试库")?;
    let nonce = Uuid::new_v4().simple().to_string();
    let schemas = [format!("ci_{nonce}_a"), format!("ci_{nonce}_b")];
    let table = "identity_rows";
    let names: Vec<_> = schemas
        .iter()
        .map(|schema| format!("{}.{}", quoted(kind, schema), quoted(kind, table)))
        .collect();
    let mut created = Vec::new();
    let test: TestResult = async {
        for schema in &schemas {
            driver.execute(&format!("CREATE {} {}", if kind == "mysql" { "DATABASE" } else { "SCHEMA" }, quoted(kind, schema)), Some(&database), None)
                .await.map_err(|_| "无法创建随机测试 schema")?;
            created.push(schema.clone());
        }
        for name in &names {
            driver.execute(&format!("CREATE TABLE {name} (id BIGINT NOT NULL PRIMARY KEY, payload VARCHAR(20) NULL){}", if kind == "mysql" { " ENGINE=InnoDB" } else { "" }), Some(&database), None)
                .await.map_err(|_| "无法创建自有测试表")?;
        }
        let mut targets = Vec::new();
        for schema in &schemas {
            let db = if kind == "mysql" { schema } else { &database };
            targets.push(driver.resolve_table(db, &[schema.clone(), table.into()]).await.map_err(|_| "表身份解析失败")?);
        }
        let before = inspect_and_lock(&driver, kind, &targets[0]).await?;
        let other = inspect_and_lock(&driver, kind, &targets[1]).await?;
        require(before != other, "不同 schema 的同名同结构表必须具有不同对象身份")?;
        // 同名同结构 DROP/CREATE 后不能沿用旧 token。
        driver.execute(&format!("DROP TABLE {}", names[0]), Some(&database), None).await.map_err(|_| "删除自有表失败")?;
        driver.execute(&format!("CREATE TABLE {} (id BIGINT NOT NULL PRIMARY KEY, payload VARCHAR(20) NULL){}", names[0], if kind == "mysql" { " ENGINE=InnoDB" } else { "" }), Some(&database), None)
            .await.map_err(|_| "重建自有表失败")?;
        let after = inspect_and_lock(&driver, kind, &targets[0]).await?;
        require(before != after, "同名同结构重建必须更换 token")?;
        if kind != "mysql" {
            // 未提交 ALTER 只在持有该物理会话的事务可见；若偷用元数据连接，此断言失败或等待自己。
            let mut lease = driver.lease_session(Some(&database)).await.map_err(|_| "租约获取失败")?;
            lease.begin_write().await.map_err(|_| "事务启动失败")?;
            let test: TestResult = async {
                lease.execute(&format!("ALTER TABLE {} ADD private_column BIGINT NULL", names[0])).await.map_err(|_| "事务内 ALTER 失败")?;
                let changed = lease.inspect_target(&targets[0], true).await.map_err(|_| "同会话目录读取失败")?;
                require(changed.table.columns.len() == 3 && changed.table.columns[2].name == "private_column", "inspect 必须读取当前 lease 内尚未提交的完整列目录")
            }.await;
            let rollback = lease.rollback().await;
            let release = lease.release().await;
            test?;
            require(rollback.is_ok() && release.is_ok(), "事务内元数据测试清理失败")?;
        }
        Ok(())
    }.await;
    // 无论断言是否通过，先清理自己的对象，再交付测试结果。
    for name in &names {
        let _ = driver
            .execute(&format!("DROP TABLE {name}"), Some(&database), None)
            .await;
    }
    for schema in created.iter().rev() {
        let _ = driver
            .execute(
                &format!(
                    "DROP {} {}",
                    if kind == "mysql" {
                        "DATABASE"
                    } else {
                        "SCHEMA"
                    },
                    quoted(kind, schema)
                ),
                Some(&database),
                None,
            )
            .await;
    }
    let disconnected = driver.disconnect().await;
    test?;
    require(disconnected.is_ok(), "测试连接断开失败")
}

async fn inspect_and_lock(
    driver: &Arc<dyn DatabaseDriver>,
    kind: &str,
    target: &ResolvedTable,
) -> TestResult<String> {
    let mut lease = driver
        .lease_session(Some(&target.database))
        .await
        .map_err(|_| "无法租用测试会话")?;
    lease.begin_write().await.map_err(|_| "事务启动失败")?;
    let test: TestResult<String> = async {
        let snapshot = lease
            .inspect_target(target, false)
            .await
            .map_err(|_| "基线目录读取失败（检查版本和目录权限）")?;
        require(
            serde_json::to_value(&snapshot.table).map_err(|_| "目录编码失败")?
                == serde_json::to_value(target).map_err(|_| "目录编码失败")?,
            "本 lease 列目录必须与 resolve_table 完全一致",
        )?;
        let locked = lease
            .inspect_target(target, true)
            .await
            .map_err(|_| "写入复核目录读取失败")?;
        require(
            snapshot.identity == locked.identity,
            "同事务两次检查身份应稳定",
        )?;
        require(
            locked
                .write_reasons
                .iter()
                .all(|r| r != "DRIVER_WRITE_VALIDATION_REQUIRED"),
            "适配器不得混入生产网络门禁",
        )?;
        let mut second = driver
            .lease_session(Some(&target.database))
            .await
            .map_err(|_| "并发租约获取失败")?;
        let timeout_sql = match kind {
            "postgres" => "SET lock_timeout = '250ms'",
            "mssql" => "SET LOCK_TIMEOUT 250",
            _ => "SET SESSION lock_wait_timeout = 1",
        };
        second
            .execute(timeout_sql)
            .await
            .map_err(|_| "设置并发锁超时失败")?;
        let schema = if kind == "mysql" {
            &target.database
        } else {
            target.schema.as_ref().ok_or("缺少明确schema")?
        };
        let name = format!("{}.{}", quoted(kind, schema), quoted(kind, &target.table));
        let blocked = second.execute(&format!("DROP TABLE {name}")).await;
        let _ = second.release().await;
        require(blocked.is_err(), "inspect 的事务对象锁必须阻止并发 DROP")?;
        let confirmed = lease
            .inspect_target(target, true)
            .await
            .map_err(|_| "并发锁等待后本租约应保持可用")?;
        require(
            confirmed.identity == locked.identity,
            "并发 DROP 失败后身份不能改变",
        )?;
        Ok(locked.identity)
    }
    .await;
    let rollback = lease.rollback().await;
    let release = lease.release().await;
    let token = test?;
    require(rollback.is_ok() && release.is_ok(), "身份测试租约清理失败")?;
    Ok(token)
}

#[tokio::test]
#[ignore = "需要显式 SQLINK_TEST_POSTGRES 专用网络测试库"]
async fn postgres_commit_identity_and_lease_locks() -> TestResult {
    run("postgres", "SQLINK_TEST_POSTGRES").await
}
#[tokio::test]
#[ignore = "需要显式 SQLINK_TEST_MYSQL 专用网络测试库及目录权限"]
async fn mysql_commit_identity_and_lease_locks() -> TestResult {
    run("mysql", "SQLINK_TEST_MYSQL").await
}
#[tokio::test]
#[ignore = "需要显式 SQLINK_TEST_MSSQL 专用网络测试库及目录权限"]
async fn mssql_commit_identity_and_lease_locks() -> TestResult {
    run("mssql", "SQLINK_TEST_MSSQL").await
}
