//! 仅使用独立 SQLite 内存库和本次唯一目录，验证文件导出及原值回放。
use sqlconnect_lib::{
    commands::import::import_sql_file,
    database::{dump::SqlSplitter, pool::ConnectionPoolManager, DatabaseDriver},
    export::{export_csv, export_job, MAX_LEGACY_DUMP_BYTES, MAX_SQL_STATEMENT_BYTES},
    models::ConnectionConfig,
    task_registry::{CleanupState, TaskPhase},
    typed_value::{TypedValue, MAX_CELL_BYTES},
};
use std::{
    future::Future,
    io::BufReader,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

const DEADLINE: Duration = Duration::from_secs(5);

struct TestDirectory(PathBuf);
impl TestDirectory {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("sqlink-sqlite-export-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
    fn no_temporary_files(&self) {
        for entry in std::fs::read_dir(&self.0).unwrap() {
            let entry = entry.unwrap();
            assert!(
                !entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".sqlink-export-"),
                "不能遗留未提交的导出临时文件"
            );
        }
    }
}
impl Drop for TestDirectory {
    fn drop(&mut self) {
        // 目录只属于本测试，且只含平面文件；不递归进入其他路径。
        if let Ok(entries) = std::fs::read_dir(&self.0) {
            for entry in entries.flatten() {
                let _ = std::fs::remove_file(entry.path());
            }
        }
        let _ = std::fs::remove_dir(&self.0);
    }
}

async fn bounded<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(DEADLINE, future)
        .await
        .expect("导出、回放或租约清理超过五秒")
}
fn as_string(path: &Path) -> String {
    path.to_str().expect("测试目录应是 UTF-8").to_owned()
}

async fn manager(database: &str) -> ConnectionPoolManager {
    let manager = ConnectionPoolManager::new(None);
    let config: ConnectionConfig = serde_json::from_value(serde_json::json!({
        "id":"test", "name":"SQLite原值导出回归", "driverType":"sqlite",
        "host":"", "port":0, "user":"", "password":"", "database":database,
    }))
    .unwrap();
    bounded(manager.create_connection(config)).await.unwrap();
    manager
}
async fn driver(manager: &ConnectionPoolManager) -> Arc<dyn DatabaseDriver> {
    manager.get_connection("test").await.unwrap()
}
async fn execute(manager: &ConnectionPoolManager, sql: &str) {
    bounded(driver(manager).await.execute(sql, None, None))
        .await
        .unwrap();
}
async fn scalar(manager: &ConnectionPoolManager, sql: &str) -> i64 {
    bounded(driver(manager).await.query(sql, None, None))
        .await
        .unwrap()
        .rows[0]
        .values[0]
        .as_ref()
        .unwrap()
        .as_i64()
        .unwrap()
}
async fn csv(manager: &ConnectionPoolManager, table: &str, path: &Path) -> Result<u64, String> {
    bounded(export_csv(
        &*driver(manager).await,
        None,
        table,
        &as_string(path),
    ))
    .await
    .map_err(|e| e.to_string())
}
async fn dump(
    manager: &ConnectionPoolManager,
    tables: &[&str],
    path: &Path,
) -> Result<sqlconnect_lib::models::DumpSummary, String> {
    bounded(export_job(
        manager,
        "test".into(),
        None,
        Some(tables.iter().map(|t| (*t).into()).collect()),
        as_string(path),
        true,
        true,
        true,
        None,
    ))
    .await
}
async fn replay(manager: &ConnectionPoolManager, path: &Path) {
    bounded(import_sql_file(
        manager,
        "test".into(),
        None,
        as_string(path),
        "error".into(),
    ))
    .await
    .unwrap();
}

