//! 真实 SQLite 元数据身份：temp、main、ATTACH 同名对象不能串列、索引和外键。
use sqlconnect_lib::{
    database::{sqlite::SqliteDriver, DatabaseDriver},
    models::{ConnectionConfig, ResolvedTable},
};

async fn driver(sql: &str) -> SqliteDriver {
    let config: ConnectionConfig = serde_json::from_value(serde_json::json!({
        "name":"SQLite元数据回归", "driverType":"sqlite", "host":"", "port":0,
        "user":"", "password":"", "database":":memory:"
    }))
    .unwrap();
    let driver = SqliteDriver::connect(&config).await.unwrap();
    // execute 只接受单条语句，夹具也逐条通过真实 worker 执行。
    for statement in sql
        .split(';')
        .filter(|statement| !statement.trim().is_empty())
    {
        driver.execute(statement, None, None).await.unwrap();
    }
    driver
}

async fn resolve(driver: &SqliteDriver, parts: &[&str]) -> ResolvedTable {
    driver
        .resolve_table(
            ":memory:",
            &parts
                .iter()
                .map(|part| part.to_string())
                .collect::<Vec<_>>(),
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn same_names_in_temp_main_and_attached_keep_complete_identity() {
    let driver = driver("ATTACH ':memory:' AS aux;
        CREATE TABLE main.p(first INTEGER, second INTEGER, PRIMARY KEY(second, first));
        CREATE TABLE main.shared(m1 INTEGER, m2 INTEGER, generated INTEGER AS (m1+m2), FOREIGN KEY(m2,m1) REFERENCES p ON UPDATE CASCADE ON DELETE SET NULL);
        CREATE INDEX main.same_idx ON shared(m2, m1);
        CREATE TEMP TABLE p(first INTEGER, second INTEGER, PRIMARY KEY(first, second));
        CREATE TEMP TABLE shared(t1 INTEGER, t2 INTEGER, FOREIGN KEY(t1,t2) REFERENCES p(first,second));
        CREATE INDEX temp.same_idx ON shared(t1);
        CREATE TABLE aux.p(first INTEGER, second INTEGER, PRIMARY KEY(first, second));
        CREATE TABLE aux.shared(a1 INTEGER, a2 INTEGER, FOREIGN KEY(a2,a1) REFERENCES p(first,second) ON DELETE CASCADE);
        CREATE UNIQUE INDEX aux.same_idx ON shared(a2, a1);
        CREATE VIEW main.same_view AS SELECT m1 FROM shared;
        CREATE TEMP VIEW same_view AS SELECT t1 FROM shared;
        CREATE VIEW aux.same_view AS SELECT a1 FROM shared;").await;
    for (schema, expected_columns, expected_index, referenced, unique) in [
        (
            "temp",
            vec!["t1", "t2"],
            vec!["t1"],
            vec!["first", "second"],
            false,
        ),
        (
            "main",
            vec!["m1", "m2", "generated"],
            vec!["m2", "m1"],
            vec!["second", "first"],
            false,
        ),
        (
            "aux",
            vec!["a1", "a2"],
            vec!["a2", "a1"],
            vec!["first", "second"],
            true,
        ),
    ] {
        let target = resolve(&driver, &[schema, "shared"]).await;
        assert_eq!(target.schema.as_deref(), Some(schema));
        assert_eq!(
            target
                .columns
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>(),
            expected_columns
        );
        let indexes = driver.get_indexes_for(&target).await.unwrap();
        let index = indexes
            .iter()
            .find(|index| index.name == "same_idx")
            .unwrap();
        assert_eq!(index.columns, expected_index);
        assert_eq!(index.is_unique, unique);
        assert_eq!(
            index
                .key_parts
                .iter()
                .map(|part| part.column.as_deref().unwrap())
                .collect::<Vec<_>>(),
            expected_index
        );
        let fks = driver.get_foreign_keys_for(&target).await.unwrap();
        assert_eq!(fks.len(), 1);
        assert_eq!(fks[0].ref_schema.as_deref(), Some(schema));
        assert_eq!(fks[0].ref_table, "p");
        assert_eq!(fks[0].ref_columns, referenced);
        assert_eq!(fks[0].columns.len(), 2);
        let view = resolve(&driver, &[schema, "same_view"]).await;
        let definition = driver.get_view_definition_for(&view).await.unwrap();
        assert!(definition.contains(expected_columns[0]));
        let dump = driver.dump_table_for(&target, false, false).await;
        if schema == "main" {
            assert!(dump.unwrap().0.contains("m1"));
        } else {
            assert!(dump
                .unwrap_err()
                .to_string()
                .contains("EXPORT_SCHEMA_UNSUPPORTED"));
        }
    }
    let columns = driver.get_columns(None, "shared").await.unwrap();
    assert_eq!(columns[0].name, "t1");
    assert_eq!(
        driver.get_indexes(None, "shared").await.unwrap()[0].columns,
        vec!["t1"]
    );
    assert_eq!(
        driver.get_foreign_keys(None, "shared").await.unwrap()[0]
            .ref_schema
            .as_deref(),
        Some("temp")
    );
    for catalog in [
        driver.get_tables(None).await.unwrap(),
        driver.get_views(None).await.unwrap(),
    ] {
        let mut schemas = catalog
            .iter()
            .filter(|item| item.name == "shared" || item.name == "same_view")
            .map(|item| item.schema.as_deref().unwrap())
            .collect::<Vec<_>>();
        schemas.sort_unstable();
        assert_eq!(schemas, vec!["aux", "main", "temp"]);
    }
    let main = resolve(&driver, &["main", "shared"]).await;
    assert_eq!(main.columns[2].extra.as_deref(), Some("generated virtual"));
    let fk = &driver.get_foreign_keys_for(&main).await.unwrap()[0];
    assert_eq!(fk.columns, vec!["m2", "m1"]);
    assert_eq!(fk.on_update.as_deref(), Some("CASCADE"));
    assert_eq!(fk.on_delete.as_deref(), Some("SET NULL"));
    driver.disconnect().await.unwrap();
}

#[tokio::test]
async fn escaped_names_expression_positions_and_ddl_are_read_from_real_catalogs() {
    let driver = driver(r#"ATTACH ':memory:' AS "库.一""二";
        CREATE TABLE "库.一""二"."表.一""二"("列.一""二" INTEGER, ordinary TEXT);
        CREATE INDEX "库.一""二"."索引.一""二" ON "表.一""二"("列.一""二" DESC, lower(ordinary)) WHERE ordinary IS NOT NULL;
        CREATE VIEW "库.一""二"."视图.一""二" AS SELECT ordinary FROM "表.一""二";"#).await;
    let target = resolve(&driver, &["库.一\"二", "表.一\"二"]).await;
    let indexes = driver.get_indexes_for(&target).await.unwrap();
    assert_eq!(indexes.len(), 1);
    let index = &indexes[0];
    assert_eq!(index.name, "索引.一\"二");
    assert_eq!(index.columns, vec!["列.一\"二"]);
    assert_eq!(index.key_parts.len(), 2);
    assert!(index.key_parts[0].descending);
    assert_eq!(index.key_parts[1].column, None);
    assert!(index
        .definition
        .as_deref()
        .unwrap()
        .contains("lower(ordinary)"));
    assert!(index
        .definition
        .as_deref()
        .unwrap()
        .contains("WHERE ordinary IS NOT NULL"));
    let view = resolve(&driver, &["库.一\"二", "视图.一\"二"]).await;
    assert!(driver
        .get_view_definition_for(&view)
        .await
        .unwrap()
        .contains("ordinary"));
    driver
        .execute(r#"DROP INDEX "库.一""二"."索引.一""二""#, None, None)
        .await
        .unwrap();
    assert!(driver.get_indexes_for(&target).await.unwrap().is_empty());
    driver
        .execute(
            r#"ALTER TABLE "库.一""二"."表.一""二" ADD COLUMN added INTEGER"#,
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        resolve(&driver, &["库.一\"二", "表.一\"二"])
            .await
            .columns
            .len(),
        3
    );
    driver.disconnect().await.unwrap();
}

#[tokio::test]
async fn implicit_foreign_key_with_missing_parent_primary_key_fails_explicitly() {
    let driver =
        driver("CREATE TABLE p(value INTEGER); CREATE TABLE c(value INTEGER REFERENCES p);").await;
    let target = resolve(&driver, &["main", "c"]).await;
    let error = driver
        .get_foreign_keys_for(&target)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("METADATA_FOREIGN_KEY_UNSUPPORTED"));
    driver.disconnect().await.unwrap();
}

#[tokio::test]
async fn without_rowid_primary_index_has_keys_even_without_own_schema_record() {
    let driver = driver("CREATE TABLE p(first INTEGER, second INTEGER, PRIMARY KEY(second,first)) WITHOUT ROWID; CREATE TABLE c(a INTEGER, b INTEGER, FOREIGN KEY(a,b) REFERENCES p);").await;
    let parent = resolve(&driver, &["main", "p"]).await;
    let indexes = driver.get_indexes_for(&parent).await.unwrap();
    let primary = indexes.iter().find(|index| index.is_primary).unwrap();
    assert!(primary.is_unique);
    assert_eq!(primary.columns, vec!["second", "first"]);
    assert_eq!(primary.definition, None);
    let child = resolve(&driver, &["main", "c"]).await;
    let fks = driver.get_foreign_keys_for(&child).await.unwrap();
    assert_eq!(fks[0].ref_columns, vec!["second", "first"]);
    driver.disconnect().await.unwrap();
}
