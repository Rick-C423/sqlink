//! 使用真实 SQLite 内存库验证导入预算、零写拒绝、部分提交与会话清理。
use serde_json::json;
use sqlconnect_lib::{
    commands::import::import_sql_file,
    database::{
        dump::{normalize_table_name, parse_create_table, SqlSplitter, MAX_IMPORT_STATEMENT_BYTES},
        pool::ConnectionPoolManager,
    },
    export::export_job,
    models::ConnectionConfig,
    typed_value::TypedValue,
};
use std::io::{BufRead, Cursor, Read, Write};
use std::path::PathBuf;

struct SqlFile(PathBuf);
impl SqlFile {
    fn new(bytes: impl AsRef<[u8]>) -> Self {
        let path =
            std::env::temp_dir().join(format!("sqlink-import-budget-{}.sql", uuid::Uuid::new_v4()));
        std::fs::write(&path, bytes).unwrap();
        Self(path)
    }
    fn oversized() -> Self {
        let file = Self::new([]);
        let mut writer = std::fs::File::create(&file.0).unwrap();
        writer.write_all(b"INSERT INTO imported VALUES('").unwrap();
        let block = [b'x'; 16 * 1024];
        for _ in 0..MAX_IMPORT_STATEMENT_BYTES / block.len() {
            writer.write_all(&block).unwrap();
        }
        writer.write_all(b"');\n").unwrap();
        file
    }
    fn path(&self) -> String {
        self.0.to_string_lossy().into_owned()
    }
}
impl Drop for SqlFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

async fn manager() -> ConnectionPoolManager {
    let manager = ConnectionPoolManager::new(None);
    let config: ConnectionConfig = serde_json::from_value(json!({
        "id":"import","name":"导入预算回归","driverType":"sqlite", "host":"", "port":0,
        "user":"", "password":"", "database":":memory:",
    }))
    .unwrap();
    manager.create_connection(config).await.unwrap();
    manager
        .get_connection("import")
        .await
        .unwrap()
        .execute("CREATE TABLE imported(value)", None, None)
        .await
        .unwrap();
    manager
}

async fn import(
    manager: &ConnectionPoolManager,
    file: &SqlFile,
    conflict: &str,
) -> Result<sqlconnect_lib::models::ImportSummary, String> {
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        import_sql_file(manager, "import".into(), None, file.path(), conflict.into()),
    )
    .await
    .expect("导入或归还不能遗留会话阻塞")
}

async fn values(manager: &ConnectionPoolManager) -> Vec<serde_json::Value> {
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        manager
            .get_connection("import")
            .await
            .unwrap()
            .query("SELECT value FROM imported ORDER BY value", None, None)
            .await
            .unwrap()
            .rows
            .into_iter()
            .map(|r| r.values[0].clone().unwrap())
            .collect()
    })
    .await
    .expect("下一次访问必须复用已清理的会话")
}

fn assert_counts(error: &str, statement: u64, executed: u64, skipped: u64) {
    assert!(error.contains(&format!("第 {statement} 条")), "{error}");
    assert!(
        error.contains(&format!("已执行成功 {executed} 条")),
        "{error}"
    );
    assert!(error.contains(&format!("已跳过 {skipped} 条")), "{error}");
    assert!(error.contains("此前成功语句可能已提交"), "{error}");
    assert!(error.contains("不要盲目重试"), "{error}");
}

#[tokio::test]
async fn invalid_conflict_and_unreadable_file_are_rejected_before_database_access() {
    let manager = manager().await;
    let file = SqlFile::new("INSERT INTO imported VALUES(1);");
    for conflict in ["", "Error", "ignore", "replace"] {
        let error = import(&manager, &file, conflict).await.unwrap_err();
        assert!(error.contains("IMPORT_CONFLICT_INVALID"));
        assert_counts(&error, 1, 0, 0);
        assert!(values(&manager).await.is_empty());
    }
    // 即使连接不存在，也应先报告不可读文件，不能先访问目录或尝试重连。
    let empty_manager = ConnectionPoolManager::new(None);
    let missing = std::env::temp_dir().join(format!("sqlink-missing-{}.sql", uuid::Uuid::new_v4()));
    let error = import_sql_file(
        &empty_manager,
        "absent".into(),
        None,
        missing.to_string_lossy().into_owned(),
        "skip".into(),
    )
    .await
    .unwrap_err();
    assert!(error.contains("打开文件失败"), "{error}");
    assert_counts(&error, 1, 0, 0);
}

