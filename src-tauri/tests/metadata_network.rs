//! 显式启用的 PG/SQL Server 完整元数据身份集成测试。
//! 仅连接 SQLINK_TEST_POSTGRES / SQLINK_TEST_MSSQL 指定的专用测试库。
//! 缺少环境变量时显式运行会失败；只清理本次随机 schema、表和视图。

use reqwest::Url;
use sqlconnect_lib::database::pool::ConnectionPoolManager;
use sqlconnect_lib::database::DatabaseDriver;
use sqlconnect_lib::models::{ConnectionConfig, DatabaseError};
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

fn quote(driver: &str, identifier: &str) -> String {
    if driver == "postgres" {
        format!("\"{}\"", identifier.replace('"', "\"\""))
    } else {
        format!("[{}]", identifier.replace(']', "]]"))
    }
}

fn qualified(driver: &str, schema: &str, table: &str) -> String {
    format!("{}.{}", quote(driver, schema), quote(driver, table))
}

fn metadata_error(error: DatabaseError) -> String {
    format!("元数据请求失败：{error}")
}

async fn ddl(driver: &dyn DatabaseDriver, database: &str, sql: &str) -> TestResult {
    // SQL 均由本测试生成，不在失败日志中输出可能含连接上下文的底层错误。
    driver
        .execute(sql, Some(database), None)
        .await
        .map_err(|_| "测试夹具 DDL 执行失败".to_string())?;
    Ok(())
}

async fn create(
    driver: &dyn DatabaseDriver,
    database: &str,
    sql: String,
    drop_sql: String,
    cleanup: &mut Vec<String>,
) -> TestResult {
    ddl(driver, database, &sql).await?;
    cleanup.push(drop_sql);
    Ok(())
}

