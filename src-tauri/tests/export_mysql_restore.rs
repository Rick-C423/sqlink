//! MySQL 跨库迁移回环：只在 SQLINK_TEST_MYSQL 显式实验库创建随机源表。
//! 需要 CREATE DATABASE 权限；随机目标库在结束时删除，连接串不写入错误信息。
use sqlconnect_lib::{
    commands::import::import_sql_file,
    database::{pool::ConnectionPoolManager, DatabaseDriver},
    export::export_job,
    models::ConnectionConfig,
    task_registry::TaskPhase,
    typed_value::{TimeValue, TimestampValue, TimezoneKind, TypedValue},
};
use std::path::PathBuf;
use uuid::Uuid;

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

fn config() -> TestResult<ConnectionConfig> {
    let raw = std::env::var("SQLINK_TEST_MYSQL")
        .map_err(|_| "显式运行必须设置 SQLINK_TEST_MYSQL，指向有建库权限的独立实验库")?;
    let options = mysql_async::Opts::from_url(&raw).map_err(|_| "MySQL 实验连接串无效")?;
    let database = options
        .db_name()
        .filter(|database| !database.is_empty())
        .ok_or("连接串必须明确指定独立实验库")?;
    serde_json::from_value(serde_json::json!({
        "id": Uuid::new_v4().to_string(),
        "name": "MySQL 跨库导出迁移实验",
        "driverType": "mysql",
        "host": options.ip_or_hostname(),
        "port": options.tcp_port(),
        "user": options.user().unwrap_or(""),
        "password": options.pass().unwrap_or(""),
        "database": database,
        "savePassword": false,
        "mcpAllowed": false
    }))
    .map_err(|_| "构造实验连接配置失败".into())
}

struct TestFiles {
    dump: PathBuf,
    import: PathBuf,
}
impl TestFiles {
    fn new() -> TestResult<Self> {
        let root = std::env::temp_dir()
            .canonicalize()
            .map_err(|_| "无法解析测试临时目录")?;
        let id = Uuid::new_v4().simple().to_string();
        Ok(Self {
            dump: root.join(format!("sqlink-cross-db-{id}.sql")),
            import: root.join(format!("sqlink-cross-db-{id}-no-backslash.sql")),
        })
    }
}
impl Drop for TestFiles {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.dump);
        let _ = std::fs::remove_file(&self.import);
    }
}

fn input_values(id: i64, null_text: TypedValue, text: &str) -> Vec<TypedValue> {
    vec![
        TypedValue::Integer(id.to_string()),
        TypedValue::Integer("9007199254740993".into()),
        TypedValue::UnsignedInteger(u64::MAX.to_string()),
        TypedValue::Decimal("12345678901234567890.12345678901234567890".into()),
        TypedValue::binary(&[0, 255, 39, 92]),
        TypedValue::binary(&[]),
        null_text,
        TypedValue::Text(String::new()),
        TypedValue::Text(text.into()),
        TypedValue::Date("2026-10-20".into()),
        TypedValue::Time(TimeValue {
            value: "12:34:56.123456".into(),
            precision: 6,
        }),
        TypedValue::Timestamp(TimestampValue {
            value: "2026-10-20T12:34:56.654321".into(),
            precision: 6,
            timezone: TimezoneKind::None,
        }),
    ]
}

async fn read_rows(
    driver: &dyn DatabaseDriver,
    database: &str,
    table: &str,
) -> TestResult<Vec<Vec<TypedValue>>> {
    let result = driver
        .query_typed(
            &format!(
                "SELECT * FROM {}.{} ORDER BY id",
                quote(database),
                quote(table)
            ),
            &[],
            Some(database),
        )
        .await
        .map_err(|_| "读取跨库迁移 typed 原值失败")?;
    require(!result.result.truncated, "验证查询不能截断")?;
    result
        .typed_rows
        .into_iter()
        .map(|row| {
            row.into_iter()
                .map(|value| value.ok_or_else(|| "原值含无法无损解码的列".into()))
                .collect()
        })
        .collect()
}