#[tokio::test]
async fn malformed_and_oversized_statements_send_no_partial_sql_and_allow_reuse() {
    let manager = manager().await;
    for sql in [
        "INSERT INTO imported VALUES('未闭合",
        "INSERT INTO imported VALUES(1) /* 未闭合",
        "INSERT INTO imported VALUES(1,);",
    ] {
        let file = SqlFile::new(sql);
        let error = import(&manager, &file, "error").await.unwrap_err();
        assert_counts(&error, 1, 0, 0);
        assert!(values(&manager).await.is_empty());
    }
    let file = SqlFile::oversized();
    let error = import(&manager, &file, "error").await.unwrap_err();
    assert!(error.contains("IMPORT_STATEMENT_LIMIT"), "{error}");
    assert_counts(&error, 1, 0, 0);
    assert!(values(&manager).await.is_empty());
    let summary = import(
        &manager,
        &SqlFile::new("INSERT INTO imported VALUES(42);"),
        "error",
    )
    .await
    .unwrap();
    assert_eq!(summary.executed, 1);
    assert_eq!(values(&manager).await, vec![json!(42)]);
}

#[tokio::test]
async fn failure_preserves_prior_autocommits_rolls_back_open_transaction_and_reports_counts() {
    let manager = manager().await;
    let file = SqlFile::new("INSERT INTO imported VALUES(1);\nBEGIN;\nINSERT INTO imported VALUES(2);\nINSERT INTO missing_import_table VALUES(3);");
    let error = import(&manager, &file, "error").await.unwrap_err();
    assert_counts(&error, 4, 3, 0);
    assert_eq!(values(&manager).await, vec![json!(1)]);
    import(
        &manager,
        &SqlFile::new("INSERT INTO imported VALUES(3);"),
        "error",
    )
    .await
    .unwrap();
    assert_eq!(values(&manager).await, vec![json!(1), json!(3)]);

    let file = SqlFile::new("CREATE TABLE imported(value);\nINSERT INTO imported VALUES(4);\nINSERT INTO missing_import_table VALUES(5);");
    let error = import(&manager, &file, "skip").await.unwrap_err();
    assert_counts(&error, 3, 1, 1);
    assert_eq!(values(&manager).await, vec![json!(1), json!(3), json!(4)]);
}

#[tokio::test]
async fn success_counts_execution_and_explicitly_warns_that_unclosed_transactions_are_rolled_back()
{
    let manager = manager().await;
    let file =
        SqlFile::new("INSERT INTO imported VALUES(1);\nBEGIN;\nINSERT INTO imported VALUES(2);");
    let summary = import(&manager, &file, "error").await.unwrap();
    assert_eq!(summary.statements, 3);
    assert_eq!(summary.executed, 3);
    assert!(summary.cleanup_notice.contains("不代表全部已提交"));
    assert!(summary.cleanup_notice.contains("回滚未闭合事务"));
    assert_eq!(values(&manager).await, vec![json!(1)]);
    let file = SqlFile::new("BEGIN;\nINSERT INTO imported VALUES(3);\nCOMMIT;");
    assert_eq!(import(&manager, &file, "error").await.unwrap().executed, 3);
    assert_eq!(values(&manager).await, vec![json!(1), json!(3)]);
}

#[test]
fn splitter_rejects_line_and_multiline_statement_limits_at_small_budgets() {
    let mut exact = SqlSplitter::with_max_statement_bytes(false, 12);
    assert_eq!(
        exact
            .next_statement(&mut Cursor::new(b"SELECT\n12345"))
            .unwrap(),
        Some("SELECT\n12345".into())
    );
    for sql in ["SELECT\n123456", "SELECT 1;SELECT 2;", "SELECT '中文'"] {
        let mut splitter = SqlSplitter::with_max_statement_bytes(false, 12);
        assert!(splitter
            .next_statement(&mut Cursor::new(sql.as_bytes()))
            .unwrap_err()
            .to_string()
            .contains("IMPORT_STATEMENT_LIMIT"));
    }
}

struct NoReadLine(Cursor<Vec<u8>>);
impl Read for NoReadLine {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(out)
    }
}
impl BufRead for NoReadLine {
    fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
        self.0.fill_buf()
    }
    fn consume(&mut self, amount: usize) {
        self.0.consume(amount);
    }
    fn read_line(&mut self, _: &mut String) -> std::io::Result<usize> {
        panic!("不能先用 read_line 全量分配后才检查预算");
    }
}