#[tokio::test]
async fn csv_streams_more_than_ten_thousand_rows_without_truncation() {
    let directory = TestDirectory::new();
    let manager = manager(":memory:").await;
    execute(&manager, "CREATE TABLE many(value INTEGER)").await;
    execute(&manager, "WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<10001) INSERT INTO many SELECT x FROM n").await;
    let path = directory.path("many.csv");
    assert_eq!(csv(&manager, "many", &path).await.unwrap(), 10001);
    let text = std::fs::read_to_string(&path).unwrap();
    assert_eq!(text.lines().count(), 10002);
    assert!(text.starts_with("\"value\"\n1\n"));
    assert!(text.ends_with("10001\n"));
    execute(&manager, "CREATE TABLE empty_values(value)").await;
    let empty = directory.path("empty.csv");
    assert_eq!(csv(&manager, "empty_values", &empty).await.unwrap(), 0);
    assert_eq!(std::fs::read(&empty).unwrap(), b"\"value\"\n");
    directory.no_temporary_files();
}

#[tokio::test]
async fn csv_exact_bytes_distinguish_null_empty_text_binary_and_native_storage_classes() {
    let directory = TestDirectory::new();
    let manager = manager(":memory:").await;
    execute(&manager, "CREATE TABLE exact_values(n,empty_text,slash_n,empty_binary,bytes,integer_value,real_value,decimal_text,unicode_text,nul_text)").await;
    let values = vec![
        TypedValue::Null,
        TypedValue::Text(String::new()),
        TypedValue::Text("\\N".into()),
        TypedValue::binary(&[]),
        TypedValue::binary(&[0, 255, 0, 39, 92]),
        TypedValue::Integer("9007199254740993".into()),
        TypedValue::Float(1.25),
        TypedValue::Text("12345678901234567890.12345678901234567890".into()),
        TypedValue::Text("中文😀,\"引号\"\n第二行".into()),
        TypedValue::Text("A\0B".into()),
    ];
    bounded(driver(&manager).await.execute_bound(
        "INSERT INTO exact_values VALUES(?,?,?,?,?,?,?,?,?,?)",
        &values,
        None,
    ))
    .await
    .unwrap();
    let path = directory.path("exact.csv");
    assert_eq!(csv(&manager, "exact_values", &path).await.unwrap(), 1);
    let expected = "\"n\",\"empty_text\",\"slash_n\",\"empty_binary\",\"bytes\",\"integer_value\",\"real_value\",\"decimal_text\",\"unicode_text\",\"nul_text\"\n\\N,\"\",\"\\N\",\"\\x\",\"\\x00ff00275c\",9007199254740993,1.25,\"12345678901234567890.12345678901234567890\",\"中文😀,\"\"引号\"\"\n第二行\",\"A\0B\"\n";
    assert_eq!(std::fs::read(&path).unwrap(), expected.as_bytes());
    let result = bounded(driver(&manager).await.query_typed(
        "SELECT typeof(real_value),typeof(decimal_text) FROM exact_values",
        &[],
        None,
    ))
    .await
    .unwrap();
    assert_eq!(
        result.typed_rows[0],
        vec![
            Some(TypedValue::Text("real".into())),
            Some(TypedValue::Text("text".into()))
        ]
    );
    directory.no_temporary_files();
}

#[tokio::test]
async fn sql_round_trip_preserves_mixed_affinity_free_values_and_binary_types() {
    let directory = TestDirectory::new();
    let source = manager(":memory:").await;
    let target = manager(":memory:").await;
    execute(&source, "CREATE TABLE mixed(id INTEGER PRIMARY KEY,value)").await;
    let values = vec![
        TypedValue::Integer(i64::MIN.to_string()),
        TypedValue::Integer("9007199254740993".into()),
        TypedValue::Text("9007199254740993".into()),
        TypedValue::Text("\\N".into()),
        TypedValue::Text("'\";中文😀\nA\0B\\".into()),
        TypedValue::Text(String::new()),
        TypedValue::binary(&[]),
        TypedValue::binary(&[0, 255, 39, 92]),
        TypedValue::Float(1.25),
        TypedValue::Null,
    ];
    for (index, value) in values.iter().enumerate() {
        bounded(driver(&source).await.execute_bound(
            "INSERT INTO mixed VALUES(?,?)",
            &[TypedValue::Integer(index.to_string()), value.clone()],
            None,
        ))
        .await
        .unwrap();
    }
    let path = directory.path("mixed.sql");
    assert_eq!(
        dump(&source, &["mixed"], &path).await.unwrap().rows,
        values.len() as u64
    );
    replay(&target, &path).await;
    let query = "SELECT id,typeof(value),value FROM mixed ORDER BY id";
    let before = bounded(driver(&source).await.query_typed(query, &[], None))
        .await
        .unwrap();
    let after = bounded(driver(&target).await.query_typed(query, &[], None))
        .await
        .unwrap();
    assert_eq!(before.typed_rows, after.typed_rows);
    assert_eq!(
        after.typed_rows[2][1],
        Some(TypedValue::Text("text".into()))
    );
    assert_eq!(
        after.typed_rows[7][1],
        Some(TypedValue::Text("blob".into()))
    );
    directory.no_temporary_files();
}