#[tokio::test]
#[ignore = "需要 SQLINK_TEST_MYSQL 指向有 CREATE DATABASE 权限的独立 MySQL 实验库"]
async fn mysql_dump_restores_into_selected_database_without_touching_source() -> TestResult {
    let source_config = config()?;
    let source_database = source_config.database.clone().ok_or("缺少源实验库")?;
    let table = format!("sqlink_migrate_{}", Uuid::new_v4().simple());
    let destination_database = format!("sqlink_restore_{}", Uuid::new_v4().simple());
    let source_manager = ConnectionPoolManager::new(None);
    let destination_manager = ConnectionPoolManager::new(None);
    let source_id = source_manager
        .create_connection(source_config.clone())
        .await
        .map_err(|_| "连接源实验库失败")?;
    let source = source_manager
        .get_connection(&source_id)
        .await
        .map_err(|_| "获取源实验库连接失败")?;
    let files = TestFiles::new()?;
    let mut destination_id = None;
    let outcome = async {
        source
            .execute(
                &format!("CREATE DATABASE {} CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci", quote(&destination_database)),
                Some(&source_database),
                None,
            )
            .await
            .map_err(|_| "创建随机目标实验库失败，显式测试要求 CREATE DATABASE 权限")?;
        source
            .execute(
                &format!(
                    "CREATE TABLE {}.{} (\
                     id INT PRIMARY KEY, big_value BIGINT NOT NULL, unsigned_value BIGINT UNSIGNED NOT NULL, \
                     decimal_value DECIMAL(40,20), binary_value VARBINARY(4), empty_binary VARBINARY(4), \
                     null_text TEXT NULL, empty_text TEXT, special_text TEXT, \
                     date_value DATE, time_value TIME(6), datetime_value DATETIME(6), \
                     generated_value BIGINT GENERATED ALWAYS AS (big_value + 1) STORED\
                     ) CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci",
                    quote(&source_database), quote(&table)
                ),
                Some(&source_database),
                None,
            )
            .await
            .map_err(|_| "创建随机源夹具失败")?;
        let inputs = [
            input_values(1, TypedValue::Null, "中文😀,\"双引号\"'单引号'\\反斜杠\n下一行\0末尾"),
            input_values(2, TypedValue::Text("NULL".into()), "\\N"),
        ];
        let mut expected = Vec::new();
        for values in inputs {
            source
                .execute_bound(
                    &format!(
                        "INSERT INTO {}.{} (id,big_value,unsigned_value,decimal_value,binary_value,empty_binary,null_text,empty_text,special_text,date_value,time_value,datetime_value) VALUES (?,?,?,?,?,?,?,?,?,?,?,?)",
                        quote(&source_database), quote(&table)
                    ),
                    &values,
                    Some(&source_database),
                )
                .await
                .map_err(|_| "写入随机源夹具失败")?;
            let mut row = values;
            row.push(TypedValue::Integer("9007199254740994".into()));
            expected.push(row);
        }
        let source_before = read_rows(source.as_ref(), &source_database, &table).await?;
        require(source_before == expected, "源夹具必须先证明全部 typed 值精确")?;

        let task_id = Uuid::new_v4().to_string();
        let summary = export_job(
            &source_manager,
            source_id.clone(),
            Some(source_database.clone()),
            Some(vec![table.clone()]),
            files.dump.to_str().ok_or("SQL 导出路径不是 UTF-8")?.to_owned(),
            true,
            true,
            true,
            Some(task_id.clone()),
        )
        .await
        .map_err(|_| "跨库迁移 SQL 文件导出失败")?;
        require(summary.tables == 1 && summary.rows == 2, "导出摘要必须包含一表两行")?;
        require(
            source_manager.tasks.snapshot(&task_id).map_err(|_| "读取导出任务终态失败")?.phase == TaskPhase::Succeeded,
            "导出任务必须在文件提交后成功",
        )?;
        let dump = std::fs::read_to_string(&files.dump).map_err(|_| "读取已提交 SQL 文件失败")?;
        require(
            dump.contains(&format!("INSERT INTO {} (", quote(&table))),
            "恢复 INSERT 必须使用与建表 DDL 一致的未限定表名",
        )?;
        require(
            !dump.contains(&format!("INSERT INTO {}.", quote(&source_database))),
            "恢复 INSERT 不能指向原实验库",
        )?;
        let mut dump_reader = std::io::Cursor::new(dump.as_bytes());
        let mut splitter = sqlconnect_lib::database::dump::SqlSplitter::new(true);
        while let Some(statement) = splitter.next_statement(&mut dump_reader).map_err(|_| "转储语句必须可拆分")? {
            require(sqlconnect_lib::database::first_keyword(&statement) != "SELECT", "转储不得执行源读取 SELECT")?;
        }

        let mut destination_config = source_config.clone();
        destination_config.id = Some(Uuid::new_v4().to_string());
        destination_config.database = Some(destination_database.clone());
        let id = destination_manager
            .create_connection(destination_config)
            .await
            .map_err(|_| "连接随机目标实验库失败")?;
        destination_id = Some(id.clone());
        // SET 与转储正文由同一次导入、同一租约执行，验证文本编码不依赖反斜杠模式。
        std::fs::write(
            &files.import,
            format!("SET SESSION sql_mode = 'NO_BACKSLASH_ESCAPES';\n{dump}"),
        )
        .map_err(|_| "写入 SQL 模式验证文件失败")?;
        let imported = import_sql_file(
            &destination_manager,
            id.clone(),
            Some(destination_database.clone()),
            files.import.to_str().ok_or("SQL 导入路径不是 UTF-8")?.to_owned(),
            "error".into(),
        )
        .await
        .map_err(|_| "把转储导入随机目标库失败")?;
        require(imported.executed >= 4 && imported.skipped == 0, "必须实际执行 SET、DROP、CREATE 和 INSERT")?;
        let destination = destination_manager.get_connection(&id).await.map_err(|_| "获取目标库验证连接失败")?;
        let source_after = read_rows(source.as_ref(), &source_database, &table).await?;
        let restored = read_rows(destination.as_ref(), &destination_database, &table).await?;
        require(source_after == source_before, "迁移不得改变源库行数或任何原值")?;
        require(restored == source_before, "目标库原值和重新计算的生成列必须逐项等于源库")?;
        Ok::<_, String>(())
    }
    .await;

    // 所有校验均返回 Result，先清理本次随机对象，再向测试框架报告失败。
    let destination_cleanup = if let Some(id) = destination_id {
        destination_manager.disconnect(&id).await
    } else {
        Ok(())
    };
    let source_cleanup = source
        .execute(
            &format!(
                "DROP TABLE IF EXISTS {}.{}",
                quote(&source_database),
                quote(&table)
            ),
            Some(&source_database),
            None,
        )
        .await;
    let database_cleanup = source
        .execute(
            &format!("DROP DATABASE IF EXISTS {}", quote(&destination_database)),
            Some(&source_database),
            None,
        )
        .await;
    let source_disconnect = source_manager.disconnect(&source_id).await;
    outcome?;
    destination_cleanup.map_err(|_| "断开目标实验库失败")?;
    source_cleanup.map_err(|_| "清理随机源夹具失败")?;
    database_cleanup.map_err(|_| "清理随机目标实验库失败")?;
    source_disconnect.map_err(|_| "断开源实验库失败")?;
    Ok(())
}