async fn inspect_fixture(
    driver: &dyn DatabaseDriver,
    engine: &str,
    database: &str,
    schemas: &[String; 2],
    table: &str,
    parent: &str,
    view: &str,
    index: &str,
    primary: &str,
    foreign_key: &str,
) -> TestResult {
    let tables = driver
        .get_tables(Some(database))
        .await
        .map_err(metadata_error)?;
    let views = driver
        .get_views(Some(database))
        .await
        .map_err(metadata_error)?;
    for (position, schema) in schemas.iter().enumerate() {
        require(
            tables
                .iter()
                .filter(|item| item.name == table && item.schema.as_deref() == Some(schema))
                .count()
                == 1,
            "跨 schema 的同名表必须各自保留真实身份",
        )?;
        require(
            views
                .iter()
                .filter(|item| item.name == view && item.schema.as_deref() == Some(schema))
                .count()
                == 1,
            "跨 schema 的同名视图必须各自保留真实身份",
        )?;
        let target = driver
            .resolve_table(database, &[schema.clone(), table.into()])
            .await
            .map_err(metadata_error)?;
        require(
            target.database == database
                && target.schema.as_deref() == Some(schema)
                && target.table == table,
            "带点、空格、引号的各段名称必须完整保留",
        )?;
        let marker = if position == 0 {
            "left_marker"
        } else {
            "right_marker"
        };
        let other_marker = if position == 0 {
            "right_marker"
        } else {
            "left_marker"
        };
        require(
            target.columns.iter().any(|column| column.name == marker)
                && !target
                    .columns
                    .iter()
                    .any(|column| column.name == other_marker),
            "同名表的列不能跨 schema 混合",
        )?;
        let indexes = driver
            .get_indexes_for(&target)
            .await
            .map_err(metadata_error)?;
        let pk = indexes
            .iter()
            .find(|item| item.name == primary)
            .ok_or("未找到自定义名称主键索引")?;
        require(
            pk.is_primary && pk.is_unique && pk.columns == ["id"],
            "主键标记必须来自目录，不能猜测名称后缀",
        )?;
        let ordinary = indexes
            .iter()
            .find(|item| item.name == index)
            .ok_or("未找到以 _pkey 结尾的普通索引")?;
        require(
            !ordinary.is_primary && !ordinary.is_unique,
            "普通索引不能因 _pkey 后缀被当作主键",
        )?;
        require(
            ordinary.columns == ["payload"] && ordinary.included_columns == ["b"],
            "包含列不能混入索引键列",
        )?;
        require(
            ordinary.key_parts.len() == 1
                && ordinary.key_parts[0].column.as_deref() == Some("payload")
                && ordinary.key_parts[0].descending,
            "键序位、真实列名和降序标记必须保留",
        )?;
        require(
            ordinary
                .predicate
                .as_ref()
                .is_some_and(|predicate| predicate.contains("payload")),
            "过滤索引谓词不能丢失",
        )?;
        if engine == "postgres" {
            let expression = indexes
                .iter()
                .find(|item| item.name == "expression_key")
                .ok_or("未找到表达式索引")?;
            require(
                expression.columns.is_empty()
                    && expression.key_parts.len() == 1
                    && expression.key_parts[0].column.is_none()
                    && expression.key_parts[0]
                        .expression
                        .as_ref()
                        .is_some_and(|value| value.contains("lower"))
                    && expression.definition.is_some(),
                "表达式必须显式返回，不能伪造成真实列或静默丢弃",
            )?;
        }
        let keys = driver
            .get_foreign_keys_for(&target)
            .await
            .map_err(metadata_error)?;
        require(keys.len() == 1, "同名约束不能跨 schema 合并或重复")?;
        let key = &keys[0];
        require(
            key.name == foreign_key && key.columns == ["a", "b"] && key.ref_columns == ["b", "a"],
            "复合外键必须按约束序位一一配对，不能产生笛卡尔积",
        )?;
        require(
            key.ref_database.as_deref() == Some(database)
                && key.ref_schema.as_deref() == Some(&schemas[1 - position])
                && key.ref_table == parent,
            "引用表必须保留实际目标 schema",
        )?;
        require(
            key.on_delete.as_deref() == Some("CASCADE")
                && key.on_update.as_deref() == Some("NO ACTION"),
            "外键动作必须来自真实约束",
        )?;
        let view_target = driver
            .resolve_table(database, &[schema.clone(), view.into()])
            .await
            .map_err(metadata_error)?;
        let definition = driver
            .get_view_definition_for(&view_target)
            .await
            .map_err(metadata_error)?;
        require(
            definition.contains(marker) && !definition.contains(other_marker),
            "视图定义必须读取选定 schema 的对象",
        )?;
        let (dump, count) = driver
            .dump_table_for(&target, true, false)
            .await
            .map_err(metadata_error)?;
        require(
            count == 0
                && dump.contains(&qualified(engine, schema, table))
                && dump.contains(marker)
                && !dump.contains(other_marker),
            "DDL 导出必须沿用已解析身份和列",
        )?;
    }
    Ok(())
}

/// 随机网络夹具专用平面目录；只删除本次创建的导出文件。
struct ExportDirectory(std::path::PathBuf);
impl ExportDirectory {
    fn new() -> TestResult<Self> {
        let directory =
            std::env::temp_dir().join(format!("sqlink-pg-metadata-export-{}", Uuid::new_v4()));
        std::fs::create_dir(&directory).map_err(|_| "创建网络导出测试目录失败".to_string())?;
        Ok(Self(directory))
    }
    fn no_temporary_files(&self) -> TestResult {
        let entries = std::fs::read_dir(&self.0).map_err(|_| "读取测试导出目录失败".to_string())?;
        for entry in entries {
            let entry = entry.map_err(|_| "读取测试导出文件失败".to_string())?;
            require(
                !entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".sqlink-export-"),
                "canonical 网络导出不能遗留未提交的临时文件",
            )?;
        }
        Ok(())
    }
}
impl Drop for ExportDirectory {
    fn drop(&mut self) {
        if let Ok(entries) = std::fs::read_dir(&self.0) {
            for entry in entries.flatten() {
                let _ = std::fs::remove_file(entry.path());
            }
        }
        let _ = std::fs::remove_dir(&self.0);
    }
}

