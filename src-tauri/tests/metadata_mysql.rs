//! 仅显式运行：需要 SQLINK_TEST_MYSQL 和 SQLINK_TEST_MYSQL_OTHER_DATABASE 两个独立测试库。
//! 两库创建同名随机表，测试账号须有这两库的建表、建索引及清理权限。
use sqlconnect_lib::{
    database::{mysql::MySqlDriver, DatabaseDriver},
    models::{ConnectionConfig, ResolvedTable},
};

fn quote(name: &str) -> String {
    format!("`{}`", name.replace('`', "``"))
}
fn qualified(database: &str, table: &str) -> String {
    format!("{}.{}", quote(database), quote(table))
}

async fn fixture() -> (MySqlDriver, String, String, String) {
    let raw = std::env::var("SQLINK_TEST_MYSQL")
        .expect("显式运行必须设置 SQLINK_TEST_MYSQL，禁止缺少环境时假成功");
    let opts = mysql_async::Opts::from_url(&raw).expect("SQLINK_TEST_MYSQL 连接串无效");
    let database = opts
        .db_name()
        .filter(|name| !name.is_empty())
        .expect("必须明确指定独立测试库")
        .to_string();
    let other = std::env::var("SQLINK_TEST_MYSQL_OTHER_DATABASE")
        .expect("跨库回归必须设置 SQLINK_TEST_MYSQL_OTHER_DATABASE 独立测试库");
    assert!(!other.is_empty() && other != database, "两测试库必须不同");
    let config: ConnectionConfig = serde_json::from_value(serde_json::json!({
        "name":"MySQL元数据身份回归", "driverType":"mysql", "host":opts.ip_or_hostname(),
        "port":opts.tcp_port(), "user":opts.user().unwrap_or(""),
        "password":opts.pass().unwrap_or(""), "database":database
    }))
    .unwrap();
    let driver = MySqlDriver::connect(&config)
        .await
        .expect("独立测试库必须可连接");
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    (driver, database, other, suffix)
}

async fn resolve(driver: &MySqlDriver, database: &str, table: &str) -> ResolvedTable {
    driver
        .resolve_table(database, &[table.to_string()])
        .await
        .unwrap()
}

#[tokio::test]
#[ignore = "需要显式配置两个独立 MySQL 测试库"]
async fn cross_database_same_names_preserve_composite_foreign_keys_and_real_unique_flags() {
    let (driver, database, other, suffix) = fixture().await;
    let parent = format!("md_parent_{suffix}");
    let child = format!("md_child_{suffix}");
    let fk_name = format!("md_fk_{suffix}");
    let idx_name = format!("md_idx_{suffix}");
    for schema in [&database, &other] {
        driver.execute(&format!("CREATE TABLE {} (p1 BIGINT NOT NULL, p2 BIGINT NOT NULL, PRIMARY KEY(p2,p1)) ENGINE=InnoDB", qualified(schema, &parent)), None, None).await.unwrap();
    }
    // 同名子表使用不同键列，且第一库的引用目标在另一个库。
    driver.execute(&format!("CREATE TABLE {} (a BIGINT, b BIGINT, CONSTRAINT {} FOREIGN KEY(b,a) REFERENCES {}(p2,p1) ON UPDATE CASCADE ON DELETE SET NULL, UNIQUE KEY {}(a,b)) ENGINE=InnoDB", qualified(&database, &child), quote(&fk_name), qualified(&other, &parent), quote(&idx_name)), None, None).await.unwrap();
    driver.execute(&format!("CREATE TABLE {} (x BIGINT, y BIGINT, CONSTRAINT {} FOREIGN KEY(x,y) REFERENCES {}(p2,p1) ON UPDATE RESTRICT ON DELETE CASCADE, KEY {}(y,x)) ENGINE=InnoDB", qualified(&other, &child), quote(&fk_name), qualified(&other, &parent), quote(&idx_name)), None, None).await.unwrap();
    for (schema, expected_cols, expected_ref, unique) in [
        (&database, vec!["a", "b"], vec!["b", "a"], true),
        (&other, vec!["y", "x"], vec!["x", "y"], false),
    ] {
        let target = resolve(&driver, schema, &child).await;
        assert_eq!(target.database, *schema);
        let indexes = driver.get_indexes_for(&target).await.unwrap();
        let index = indexes.iter().find(|index| index.name == idx_name).unwrap();
        assert_eq!(index.columns, expected_cols);
        assert_eq!(index.is_unique, unique);
        assert!(!index.is_primary);
        let fks = driver.get_foreign_keys_for(&target).await.unwrap();
        assert_eq!(fks.len(), 1);
        assert_eq!(fks[0].name, fk_name);
        assert_eq!(fks[0].columns, expected_ref);
        assert_eq!(fks[0].ref_schema.as_deref(), Some(other.as_str()));
        assert_eq!(fks[0].ref_table, parent);
        assert_eq!(fks[0].ref_columns, vec!["p2", "p1"]);
        assert_eq!(
            fks[0].on_delete.as_deref(),
            Some(if unique { "SET NULL" } else { "CASCADE" })
        );
    }
    let before = resolve(&driver, &database, &child).await;
    driver
        .execute(
            &format!(
                "ALTER TABLE {} DROP INDEX {}",
                qualified(&database, &child),
                quote(&idx_name)
            ),
            None,
            None,
        )
        .await
        .unwrap();
    assert!(!driver
        .get_indexes_for(&before)
        .await
        .unwrap()
        .iter()
        .any(|index| index.name == idx_name));
    driver
        .execute(
            &format!(
                "ALTER TABLE {} ADD COLUMN added BIGINT",
                qualified(&database, &child)
            ),
            None,
            None,
        )
        .await
        .unwrap();
    assert!(driver
        .get_columns(Some(&database), &child)
        .await
        .unwrap()
        .iter()
        .any(|column| column.name == "added"));
    for schema in [&database, &other] {
        driver
            .execute(
                &format!("DROP TABLE {}", qualified(schema, &child)),
                None,
                None,
            )
            .await
            .unwrap();
    }
    for schema in [&database, &other] {
        driver
            .execute(
                &format!("DROP TABLE {}", qualified(schema, &parent)),
                None,
                None,
            )
            .await
            .unwrap();
    }
    driver.disconnect().await.unwrap();
}