#[tokio::test]
async fn sql_preserves_original_ddl_and_omits_generated_columns_from_insert_projection() {
    let directory = TestDirectory::new();
    let source = manager(":memory:").await;
    let target = manager(":memory:").await;
    let ddl = "CREATE TABLE generated_values(id INTEGER PRIMARY KEY,value INTEGER,twice INTEGER GENERATED ALWAYS AS (value*2) STORED,label TEXT GENERATED ALWAYS AS ('值='||value) VIRTUAL)";
    execute(&source, ddl).await;
    execute(
        &source,
        "INSERT INTO generated_values(id,value) VALUES(1,21),(2,7)",
    )
    .await;
    let path = directory.path("generated.sql");
    assert_eq!(
        dump(&source, &["generated_values"], &path)
            .await
            .unwrap()
            .rows,
        2
    );
    let text = std::fs::read_to_string(&path).unwrap();
    // 仅目标名称限定 main，生成列、表达式与其余原始定义必须逐字节保留。
    let span = sqlconnect_lib::database::dump::create_table_target_span(ddl).unwrap();
    let expected = format!(
        "{}\"main\".{}{}",
        &ddl[..span.start],
        &ddl[span.clone()],
        &ddl[span.end..]
    );
    assert!(text.contains(&expected));
    assert!(text.contains("(\"id\", \"value\") VALUES"));
    replay(&target, &path).await;
    let query = "SELECT id,value,twice,label FROM generated_values ORDER BY id";
    assert_eq!(
        bounded(driver(&source).await.query_typed(query, &[], None))
            .await
            .unwrap()
            .typed_rows,
        bounded(driver(&target).await.query_typed(query, &[], None))
            .await
            .unwrap()
            .typed_rows
    );
    directory.no_temporary_files();
}

#[tokio::test]
async fn raw_one_mebibyte_text_and_binary_cells_are_accepted_in_full() {
    let directory = TestDirectory::new();
    let manager = manager(":memory:").await;
    execute(&manager, "CREATE TABLE boundary(text_value,binary_value)").await;
    let text = "x".repeat(MAX_CELL_BYTES);
    let bytes = vec![0u8; MAX_CELL_BYTES];
    bounded(driver(&manager).await.execute_bound(
        "INSERT INTO boundary VALUES(?,?)",
        &[TypedValue::Text(text.clone()), TypedValue::binary(&bytes)],
        None,
    ))
    .await
    .unwrap();
    let path = directory.path("boundary.csv");
    assert_eq!(csv(&manager, "boundary", &path).await.unwrap(), 1);
    let output = std::fs::read_to_string(&path).unwrap();
    let header = "\"text_value\",\"binary_value\"\n";
    assert_eq!(output.len(), header.len() + MAX_CELL_BYTES * 3 + 8);
    assert!(output.starts_with(&format!("{header}\"{text}\",\"\\x")));
    assert!(output.ends_with("000000\"\n"));
    directory.no_temporary_files();
}