async fn export_postgres_source(
    manager: &ConnectionPoolManager,
    id: &str,
    database: &str,
    schema: &str,
    table: &str,
    path: &std::path::Path,
    sql: bool,
) -> TestResult {
    let snapshot =
        sqlconnect_lib::metadata::read(manager, id, database, &[schema.into(), table.into()])
            .await
            .map_err(metadata_error)?;
    let summary = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        sqlconnect_lib::export::export_job_sources(
            manager,
            id.into(),
            Some(snapshot.target.database.clone()),
            None,
            Some(vec![sqlconnect_lib::metadata::ObjectIdentity::from(
                &snapshot.target,
            )]),
            path.to_str().ok_or("测试路径不是有效UTF-8")?.into(),
            sql,
            true,
            true,
            None,
        ),
    )
    .await
    .map_err(|_| "PG canonical 导出超时".to_string())?
    .map_err(|_| "PG canonical 导出失败".to_string())?;
    require(
        summary.tables == 1 && summary.rows == 1,
        "PG canonical 导出必须精确读取所选同名表的一行",
    )
}

async fn postgres_export_round_trip(
    manager: &ConnectionPoolManager,
    driver: &dyn DatabaseDriver,
    id: &str,
    database: &str,
    schemas: &[String; 2],
    table: &str,
    parent: &str,
    view: &str,
) -> TestResult {
    let directory = ExportDirectory::new()?;
    // 两端父表先准备相同复合键，再写入带不同实际值的同名子表，验证跨 schema FK 不受影响。
    for schema in schemas {
        ddl(
            driver,
            database,
            &format!(
                "INSERT INTO {} (a, b) VALUES (11, 22)",
                qualified("postgres", schema, parent)
            ),
        )
        .await?;
    }
    let expected = [
        "\"id\",\"a\",\"b\",\"payload\",\"left_marker\"\n101,22,11,\"left source\",111\n",
        "\"id\",\"a\",\"b\",\"payload\",\"right_marker\"\n202,22,11,\"right source\",222\n",
    ];
    for (position, schema) in schemas.iter().enumerate() {
        let marker = if position == 0 {
            "left_marker"
        } else {
            "right_marker"
        };
        let (row_id, marker_value, payload) = if position == 0 {
            (101, 111, "left source")
        } else {
            (202, 222, "right source")
        };
        ddl(driver, database, &format!("INSERT INTO {} (id, a, b, payload, {marker}) VALUES ({row_id}, 22, 11, '{payload}', {marker_value})", qualified("postgres", schema, table))).await?;
        let path = directory.0.join(format!("source-{position}.csv"));
        std::fs::write(&path, b"previous source output")
            .map_err(|_| "写入导出旧文件失败".to_string())?;
        export_postgres_source(manager, id, database, schema, table, &path, false).await?;
        let csv = std::fs::read_to_string(&path).map_err(|_| "读取导出CSV失败".to_string())?;
        require(
            csv == expected[position],
            "PG canonical CSV 的列和值必须都来自所选非默认 schema",
        )?;
        directory.no_temporary_files()?;
    }
    let dump_path = directory.0.join("left-schema.sql");
    export_postgres_source(manager, id, database, &schemas[0], table, &dump_path, true).await?;
    let dump = std::fs::read_to_string(&dump_path).map_err(|_| "读取PG转储失败".to_string())?;
    let left = qualified("postgres", &schemas[0], table);
    let right = qualified("postgres", &schemas[1], table);
    require(
        dump.contains(&format!("CREATE TABLE {left}"))
            && dump.contains(&format!("INSERT INTO {left}")),
        "PG非默认schema转储的DDL和INSERT必须保持同一个canonical来源",
    )?;
    require(
        dump.contains("left_marker")
            && dump.contains("left source")
            && !dump.contains("right_marker")
            && !dump.contains("right source")
            && !dump.contains(&right),
        "左schema转储不能混入右schema同名表的定义或数据",
    )?;
    // 只清理本次左schema夹具的依赖视图和子表，再通过生产导入入口回放同一份SQL。
    // 转储仍沿用现有近似DDL语义；本测试不把缺省未重建的FK/索引当作完整备份承诺。
    let left_view = qualified("postgres", &schemas[0], view);
    ddl(driver, database, &format!("DROP VIEW {left_view}")).await?;
    ddl(driver, database, &format!("DROP TABLE {left}")).await?;
    let imported = sqlconnect_lib::commands::import::import_sql_file(
        manager,
        id.into(),
        Some(database.into()),
        dump_path.to_str().ok_or("测试路径不是有效UTF-8")?.into(),
        "error".into(),
    )
    .await
    .map_err(|_| "PG非默认schema转储回放失败".to_string())?;
    require(
        imported.executed >= 3 && imported.skipped == 0,
        "PG转储应实际执行DROP/CREATE/INSERT",
    )?;
    ddl(
        driver,
        database,
        &format!("CREATE VIEW {left_view} AS SELECT left_marker FROM {left}"),
    )
    .await?;
    for (position, schema) in schemas.iter().enumerate() {
        let path = directory.0.join(format!("replayed-{position}.csv"));
        export_postgres_source(manager, id, database, schema, table, &path, false).await?;
        require(
            std::fs::read_to_string(&path).map_err(|_| "读取回放CSV失败".to_string())?
                == expected[position],
            "PG转储回放必须恢复选中schema的原始行，并保持另一schema同名表不变",
        )?;
    }
    require(
        manager.tasks.active_count() == 0,
        "PG导出任务归还后不能遗留活动租约任务",
    )?;
    directory.no_temporary_files()
}

