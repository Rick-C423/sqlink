//! 仅显式执行：两个独立实验库中的同名对象不能影响 skip 的目标存在性判断。
//! 不读取应用保存连接，不打印账号、密码或连接串；清理始终使用预先固定的完整库表名。
use sqlconnect_lib::{
    commands::import::import_sql_file,
    database::{pool::ConnectionPoolManager, DatabaseDriver},
    models::{ConnectionConfig, ImportSummary},
};
use std::{path::PathBuf, sync::Arc, time::Duration};

type TestResult<T = ()> = Result<T, String>;

fn require(condition: bool, message: &str) -> TestResult {
    if condition {
        Ok(())
    } else {
        Err(message.into())
    }
}
fn quote(name: &str) -> String {
    format!("`{}`", name.replace('`', "``"))
}

struct SqlFile(PathBuf);
impl SqlFile {
    fn new(sql: &str) -> TestResult<Self> {
        let path = std::env::temp_dir().join(format!(
            "sqlink-import-skip-mysql-{}.sql",
            uuid::Uuid::new_v4()
        ));
        std::fs::write(&path, sql).map_err(|_| "创建临时导入 SQL 文件失败")?;
        Ok(Self(path))
    }
}
impl Drop for SqlFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

struct Fixture {
    manager: ConnectionPoolManager,
    driver: Arc<dyn DatabaseDriver>,
    connection_id: String,
    database: String,
    other_database: String,
    table: String,
}
impl Fixture {
    async fn new() -> TestResult<Self> {
        let raw = std::env::var("SQLINK_TEST_MYSQL")
            .map_err(|_| "显式运行必须设置 SQLINK_TEST_MYSQL 独立实验库，缺环境不得跳过成功")?;
        let options = mysql_async::Opts::from_url(&raw).map_err(|_| "MySQL 实验连接串无效")?;
        let database = options
            .db_name()
            .filter(|name| !name.is_empty())
            .ok_or("必须在连接串中明确指定独立实验库")?
            .to_owned();
        let other_database = std::env::var("SQLINK_TEST_MYSQL_OTHER_DATABASE")
            .map_err(|_| "跨库回归必须设置 SQLINK_TEST_MYSQL_OTHER_DATABASE，缺环境不得跳过成功")?;
        require(
            !other_database.is_empty() && other_database != database,
            "两实验库必须显式指定且互不相同",
        )?;
        let connection_id = uuid::Uuid::new_v4().to_string();
        let config: ConnectionConfig = serde_json::from_value(serde_json::json!({
            "id":connection_id, "name":"MySQL导入skip完整身份回归", "driverType":"mysql",
            "host":options.ip_or_hostname(), "port":options.tcp_port(),
            "user":options.user().unwrap_or(""), "password":options.pass().unwrap_or(""),
            "database":database, "savePassword":false, "mcpAllowed":false
        }))
        .map_err(|_| "构造实验连接配置失败")?;
        let manager = ConnectionPoolManager::new(None);
        manager
            .create_connection(config)
            .await
            .map_err(|_| "独立实验库连接失败")?;
        let driver = manager
            .get_connection(&connection_id)
            .await
            .map_err(|_| "获取实验连接失败")?;
        let table = format!("skip_中文_'`\"_{}", uuid::Uuid::new_v4().simple());
        Ok(Self {
            manager,
            driver,
            connection_id,
            database,
            other_database,
            table,
        })
    }

    fn target(&self, database: &str) -> String {
        format!("{}.{}", quote(database), quote(&self.table))
    }

    async fn execute(&self, sql: &str) -> TestResult {
        self.driver
            .execute(sql, None, None)
            .await
            .map_err(|_| "执行固定目标的实验 SQL 失败")?;
        Ok(())
    }

    async fn create(&self, database: &str, value: i64) -> TestResult {
        self.execute(&format!(
            "CREATE TABLE {} (value BIGINT NOT NULL) ENGINE=InnoDB",
            self.target(database)
        ))
        .await?;
        self.execute(&format!(
            "INSERT INTO {} VALUES ({value})",
            self.target(database)
        ))
        .await
    }

    async fn import(&self, sql: &str) -> TestResult<ImportSummary> {
        let file = SqlFile::new(sql)?;
        tokio::time::timeout(
            Duration::from_secs(15),
            import_sql_file(
                &self.manager,
                self.connection_id.clone(),
                Some(self.database.clone()),
                file.0.to_string_lossy().into_owned(),
                "skip".into(),
            ),
        )
        .await
        .map_err(|_| "skip 导入或单租约归还超时")?
        .map_err(|_| "skip 导入失败：应按当前会话中的完整目标身份判断存在性".to_owned())
    }

    async fn expect_values(&self, database: &str, expected: &[i64]) -> TestResult {
        let result = self
            .driver
            .query_typed(
                &format!("SELECT value FROM {} ORDER BY value", self.target(database)),
                &[],
                None,
            )
            .await
            .map_err(|_| "读取固定库表验证值失败")?;
        require(!result.result.truncated, "实验验证查询不得被截断")?;
        let actual = result
            .typed_rows
            .iter()
            .map(|row| {
                row.first()
                    .and_then(|v| v.as_ref())
                    .and_then(|v| match v {
                        sqlconnect_lib::typed_value::TypedValue::Integer(value) => {
                            value.parse::<i64>().ok()
                        }
                        _ => None,
                    })
                    .ok_or_else(|| "实验验证值类型错误".to_string())
            })
            .collect::<TestResult<Vec<_>>>()?;
        require(
            actual == expected,
            "固定来源表的数据与预期不符，可能误跳 CREATE 或数据写入了另一库",
        )
    }