#[tokio::test]
async fn cell_and_row_limits_preserve_previous_target_bytes_and_clean_temporary_files() {
    let directory = TestDirectory::new();
    let manager = manager(":memory:").await;
    execute(
        &manager,
        "CREATE VIEW oversized_blob AS SELECT zeroblob(1048577) AS payload",
    )
    .await;
    execute(
        &manager,
        "CREATE VIEW oversized_text AS SELECT CAST(zeroblob(1048577) AS TEXT) AS payload",
    )
    .await;
    let wide = (0..17)
        .map(|n| format!("zeroblob(1048576) AS c{n}"))
        .collect::<Vec<_>>()
        .join(",");
    execute(
        &manager,
        &format!("CREATE VIEW oversized_row AS SELECT {wide}"),
    )
    .await;
    let path = directory.path("old.csv");
    let old = b"\x00\xffprevious exact bytes\n";
    for table in ["oversized_blob", "oversized_text", "oversized_row"] {
        std::fs::write(&path, old).unwrap();
        let error = csv(&manager, table, &path).await.unwrap_err();
        assert!(error.contains("EXPORT_VALUE_LIMIT"), "{error}");
        assert_eq!(std::fs::read(&path).unwrap(), old);
        assert_eq!(scalar(&manager, "SELECT 42").await, 42);
        directory.no_temporary_files();
    }
}

#[tokio::test]
async fn large_sql_file_succeeds_with_bounded_batches_and_replay_while_legacy_string_refuses() {
    let directory = TestDirectory::new();
    let source = manager(":memory:").await;
    execute(&source, "CREATE TABLE bulk(id INTEGER PRIMARY KEY,payload)").await;
    bounded(driver(&source).await.execute_bound(
        "INSERT INTO bulk VALUES(1,?)",
        &[TypedValue::Text("x".repeat(MAX_CELL_BYTES))],
        None,
    ))
    .await
    .unwrap();
    execute(&source, "WITH RECURSIVE n(x) AS (VALUES(2) UNION ALL SELECT x+1 FROM n WHERE x<18) INSERT INTO bulk SELECT x,payload FROM n CROSS JOIN bulk original WHERE original.id=1").await;
    let path = directory.path("large.sql");
    assert_eq!(dump(&source, &["bulk"], &path).await.unwrap().rows, 18);
    assert!(std::fs::metadata(&path).unwrap().len() > MAX_LEGACY_DUMP_BYTES as u64);
    let mut reader = BufReader::new(std::fs::File::open(&path).unwrap());
    let mut splitter = SqlSplitter::with_max_statement_bytes(false, MAX_SQL_STATEMENT_BYTES);
    let mut batches = 0;
    while let Some(statement) = splitter.next_statement(&mut reader).unwrap() {
        assert!(statement.len() <= MAX_SQL_STATEMENT_BYTES);
        if statement.contains("INSERT INTO") {
            batches += 1;
        }
    }
    assert!(batches >= 3, "大行必须按字节预算拆批，不能只依赖每批一百行");
    let target = manager(":memory:").await;
    replay(&target, &path).await;
    assert_eq!(scalar(&target, "SELECT count(*) FROM bulk").await, 18);
    assert_eq!(
        scalar(&target, "SELECT min(length(payload)) FROM bulk").await,
        MAX_CELL_BYTES as i64
    );
    assert_eq!(
        scalar(&target, "SELECT max(length(payload)) FROM bulk").await,
        MAX_CELL_BYTES as i64
    );
    let error = bounded(driver(&source).await.dump_table(None, "bulk", false, true))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("16 MiB"), "{error}");
    assert_eq!(scalar(&source, "SELECT count(*) FROM bulk").await, 18);
    directory.no_temporary_files();
}