#[test]
fn splitter_checks_buffer_before_copying_an_oversized_line() {
    let mut splitter = SqlSplitter::with_max_statement_bytes(false, 32);
    let mut reader = NoReadLine(Cursor::new(vec![b'x'; 256]));
    let error = splitter.next_statement(&mut reader).unwrap_err();
    assert!(error.to_string().contains("IMPORT_STATEMENT_LIMIT"));
    assert_eq!(reader.0.position(), 0, "超预算片段不能被复制或消费");
}

#[test]
fn splitter_rejects_unclosed_eof_and_delimiter_budget_without_confusing_keyword_prefixes() {
    for sql in [
        "SELECT 'x",
        "SELECT \"x",
        "SELECT `x",
        "SELECT /* x",
        "DELIMITER\n",
        &format!("DELIMITER {}\n", "$".repeat(129)),
    ] {
        let error = SqlSplitter::new(true)
            .next_statement(&mut Cursor::new(sql.as_bytes()))
            .unwrap_err();
        assert!(error.to_string().contains("IMPORT_PARSE_ERROR"), "{sql}");
    }
    let mut splitter = SqlSplitter::new(true);
    let mut reader = Cursor::new(b"DELIMITERX;SELECT 1;");
    assert_eq!(
        splitter.next_statement(&mut reader).unwrap(),
        Some("DELIMITERX".into())
    );
    assert_eq!(
        splitter.next_statement(&mut reader).unwrap(),
        Some("SELECT 1".into())
    );
    let delimiter = "$".repeat(128);
    let input = format!("DELIMITER {delimiter}\nSELECT 1{delimiter}\n");
    assert_eq!(
        SqlSplitter::new(true)
            .next_statement(&mut Cursor::new(input.as_bytes()))
            .unwrap(),
        Some("SELECT 1".into())
    );
}

#[test]
fn bracket_identifiers_preserve_delimiters_comments_and_sql_server_doubled_closers() {
    let first = "CREATE TABLE [a;b]([x--y] TEXT)";
    let second = "SELECT [semi]];colon], [a]]-- b] FROM [dbo].[t]];name]";
    let input = format!("{first};\n{second}; SELECT 2;");
    let mut reader = Cursor::new(input.as_bytes());
    let mut splitter = SqlSplitter::new(false);
    assert_eq!(
        splitter.next_statement(&mut reader).unwrap(),
        Some(first.into())
    );
    assert_eq!(
        splitter.next_statement(&mut reader).unwrap(),
        Some(second.into())
    );
    assert_eq!(
        splitter.next_statement(&mut reader).unwrap(),
        Some("SELECT 2".into())
    );
    assert!(splitter.next_statement(&mut reader).unwrap().is_none());
    let error = SqlSplitter::new(false)
        .next_statement(&mut Cursor::new(b"SELECT [not_closed"))
        .unwrap_err();
    assert!(error.to_string().contains("IMPORT_PARSE_ERROR"));
}

#[test]
fn create_table_scanning_and_normalization_respect_identifier_segments() {
    for (token, name) in [
        ("[db.with.dot] . [a b;c]", "a b;c"),
        ("\"db\".\"a.b c\"", "a.b c"),
        ("`db`.`a.b c`", "a.b c"),
        ("[dbo].[a]]b.c]", "a]b.c"),
        ("[main].[a b]", "a b"),
        ("\"db\".\"a\"\".b c\"", "a\".b c"),
    ] {
        let sql = format!("CREATE TABLE {token} (value TEXT)");
        let (actual, _, ine) = parse_create_table(&sql).unwrap();
        assert_eq!(actual, token);
        assert_eq!(normalize_table_name(&actual), name);
        assert!(!ine);
        let sql = format!("CREATE TABLE IF NOT EXISTS {token} (value TEXT)");
        let (actual, _, ine) = parse_create_table(&sql).unwrap();
        assert_eq!(actual, token);
        assert!(ine);
    }
    for sql in [
        "CREATE TABLE [not_closed(value TEXT)",
        "CREATE TABLE [a]garbage(value TEXT)",
        "CREATE TABLE main..a(value TEXT)",
        "CREATE TABLE [a] + [b](value TEXT)",
    ] {
        assert!(
            parse_create_table(sql).is_none(),
            "不能从未知语法猜测 overwrite 目标: {sql}"
        );
    }
    assert_eq!(normalize_table_name("[a]garbage"), "");
    assert_eq!(normalize_table_name("[dbo].[not_closed"), "");
}