    async fn finish(self, result: TestResult) -> TestResult {
        // 即使业务断言失败，也只清理本测试持有的两个完整来源；不采用导入结束后的默认库。
        let mut cleanup_failed = false;
        for database in [&self.database, &self.other_database] {
            cleanup_failed |= self
                .driver
                .execute(
                    &format!("DROP TABLE IF EXISTS {}", self.target(database)),
                    None,
                    None,
                )
                .await
                .is_err();
        }
        cleanup_failed |= self.manager.disconnect(&self.connection_id).await.is_err();
        match (result, cleanup_failed) {
            (Err(error), true) => Err(format!("{error}；本轮随机表清理失败，请核对固定实验来源")),
            (Err(error), false) => Err(error),
            (Ok(()), true) => Err("本轮随机表清理或实验连接断开失败".into()),
            (Ok(()), false) => Ok(()),
        }
    }
}

fn expect_counts(
    summary: &ImportSummary,
    statements: u64,
    executed: u64,
    skipped: u64,
) -> TestResult {
    require(
        (summary.statements, summary.executed, summary.skipped) == (statements, executed, skipped),
        "导入统计不符合预期：skip 只能省略真实已存在目标的 CREATE，不能省略 USE 或 INSERT",
    )
}

#[tokio::test]
#[ignore = "需要显式配置两个独立 MySQL 实验库"]
async fn existing_main_name_does_not_skip_missing_qualified_other_target() -> TestResult {
    let fixture = Fixture::new().await?;
    let result = async {
        fixture.create(&fixture.database, 7).await?;
        let target = fixture.target(&fixture.other_database);
        let summary = fixture.import(&format!("CREATE TABLE {target} (value BIGINT NOT NULL) ENGINE=InnoDB; INSERT INTO {target} VALUES (11);")).await?;
        expect_counts(&summary, 2, 2, 0)?;
        fixture.expect_values(&fixture.database, &[7]).await?;
        fixture.expect_values(&fixture.other_database, &[11]).await
    }.await;
    fixture.finish(result).await
}

#[tokio::test]
#[ignore = "需要显式配置两个独立 MySQL 实验库"]
async fn existing_exact_qualified_other_target_skips_only_create_and_imports_data() -> TestResult {
    let fixture = Fixture::new().await?;
    let result = async {
        fixture.create(&fixture.database, 7).await?;
        fixture.create(&fixture.other_database, 5).await?;
        let target = fixture.target(&fixture.other_database);
        let summary = fixture.import(&format!("CREATE TABLE {target} (wrong_column TEXT) ENGINE=InnoDB; INSERT INTO {target} VALUES (11);")).await?;
        expect_counts(&summary, 2, 1, 1)?;
        fixture.expect_values(&fixture.database, &[7]).await?;
        fixture.expect_values(&fixture.other_database, &[5, 11]).await
    }.await;
    fixture.finish(result).await
}

#[tokio::test]
#[ignore = "需要显式配置两个独立 MySQL 实验库"]
async fn use_changes_unqualified_create_lookup_and_keeps_file_in_one_session() -> TestResult {
    let fixture = Fixture::new().await?;
    let result = async {
        fixture.create(&fixture.database, 7).await?;
        let table = quote(&fixture.table);
        let summary = fixture.import(&format!("USE {}; CREATE TABLE {table} (value BIGINT NOT NULL) ENGINE=InnoDB; INSERT INTO {table} VALUES (11);", quote(&fixture.other_database))).await?;
        expect_counts(&summary, 3, 3, 0)?;
        fixture.expect_values(&fixture.database, &[7]).await?;
        fixture.expect_values(&fixture.other_database, &[11]).await?;
        let default = fixture.driver.query("SELECT DATABASE()", None, None).await.map_err(|_| "验证租约归还后的默认库失败")?;
        require(default.rows.first().and_then(|row| row.values.first()).and_then(|v| v.as_ref()).and_then(|v| v.as_str()) == Some(fixture.database.as_str()),
            "USE 的会话状态不得泄漏到后续租约")
    }.await;
    fixture.finish(result).await
}

#[tokio::test]
#[ignore = "需要显式配置两个独立 MySQL 实验库"]
async fn repeated_create_in_one_script_rechecks_current_existence_after_prior_ddl() -> TestResult {
    let fixture = Fixture::new().await?;
    let result = async {
        let table = quote(&fixture.table);
        let summary = fixture.import(&format!("USE {}; CREATE TABLE {table} (value BIGINT NOT NULL) ENGINE=InnoDB; INSERT INTO {table} VALUES (1); CREATE TABLE {table} (wrong_column TEXT) ENGINE=InnoDB; INSERT INTO {table} VALUES (2);", quote(&fixture.other_database))).await?;
        expect_counts(&summary, 5, 4, 1)?;
        fixture.expect_values(&fixture.other_database, &[1, 2]).await?;
        // 用完整身份查询主库，确认重复 CREATE 没有因会话切换而错误建立主库表。
        let rows = fixture.driver.query_typed(
            "SELECT COUNT(*) FROM information_schema.TABLES WHERE BINARY TABLE_SCHEMA = BINARY ? AND BINARY TABLE_NAME = BINARY ?",
            &[sqlconnect_lib::typed_value::TypedValue::Text(fixture.database.clone()), sqlconnect_lib::typed_value::TypedValue::Text(fixture.table.clone())], None,
        ).await.map_err(|_| "验证主库未被误写失败")?;
        require(rows.result.rows.first().and_then(|row| row.values.first()).and_then(|v| v.as_ref()).and_then(|v| v.as_i64()) == Some(0),
            "脚本内重复 CREATE 不应在原主库创建同名表")
    }.await;
    fixture.finish(result).await
}