#[tokio::test]
async fn cancellation_reaches_terminal_only_after_export_cleanup_and_preserves_old_file() {
    let directory = TestDirectory::new();
    let manager = manager(":memory:").await;
    execute(&manager, "CREATE VIEW endless AS WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<1000000000) SELECT x,printf('%0100d',x) AS payload FROM n").await;
    let path = directory.path("cancel.csv");
    let old = b"original target\0\xff";
    std::fs::write(&path, old).unwrap();
    let task_id = uuid::Uuid::new_v4().to_string();
    let registry = manager.tasks.clone();
    let baseline = registry.active_count();
    let export_manager = manager.clone();
    let export_path = as_string(&path);
    let export_task_id = task_id.clone();
    let job = tokio::spawn(async move {
        export_job(
            &export_manager,
            "test".into(),
            None,
            Some(vec!["endless".into()]),
            export_path,
            false,
            false,
            true,
            Some(export_task_id),
        )
        .await
    });
    bounded(async {
        loop {
            if registry
                .snapshot(&task_id)
                .is_ok_and(|s| s.phase == TaskPhase::Running)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert_eq!(registry.active_count(), baseline + 1);
    let requested = bounded(registry.request_cancel(&task_id)).await.unwrap();
    assert!(requested.cancel_requested);
    assert!(
        !requested.phase.terminal(),
        "取消请求本身不能冒充导出和清理已经结束"
    );
    assert_eq!(requested.completed_at, None);
    assert!(bounded(job).await.unwrap().is_err());
    let stopped = registry.snapshot(&task_id).unwrap();
    assert_eq!(stopped.phase, TaskPhase::Cancelled);
    assert_eq!(stopped.cleanup_state, CleanupState::Succeeded);
    assert!(stopped.completed_at.is_some());
    assert_eq!(registry.active_count(), baseline);
    assert_eq!(std::fs::read(&path).unwrap(), old);
    assert!(registry.request_cancel(&task_id).await.is_err());
    let mut next = bounded(driver(&manager).await.lease_session(None))
        .await
        .unwrap();
    assert_eq!(
        bounded(next.query("SELECT 42")).await.unwrap().rows[0].values[0],
        Some(serde_json::json!(42))
    );
    bounded(next.release()).await.unwrap();
    directory.no_temporary_files();
}

#[tokio::test]
async fn active_main_attached_database_and_log_paths_are_never_export_targets() {
    let directory = TestDirectory::new();
    let main = directory.path("main.sqlite");
    let attached = directory.path("attached.sqlite");
    let manager = manager(&as_string(&main)).await;
    let connection = driver(&manager).await;
    bounded(connection.execute_bound(
        "ATTACH DATABASE ? AS extra",
        &[TypedValue::Text(as_string(&attached))],
        None,
    ))
    .await
    .unwrap();
    execute(&manager, "PRAGMA main.journal_mode=WAL").await;
    execute(&manager, "PRAGMA extra.journal_mode=WAL").await;
    execute(&manager, "CREATE TABLE safe(value)").await;
    execute(&manager, "INSERT INTO safe VALUES(42)").await;
    execute(&manager, "CREATE TABLE extra.attached_values(value)").await;
    for source in [&main, &attached] {
        for suffix in ["", "-wal", "-shm", "-journal"] {
            let path = PathBuf::from(format!("{}{suffix}", source.display()));
            let before = std::fs::read(&path).ok();
            let error = csv(&manager, "safe", &path).await.unwrap_err();
            assert!(error.contains("活动 SQLite 数据库或日志文件"), "{error}");
            assert_eq!(std::fs::read(&path).ok(), before);
            directory.no_temporary_files();
        }
        let before = std::fs::read(source).unwrap();
        assert!(dump(&manager, &["safe"], source).await.is_err());
        assert_eq!(std::fs::read(source).unwrap(), before);
    }
    assert_eq!(scalar(&manager, "SELECT value FROM safe").await, 42);
    assert_eq!(
        scalar(&manager, "SELECT count(*) FROM extra.attached_values").await,
        0
    );
    bounded(manager.disconnect("test")).await.unwrap();
    directory.no_temporary_files();
}

#[tokio::test]
async fn unsupported_utf8_payload_fails_without_display_fallback_or_target_replacement() {
    let directory = TestDirectory::new();
    let manager = manager(":memory:").await;
    execute(
        &manager,
        "CREATE VIEW invalid_text AS SELECT CAST(X'FF' AS TEXT) AS value",
    )
    .await;
    let path = directory.path("invalid.csv");
    let old = b"keep original";
    std::fs::write(&path, old).unwrap();
    let error = csv(&manager, "invalid_text", &path).await.unwrap_err();
    assert!(error.contains("EXPORT_TYPE_UNSUPPORTED"), "{error}");
    assert_eq!(std::fs::read(&path).unwrap(), old);
    assert_eq!(scalar(&manager, "SELECT 42").await, 42);
    directory.no_temporary_files();
}