#[tokio::test]
#[ignore = "需要显式配置两个独立 MySQL 测试库；沿用服务端默认 sql_mode"]
async fn quoted_names_and_prefix_indexes_are_complete_under_default_sql_mode() {
    let (driver, database, _, suffix) = fixture().await;
    // 表名中单引号和反引号不允许被拼成 SQL 字面量；列名可保留点号和反斜线。
    let table = format!("元数据_'`\"_{suffix}");
    let column = "列.名'`\\\"";
    let index = format!("idx_'`_{suffix}");
    let view = format!("view_'`_{suffix}");
    driver.execute(&format!("CREATE TABLE {} (id BIGINT PRIMARY KEY, {} VARCHAR(100), KEY {}({}(7))) ENGINE=InnoDB", qualified(&database, &table), quote(column), quote(&index), quote(column)), None, None).await.unwrap();
    driver
        .execute(
            &format!(
                "CREATE VIEW {} AS SELECT {} FROM {}",
                qualified(&database, &view),
                quote(column),
                qualified(&database, &table)
            ),
            None,
            None,
        )
        .await
        .unwrap();
    let target = resolve(&driver, &database, &table).await;
    assert_eq!(target.table, table);
    assert_eq!(target.columns[1].name, column);
    let indexes = driver.get_indexes_for(&target).await.unwrap();
    let prefix = indexes.iter().find(|item| item.name == index).unwrap();
    assert_eq!(prefix.columns, vec![column]);
    assert_eq!(prefix.key_parts.len(), 1);
    assert!(prefix.definition.as_deref().unwrap().contains("(7)"));
    let primary = indexes.iter().find(|item| item.is_primary).unwrap();
    assert!(primary.is_unique);
    assert_eq!(primary.columns, vec!["id"]);
    let view_target = resolve(&driver, &database, &view).await;
    assert!(driver
        .get_view_definition_for(&view_target)
        .await
        .unwrap()
        .contains(&quote(column)));
    // 参数绑定不会把恶意值改变为过滤条件。
    assert!(driver
        .resolve_table(&database, &[format!("{}' OR 1=1 --", table)])
        .await
        .is_err());
    driver
        .execute(
            &format!("DROP VIEW {}", qualified(&database, &view)),
            None,
            None,
        )
        .await
        .unwrap();
    driver
        .execute(
            &format!("DROP TABLE {}", qualified(&database, &table)),
            None,
            None,
        )
        .await
        .unwrap();
    driver.disconnect().await.unwrap();
}

#[tokio::test]
#[ignore = "需要显式配置 MySQL 8.0.13+ 独立测试库以验证功能索引"]
async fn functional_index_retains_null_column_position_expression_and_descending() {
    let (driver, database, _, suffix) = fixture().await;
    let table = format!("md_expression_{suffix}");
    let index = format!("idx_expression_{suffix}");
    driver.execute(&format!("CREATE TABLE {} (value VARCHAR(100), id BIGINT, KEY {} ((lower(value)), id DESC)) ENGINE=InnoDB", qualified(&database, &table), quote(&index)), None, None).await.expect("该项回归要求 MySQL 8.0.13+ 功能索引支持");
    let target = resolve(&driver, &database, &table).await;
    let indexes = driver.get_indexes_for(&target).await.unwrap();
    let index = indexes.iter().find(|item| item.name == index).unwrap();
    assert_eq!(index.columns, vec!["id"]);
    assert_eq!(index.key_parts.len(), 2);
    assert_eq!(index.key_parts[0].column, None);
    assert!(index.key_parts[0]
        .expression
        .as_deref()
        .unwrap()
        .to_lowercase()
        .contains("lower"));
    assert_eq!(index.key_parts[1].column.as_deref(), Some("id"));
    assert!(index.key_parts[1].descending);
    assert!(index
        .definition
        .as_deref()
        .unwrap()
        .contains("CREATE TABLE"));
    driver
        .execute(
            &format!("DROP TABLE {}", qualified(&database, &table)),
            None,
            None,
        )
        .await
        .unwrap();
    driver.disconnect().await.unwrap();
}