async fn postgres_skip_uses_exact_schema_and_current_import_search_path(
    manager: &ConnectionPoolManager,
    driver: &dyn DatabaseDriver,
    id: &str,
    database: &str,
    schemas: &[String; 2],
    suffix: &str,
    cleanup: &mut Vec<String>,
) -> TestResult {
    let directory = ExportDirectory::new()?;
    let table = format!("metadata_skip_{suffix}");
    let left = qualified("postgres", &schemas[0], &table);
    let right = qualified("postgres", &schemas[1], &table);
    create(
        driver,
        database,
        format!("CREATE TABLE {right} (right_original INT)"),
        format!("DROP TABLE {right}"),
        cleanup,
    )
    .await?;
    ddl(driver, database, &format!("INSERT INTO {right} VALUES (7)")).await?;
    let upper_table = format!("Case .\" {suffix}");
    let lower_table = format!("case .\" {suffix}");
    let upper = qualified("postgres", &schemas[0], &upper_table);
    let lower = qualified("postgres", &schemas[0], &lower_table);
    // 文件可能在任意语句处失败，预先登记本次随机schema里的独占目标，确保仍然清理。
    for object in [&left, &upper, &lower] {
        cleanup.push(format!("DROP TABLE IF EXISTS {object}"));
    }
    let script = [
        format!("CREATE TABLE {left} (left_new INT);"),
        format!("INSERT INTO {left} VALUES (8);"),
        format!("CREATE TABLE {left} (wrong_repeated INT);"),
        format!("SET search_path TO {};", quote("postgres", &schemas[0])),
        format!(
            "CREATE TABLE {} (wrong_left_search_path INT);",
            quote("postgres", &table)
        ),
        format!("SET search_path TO {};", quote("postgres", &schemas[1])),
        format!(
            "CREATE TABLE {} (wrong_right_search_path INT);",
            quote("postgres", &table)
        ),
        format!("CREATE TABLE {upper} (upper_marker INT);"),
        format!("CREATE TABLE {lower} (lower_marker INT);"),
        format!("CREATE TABLE {upper} (wrong_folded_case INT);"),
    ]
    .join("\n");
    let path = directory.0.join("skip-exact-schema.sql");
    std::fs::write(&path, script).map_err(|_| "写入PG skip回归SQL失败".to_string())?;
    let summary = sqlconnect_lib::commands::import::import_sql_file(
        manager,
        id.into(),
        Some(database.into()),
        path.to_str().ok_or("测试路径不是有效UTF-8")?.into(),
        "skip".into(),
    )
    .await
    .map_err(|_| "PG skip完整目标与会话目录回归失败".to_string())?;
    require(
        summary.statements == 10 && summary.executed == 6 && summary.skipped == 4,
        "PG skip应按每次CREATE当时的真实目录判断，不混schema、不过期、不折叠引用大小写",
    )?;
    for (schema, table, marker) in [
        (&schemas[0], &table, "left_new"),
        (&schemas[1], &table, "right_original"),
        (&schemas[0], &upper_table, "upper_marker"),
        (&schemas[0], &lower_table, "lower_marker"),
    ] {
        let target = driver
            .resolve_table(database, &[schema.clone(), table.clone()])
            .await
            .map_err(metadata_error)?;
        require(
            target.columns.len() == 1 && target.columns[0].name == marker,
            "PG skip不能跳过另一schema的新表或改动已存在表",
        )?;
    }
    for (object, column, expected) in [(&left, "left_new", 8), (&right, "right_original", 7)] {
        let result = driver
            .query(
                &format!("SELECT {column} FROM {object}"),
                Some(database),
                None,
            )
            .await
            .map_err(metadata_error)?;
        require(
            result.rows.len() == 1
                && result.rows[0].values[0]
                    .as_ref()
                    .and_then(serde_json::Value::as_i64)
                    == Some(expected),
            "PG skip必须创建缺少的目标并保留另一schema原始行",
        )?;
    }
    directory.no_temporary_files()
}