fn quote_identifier(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

#[tokio::test]
async fn sqlite_original_bracket_and_dotted_ddl_exports_and_imports_without_splitting_names() {
    let source = manager().await;
    let target = manager().await;
    let definitions = [
        ("a;b", "CREATE TABLE [a;b]([x--y] TEXT)"),
        (
            "name.with dot and space",
            "CREATE TABLE [name.with dot and space]([value.name space] TEXT)",
        ),
        (
            "quote\".name space",
            "CREATE TABLE \"quote\"\".name space\"(\"value\"\".column\" TEXT)",
        ),
    ];
    let source_driver = source.get_connection("import").await.unwrap();
    for (index, (name, ddl)) in definitions.iter().enumerate() {
        source_driver.execute(ddl, None, None).await.unwrap();
        source_driver
            .execute_bound(
                &format!("INSERT INTO {} VALUES(?)", quote_identifier(name)),
                &[TypedValue::Text(format!("原值{index};中文\n--仍是内容"))],
                None,
            )
            .await
            .unwrap();
    }
    let file = SqlFile::new([]);
    let exported = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        export_job(
            &source,
            "import".into(),
            None,
            Some(definitions.iter().map(|(name, _)| (*name).into()).collect()),
            file.path(),
            true,
            false,
            true,
            None,
        ),
    )
    .await
    .expect("特殊标识符导出不能阻塞")
    .unwrap();
    assert_eq!(exported.tables, 3);
    assert_eq!(exported.rows, 3);
    assert_eq!(import(&target, &file, "error").await.unwrap().executed, 6);
    let target_driver = target.get_connection("import").await.unwrap();
    for (name, _) in &definitions {
        let sql = format!("SELECT * FROM {}", quote_identifier(name));
        let before = source_driver.query_typed(&sql, &[], None).await.unwrap();
        let after = target_driver.query_typed(&sql, &[], None).await.unwrap();
        assert_eq!(after.typed_rows, before.typed_rows);
        let before = source_driver
            .query_typed(
                "SELECT sql FROM sqlite_schema WHERE name=?",
                &[TypedValue::Text((*name).into())],
                None,
            )
            .await
            .unwrap();
        let after = target_driver
            .query_typed(
                "SELECT sql FROM sqlite_schema WHERE name=?",
                &[TypedValue::Text((*name).into())],
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            after.typed_rows, before.typed_rows,
            "必须保留服务器原始 DDL 的引号语法"
        );
    }
}

#[tokio::test]
async fn skip_matches_actual_names_and_overwrite_drops_complete_quoted_targets() {
    let manager = manager().await;
    let tokens = [
        "[a;b]",
        "[main].[name.with dot and space]",
        "\"quote\"\".name space\"",
    ];
    let names = ["a;b", "name.with dot and space", "quote\".name space"];
    let initial = tokens
        .iter()
        .map(|token| {
            format!("CREATE TABLE {token}(value TEXT); INSERT INTO {token} VALUES('old');")
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(
        import(&manager, &SqlFile::new(initial), "error")
            .await
            .unwrap()
            .executed,
        6
    );
    let creates = tokens
        .iter()
        .map(|token| format!("CREATE TABLE {token}(different_column TEXT);"))
        .collect::<Vec<_>>()
        .join("\n");
    let skipped = import(&manager, &SqlFile::new(creates), "skip")
        .await
        .unwrap();
    assert_eq!(skipped.skipped, 3);
    assert_eq!(skipped.executed, 0);
    let connection = manager.get_connection("import").await.unwrap();
    for name in &names {
        let original = connection
            .query_typed(
                &format!("SELECT value FROM {}", quote_identifier(name)),
                &[],
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            original.typed_rows,
            vec![vec![Some(TypedValue::Text("old".into()))]]
        );
    }
    let replacement = tokens
        .iter()
        .map(|token| {
            format!("CREATE TABLE {token}(fresh TEXT); INSERT INTO {token} VALUES('new');")
        })
        .collect::<Vec<_>>()
        .join("\n");
    let replaced = import(&manager, &SqlFile::new(replacement), "overwrite")
        .await
        .unwrap();
    assert_eq!(replaced.executed, 6);
    for name in &names {
        let current = connection
            .query_typed(
                &format!("SELECT fresh FROM {}", quote_identifier(name)),
                &[],
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            current.typed_rows,
            vec![vec![Some(TypedValue::Text("new".into()))]]
        );
        assert!(connection
            .query_typed(
                &format!("SELECT value FROM {}", quote_identifier(name)),
                &[],
                None
            )
            .await
            .is_err());
    }
}