async fn exercise(engine: &str, variable: &str) -> TestResult {
    let suffix = Uuid::new_v4().simple().to_string();
    let short = &suffix[..12];
    let id = format!("metadata_{suffix}");
    let config = config_from_env(engine, variable, &id)?;
    let database = config.database.clone().ok_or("测试库未配置")?;
    let manager = ConnectionPoolManager::new(None);
    manager
        .create_connection(config)
        .await
        .map_err(|_| "连接指定测试数据库失败".to_string())?;
    let driver = manager
        .get_connection(&id)
        .await
        .map_err(|_| "测试连接不可用".to_string())?;
    let schemas = [format!("meta {short}.\"]a"), format!("meta {short}.\"]b")];
    let table = "child .\"] table";
    let parent = "parent .\"] table";
    let view = "view .\"] same";
    let primary = "custom_primary_without_suffix";
    let index = "ordinary_pkey";
    let foreign_key = "same_constraint";
    let mut cleanup = Vec::new();
    let outcome = async {
        for schema in &schemas {
            create(driver.as_ref(), &database, format!("CREATE SCHEMA {}", quote(engine, schema)),
                   format!("DROP SCHEMA {}", quote(engine, schema)), &mut cleanup).await?;
        }
        for schema in &schemas {
            let object = qualified(engine, schema, parent);
            create(driver.as_ref(), &database,
                   format!("CREATE TABLE {object} (a INT NOT NULL, b INT NOT NULL, CONSTRAINT {} PRIMARY KEY (b, a))", quote(engine, "parent_primary")),
                   format!("DROP TABLE {object}"), &mut cleanup).await?;
        }
        for (position, schema) in schemas.iter().enumerate() {
            let object = qualified(engine, schema, table);
            let referenced = qualified(engine, &schemas[1 - position], parent);
            let marker = if position == 0 { "left_marker" } else { "right_marker" };
            create(driver.as_ref(), &database,
                   format!("CREATE TABLE {object} (id INT NOT NULL, a INT NOT NULL, b INT NOT NULL, payload VARCHAR(80) NULL, {marker} INT NULL, CONSTRAINT {} PRIMARY KEY (id), CONSTRAINT {} FOREIGN KEY (a, b) REFERENCES {referenced} (b, a) ON DELETE CASCADE)", quote(engine, primary), quote(engine, foreign_key)),
                   format!("DROP TABLE {object}"), &mut cleanup).await?;
            ddl(driver.as_ref(), &database, &format!("CREATE INDEX {} ON {object} (payload DESC) INCLUDE (b) WHERE payload IS NOT NULL", quote(engine, index))).await?;
            if engine == "postgres" {
                ddl(driver.as_ref(), &database, &format!("CREATE INDEX {} ON {object} (lower(payload))", quote(engine, "expression_key"))).await?;
            }
            let view_object = qualified(engine, schema, view);
            create(driver.as_ref(), &database, format!("CREATE VIEW {view_object} AS SELECT {marker} FROM {object}"),
                   format!("DROP VIEW {view_object}"), &mut cleanup).await?;
        }
        inspect_fixture(driver.as_ref(), engine, &database, &schemas, table, parent, view, index, primary, foreign_key).await?;
        if engine == "postgres" {
            postgres_export_round_trip(&manager, driver.as_ref(), &id, &database, &schemas, table, parent, view).await?;
            postgres_skip_uses_exact_schema_and_current_import_search_path(&manager, driver.as_ref(), &id, &database, &schemas, short, &mut cleanup).await?;
        }
        // 旧 API 的 table 参数始终是一整段名称，名称里的点不能当限定符。
        let literal = format!("legacy {short}.\"] name");
        let default_schema = if engine == "postgres" { "public" } else { "dbo" };
        let literal_object = qualified(engine, default_schema, &literal);
        create(driver.as_ref(), &database,
               format!("CREATE TABLE {literal_object} (legacy_marker INT PRIMARY KEY)"),
               format!("DROP TABLE {literal_object}"), &mut cleanup).await?;
        let legacy_columns = driver.get_columns(None, &literal).await.map_err(metadata_error)?;
        require(legacy_columns.len() == 1 && legacy_columns[0].name == "legacy_marker", "旧接口不能拆分单段带点名称")?;
        let legacy_indexes = driver.get_indexes(None, &literal).await.map_err(metadata_error)?;
        require(legacy_indexes.len() == 1 && legacy_indexes[0].is_primary, "旧索引接口必须解析当前真实数据库")?;
        require(driver.get_foreign_keys(None, &literal).await.map_err(metadata_error)?.is_empty(), "旧外键接口应精确读取目标表")?;
        let changed = qualified(engine, &schemas[0], table);
        ddl(driver.as_ref(), &database, &format!("ALTER TABLE {changed} ADD after_ddl INT NULL")).await?;
        ddl(driver.as_ref(), &database, &format!("CREATE INDEX {} ON {changed} (after_ddl)", quote(engine, "after_ddl_index"))).await?;
        let fresh = driver.resolve_table(&database, &[schemas[0].clone(), table.into()]).await.map_err(metadata_error)?;
        require(fresh.columns.iter().any(|column| column.name == "after_ddl"), "DDL 后必须重新获取真实列目录")?;
        require(driver.get_indexes_for(&fresh).await.map_err(metadata_error)?.iter().any(|item| item.name == "after_ddl_index" && item.columns == ["after_ddl"]),
                "DDL 后必须重新获取真实索引目录")?;
        let untouched = driver.resolve_table(&database, &[schemas[1].clone(), table.into()]).await.map_err(metadata_error)?;
        require(!untouched.columns.iter().any(|column| column.name == "after_ddl"), "DDL 不能污染其他 schema 的同名表元数据")?;
        Ok(())
    }.await;
    let mut failures = Vec::new();
    if let Err(error) = outcome {
        failures.push(error);
    }
    for sql in cleanup.into_iter().rev() {
        if let Err(error) = ddl(driver.as_ref(), &database, &sql).await {
            failures.push(format!("清理夹具失败：{error}"));
        }
    }
    if manager.disconnect(&id).await.is_err() {
        failures.push("断开测试连接失败".into());
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("；"))
    }
}

#[tokio::test]
#[ignore = "需要 SQLINK_TEST_POSTGRES 指定允许创建 schema 的专用测试库"]
async fn postgres_metadata_preserves_complete_identity() {
    exercise("postgres", "SQLINK_TEST_POSTGRES")
        .await
        .expect("PostgreSQL 完整元数据身份测试失败");
}

#[tokio::test]
#[ignore = "需要 SQLINK_TEST_MSSQL 指定允许创建 schema 的专用测试库"]
async fn mssql_metadata_preserves_complete_identity() {
    exercise("mssql", "SQLINK_TEST_MSSQL")
        .await
        .expect("SQL Server 完整元数据身份测试失败");
}
