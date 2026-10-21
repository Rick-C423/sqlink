//! 真实 SQLite 默认回归：元数据来源、修订、DDL 与连接代次边界。
//! 只使用内存数据库，不依赖网络、凭据或外部数据库实例。

use serde_json::json;
use sqlconnect_lib::{
    database::pool::ConnectionPoolManager,
    metadata::{self, ExecuteDdlRequest, MetadataSnapshot, ObjectIdentity},
    models::ConnectionConfig,
    query_tasks,
    table_result::{self, ContextValidationRequest, TablePageRequest},
    task_registry::{CommitState, TaskPhase},
};
use std::{sync::Arc, time::Duration};
use uuid::Uuid;

const ID: &str = "metadata_service";
const AUX: &str = "attach .\" data";
const TABLE: &str = "same .\" table";
const PARENT: &str = "parent .\" table";
const INDEX: &str = "same .\" index";

fn config() -> ConnectionConfig {
    serde_json::from_value(json!({
        "id": ID, "name": "元数据服务真实回归", "driverType": "sqlite", "host": "",
        "port": 0, "user": "", "password": "", "database": ":memory:"
    }))
    .unwrap()
}

fn quote(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

fn qualified(schema: &str, table: &str) -> String {
    format!("{}.{}", quote(schema), quote(table))
}

async fn execute(manager: &ConnectionPoolManager, sql: &str) {
    manager
        .get_connection(ID)
        .await
        .unwrap()
        .execute(sql, None, None)
        .await
        .unwrap();
}

async fn fixture() -> Arc<ConnectionPoolManager> {
    let manager = Arc::new(ConnectionPoolManager::new(None));
    manager.create_connection(config()).await.unwrap();
    execute(
        &manager,
        &format!("ATTACH DATABASE ':memory:' AS {}", quote(AUX)),
    )
    .await;
    for schema in ["main", AUX] {
        let marker = if schema == "main" {
            "main_marker"
        } else {
            "attach_marker"
        };
        execute(
            &manager,
            &format!(
                "CREATE TABLE {} (a INTEGER NOT NULL, b INTEGER NOT NULL, PRIMARY KEY (b, a))",
                qualified(schema, PARENT)
            ),
        )
        .await;
        execute(&manager, &format!(
            "CREATE TABLE {} (id INTEGER PRIMARY KEY, a INTEGER, b INTEGER, {marker} TEXT, FOREIGN KEY (a, b) REFERENCES {} (b, a) ON DELETE CASCADE)",
            qualified(schema, TABLE), quote(PARENT)
        )).await;
        // SQLite 的 CREATE INDEX 把 schema 放在索引名上，表名仍是单独一段。
        execute(
            &manager,
            &format!(
                "CREATE INDEX {} ON {} (b DESC, a)",
                qualified(schema, INDEX),
                quote(TABLE)
            ),
        )
        .await;
        execute(
            &manager,
            &format!(
                "INSERT INTO {} (id, {marker}) VALUES (1, '原值')",
                qualified(schema, TABLE)
            ),
        )
        .await;
    }
    manager
}

async fn snapshot(manager: &ConnectionPoolManager, schema: &str) -> MetadataSnapshot {
    // 即使请求带了错误文件标签，SQLite 来源也必须冻结为后端连接配置。
    metadata::read(
        manager,
        ID,
        "前端不可采信的文件标签.db",
        &[schema.into(), TABLE.into()],
    )
    .await
    .unwrap()
}

fn ddl_request(snapshot: &MetadataSnapshot, statements: Vec<String>) -> ExecuteDdlRequest {
    ExecuteDdlRequest {
        connection_id: snapshot.connection_id.clone(),
        connection_generation: snapshot.connection_generation.clone(),
        database: snapshot.target.database.clone(),
        table_parts: ObjectIdentity::from(&snapshot.target).parts("sqlite"),
        revision: snapshot.revision.clone(),
        statements,
    }
}

fn has_column(snapshot: &MetadataSnapshot, name: &str) -> bool {
    snapshot
        .target
        .columns
        .iter()
        .any(|column| column.name == name)
}

async fn page(manager: &ConnectionPoolManager, schema: &str) -> table_result::TablePage {
    table_result::read_table_page(
        manager,
        TablePageRequest {
            protocol_version: 1,
            connection_id: ID.into(),
            database: ":memory:".into(),
            table_parts: vec![schema.into(), TABLE.into()],
            page: 1,
            page_size: 10,
            sort: None,
        },
    )
    .await
    .unwrap()
}

fn reference(page: &table_result::TablePage) -> ContextValidationRequest {
    ContextValidationRequest {
        protocol_version: 1,
        result_id: page.context.result_id.clone(),
        revision: page.context.revision,
        row_handles: page.row_handles.iter().flatten().cloned().collect(),
    }
}

#[test]
fn object_identity_parts_never_split_literal_dots_or_quotes() {
    let identity = ObjectIdentity {
        database: "db .\" literal".into(),
        schema: Some(AUX.into()),
        table: TABLE.into(),
    };
    assert_eq!(identity.parts("sqlite"), vec![AUX, TABLE]);
    assert_eq!(identity.parts("postgres"), vec![AUX, TABLE]);
    assert_eq!(identity.parts("mysql"), vec!["db .\" literal", TABLE]);
    assert_eq!(identity.parts("mssql"), vec!["db .\" literal", AUX, TABLE]);
    let single = ObjectIdentity {
        schema: None,
        ..identity
    };
    assert_eq!(single.parts("sqlite"), vec![TABLE]);
}

#[tokio::test]
async fn attached_snapshot_keeps_columns_indexes_and_foreign_keys_on_exact_target() {
    let manager = fixture().await;
    let main = snapshot(&manager, "main").await;
    let attached = snapshot(&manager, AUX).await;
    assert_eq!(main.target.database, ":memory:");
    assert_eq!(attached.target.database, ":memory:");
    assert_eq!(main.connection_generation, attached.connection_generation);
    assert_ne!(main.revision, attached.revision);
    for (snapshot, schema, marker, absent) in [
        (&main, "main", "main_marker", "attach_marker"),
        (&attached, AUX, "attach_marker", "main_marker"),
    ] {
        assert_eq!(snapshot.target.schema.as_deref(), Some(schema));
        assert_eq!(snapshot.target.table, TABLE);
        assert_eq!(snapshot.target.object_kind.as_deref(), Some("table"));
        assert!(has_column(snapshot, marker));
        assert!(!has_column(snapshot, absent));
        let index = snapshot
            .indexes
            .iter()
            .find(|index| index.name == INDEX)
            .unwrap();
        assert_eq!(index.columns, ["b", "a"]);
        assert_eq!(index.key_parts.len(), 2);
        assert_eq!(index.key_parts[0].column.as_deref(), Some("b"));
        assert!(index.key_parts[0].descending);
        assert!(!index.is_unique && !index.is_primary);
        assert_eq!(snapshot.foreign_keys.len(), 1);
        let foreign_key = &snapshot.foreign_keys[0];
        assert_eq!(foreign_key.columns, ["a", "b"]);
        assert_eq!(foreign_key.ref_columns, ["b", "a"]);
        assert_eq!(foreign_key.ref_database.as_deref(), Some(":memory:"));
        assert_eq!(foreign_key.ref_schema.as_deref(), Some(schema));
        assert_eq!(foreign_key.ref_table, PARENT);
        assert_eq!(foreign_key.on_delete.as_deref(), Some("CASCADE"));
        let round_trip = metadata::read(
            &manager,
            ID,
            &snapshot.target.database,
            &ObjectIdentity::from(&snapshot.target).parts("sqlite"),
        )
        .await
        .unwrap();
        assert_eq!(round_trip.revision, snapshot.revision);
    }
    manager.disconnect(ID).await.unwrap();
}

#[tokio::test]
async fn changed_directory_rejects_stale_ddl_without_dropping_original_table() {
    let manager = fixture().await;
    let before = snapshot(&manager, AUX).await;
    execute(
        &manager,
        &format!(
            "ALTER TABLE {} ADD COLUMN external_change TEXT",
            qualified(AUX, TABLE)
        ),
    )
    .await;
    let current = snapshot(&manager, AUX).await;
    assert_ne!(before.revision, current.revision);
    let error = metadata::execute_ddl(
        &manager,
        ddl_request(
            &before,
            vec![format!("DROP TABLE {}", qualified(AUX, TABLE))],
        ),
    )
    .await
    .unwrap_err();
    assert!(
        error.contains("METADATA_REVISION_CHANGED"),
        "错误为：{error}"
    );
    let preserved = snapshot(&manager, AUX).await;
    assert_eq!(preserved.revision, current.revision);
    assert!(has_column(&preserved, "external_change"));
    assert!(!has_column(
        &snapshot(&manager, "main").await,
        "external_change"
    ));
    assert_eq!(page(&manager, AUX).await.result.rows.len(), 1);
    manager.disconnect(ID).await.unwrap();
}

#[tokio::test]
async fn reconnect_rejects_old_metadata_generation_and_checked_coordinator() {
    let manager = fixture().await;
    let old = snapshot(&manager, "main").await;
    manager.disconnect(ID).await.unwrap();
    manager.create_connection(config()).await.unwrap();
    execute(
        &manager,
        &format!(
            "CREATE TABLE {} (id INTEGER PRIMARY KEY, replacement_marker TEXT)",
            qualified("main", TABLE)
        ),
    )
    .await;
    let current = snapshot(&manager, "main").await;
    assert_ne!(old.connection_generation, current.connection_generation);
    let error = metadata::execute_ddl(
        &manager,
        ddl_request(
            &old,
            vec![format!("DROP TABLE {}", qualified("main", TABLE))],
        ),
    )
    .await
    .unwrap_err();
    assert!(
        error.contains("METADATA_CONNECTION_CHANGED"),
        "错误为：{error}"
    );
    let task_id = Uuid::new_v4().to_string();
    let error = query_tasks::execute_batch_checked(
        &manager,
        ID.into(),
        None,
        vec![format!("DROP TABLE {}", qualified("main", TABLE))],
        None,
        Some(task_id.clone()),
        Some(old.connection_generation),
    )
    .await
    .unwrap_err();
    assert!(
        error.contains("METADATA_CONNECTION_CHANGED"),
        "错误为：{error}"
    );
    let task = manager.tasks.snapshot(&task_id).unwrap();
    assert_eq!(task.phase, TaskPhase::Failed);
    assert_eq!(task.commit_state, CommitState::NotStarted);
    assert_eq!(
        task.error_code.as_deref(),
        Some("METADATA_CONNECTION_CHANGED")
    );
    assert_eq!(manager.tasks.active_count(), 0);
    let preserved = snapshot(&manager, "main").await;
    assert!(has_column(&preserved, "replacement_marker"));
    assert_eq!(current.revision, preserved.revision);
    manager.disconnect(ID).await.unwrap();
}

#[tokio::test]
async fn successful_canonical_ddl_targets_attach_then_main_and_invalidates_contexts() {
    let manager = fixture().await;
    let main_before = snapshot(&manager, "main").await;
    let attached_before = snapshot(&manager, AUX).await;
    let main_page = page(&manager, "main").await;
    let attached_page = page(&manager, AUX).await;
    table_result::validate_context(&manager, reference(&main_page))
        .await
        .unwrap();
    table_result::validate_context(&manager, reference(&attached_page))
        .await
        .unwrap();
    metadata::execute_ddl(
        &manager,
        ddl_request(
            &attached_before,
            vec![format!(
                "ALTER TABLE {} ADD COLUMN attach_added TEXT",
                qualified(AUX, TABLE)
            )],
        ),
    )
    .await
    .unwrap();
    for old_page in [&main_page, &attached_page] {
        assert_eq!(
            table_result::validate_context(&manager, reference(old_page))
                .await
                .unwrap_err()
                .code,
            "RESULT_CONTEXT_NOT_FOUND"
        );
    }
    let attached_after = snapshot(&manager, AUX).await;
    let main_after = snapshot(&manager, "main").await;
    assert_ne!(attached_before.revision, attached_after.revision);
    assert_eq!(main_before.revision, main_after.revision);
    assert!(has_column(&attached_after, "attach_added"));
    assert!(!has_column(&main_after, "attach_added"));
    let refreshed_page = page(&manager, AUX).await;
    assert_ne!(
        refreshed_page.context.result_id,
        attached_page.context.result_id
    );
    assert!(refreshed_page
        .result
        .columns
        .iter()
        .any(|column| column.name == "attach_added"));
    table_result::validate_context(&manager, reference(&refreshed_page))
        .await
        .unwrap();
    metadata::execute_ddl(
        &manager,
        ddl_request(
            &main_after,
            vec![format!(
                "ALTER TABLE {} ADD COLUMN main_added INTEGER",
                qualified("main", TABLE)
            )],
        ),
    )
    .await
    .unwrap();
    assert!(has_column(&snapshot(&manager, "main").await, "main_added"));
    assert!(!has_column(&snapshot(&manager, AUX).await, "main_added"));
    assert_eq!(
        table_result::validate_context(&manager, reference(&refreshed_page))
            .await
            .unwrap_err()
            .code,
        "RESULT_CONTEXT_NOT_FOUND"
    );
    manager.disconnect(ID).await.unwrap();
}

#[tokio::test]
async fn queued_checked_batch_never_rebinds_to_replacement_connection() {
    let manager = fixture().await;
    let original = manager.get_connection_snapshot(ID).await.unwrap();
    // 独占租约让后续批次稳定排在 SQLite 租约入口，避免靠 sleep 猜时序。
    let owner = original.driver.lease_session(None).await.unwrap();
    let task_id = Uuid::new_v4().to_string();
    let worker = manager.clone();
    let worker_task_id = task_id.clone();
    let old_generation = original.generation.clone();
    let queued = tokio::spawn(async move {
        query_tasks::execute_batch_checked(
            &worker,
            ID.into(),
            None,
            vec![format!("DROP TABLE {}", qualified("main", TABLE))],
            None,
            Some(worker_task_id),
            Some(old_generation),
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if manager.tasks.snapshot(&task_id).is_ok_and(|task| {
                task.phase == TaskPhase::Preparing
                    && task.connection_generation.as_deref() == Some(original.generation.as_str())
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("批次应已固定旧连接代次并等待租约");
    tokio::time::timeout(Duration::from_secs(5), manager.create_connection(config()))
        .await
        .unwrap()
        .unwrap();
    let _ = owner.release().await;
    assert!(tokio::time::timeout(Duration::from_secs(5), queued)
        .await
        .unwrap()
        .unwrap()
        .is_err());
    execute(
        &manager,
        &format!(
            "CREATE TABLE {} (replacement_marker TEXT)",
            qualified("main", TABLE)
        ),
    )
    .await;
    let current = snapshot(&manager, "main").await;
    assert_ne!(original.generation, current.connection_generation);
    assert!(has_column(&current, "replacement_marker"));
    assert_eq!(
        manager.tasks.snapshot(&task_id).unwrap().commit_state,
        CommitState::NotStarted
    );
    assert_eq!(manager.tasks.active_count(), 0);
    manager.disconnect(ID).await.unwrap();
}

/// 每次测试独占平面目录；只清理本次导出文件，不递归遍历其他目录。
struct ExportDirectory(std::path::PathBuf);
impl ExportDirectory {
    fn new() -> Self {
        let directory =
            std::env::temp_dir().join(format!("sqlink-metadata-export-{}", Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        Self(directory)
    }
    fn path(&self, name: &str) -> std::path::PathBuf {
        self.0.join(name)
    }
    fn no_temporary_files(&self) {
        assert!(
            std::fs::read_dir(&self.0).unwrap().all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".sqlink-export-")),
            "canonical 导出成功、失败均不能遗留临时文件"
        );
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

async fn export_fixture() -> ConnectionPoolManager {
    let manager = ConnectionPoolManager::new(None);
    manager.create_connection(config()).await.unwrap();
    execute(
        &manager,
        &format!("ATTACH DATABASE ':memory:' AS {}", quote(AUX)),
    )
    .await;
    for (schema, column, value) in [
        ("main", "main_value", "main source"),
        (AUX, "attach_value", "attach source"),
        ("temp", "temp_value", "temp source"),
    ] {
        execute(
            &manager,
            &format!(
                "CREATE TABLE {} (id INTEGER PRIMARY KEY, {column} TEXT)",
                qualified(schema, TABLE)
            ),
        )
        .await;
        execute(
            &manager,
            &format!(
                "INSERT INTO {} VALUES (1, '{value}')",
                qualified(schema, TABLE)
            ),
        )
        .await;
    }
    manager
}

async fn canonical_export(
    manager: &ConnectionPoolManager,
    schema: &str,
    table: &str,
    path: &std::path::Path,
    sql: bool,
) -> Result<sqlconnect_lib::models::DumpSummary, String> {
    // 前置元数据选择与导出读取都沿用分段身份，temp 同名对象不能替换已选来源。
    let metadata = metadata::read(manager, ID, ":memory:", &[schema.into(), table.into()])
        .await
        .unwrap();
    tokio::time::timeout(
        Duration::from_secs(5),
        sqlconnect_lib::export::export_job_sources(
            manager,
            ID.into(),
            None,
            None,
            Some(vec![ObjectIdentity::from(&metadata.target)]),
            path.to_str().unwrap().into(),
            sql,
            true,
            true,
            None,
        ),
    )
    .await
    .expect("canonical 导出与清理不能阻塞")
}

#[tokio::test]
async fn canonical_csv_exports_exact_main_attached_and_temp_sources_with_literal_names() {
    let directory = ExportDirectory::new();
    let manager = export_fixture().await;
    for (schema, column, value, file) in [
        ("main", "main_value", "main source", "main.csv"),
        (AUX, "attach_value", "attach source", "attach.csv"),
        ("temp", "temp_value", "temp source", "temp.csv"),
    ] {
        let path = directory.path(file);
        std::fs::write(&path, b"previous file").unwrap();
        let summary = canonical_export(&manager, schema, TABLE, &path, false)
            .await
            .unwrap();
        assert_eq!(summary.tables, 1);
        assert_eq!(summary.rows, 1);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            format!("\"id\",\"{column}\"\n1,\"{value}\"\n")
        );
        directory.no_temporary_files();
    }
    assert_eq!(manager.tasks.active_count(), 0);
    manager.disconnect(ID).await.unwrap();
}

#[tokio::test]
async fn empty_explicit_export_selection_never_expands_to_whole_database() {
    let directory = ExportDirectory::new();
    let manager = export_fixture().await;
    let path = directory.path("empty.sql");
    std::fs::write(&path, b"previous file").unwrap();
    let error = sqlconnect_lib::export::export_job_sources(
        &manager,
        ID.into(),
        None,
        None,
        Some(vec![]),
        path.to_str().unwrap().into(),
        true,
        true,
        true,
        None,
    )
    .await
    .unwrap_err();
    assert!(error.contains("显式对象清单不能为空"), "{error}");
    assert_eq!(std::fs::read(&path).unwrap(), b"previous file");
    directory.no_temporary_files();
    assert_eq!(manager.tasks.active_count(), 0);
    manager.disconnect(ID).await.unwrap();
}

#[tokio::test]
async fn canonical_main_sql_dump_ignores_temp_shadow_and_replays_real_source() {
    let directory = ExportDirectory::new();
    let source = export_fixture().await;
    let path = directory.path("main.sql");
    let summary = canonical_export(&source, "main", TABLE, &path, true)
        .await
        .unwrap();
    assert_eq!(summary.rows, 1);
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("main_value") && text.contains("main source"));
    assert!(!text.contains("temp_value") && !text.contains("temp source"));
    assert!(!text.contains("attach_value") && !text.contains("attach source"));
    assert!(text.contains(&format!(
        "DROP TABLE IF EXISTS {}",
        qualified("main", TABLE)
    )));
    assert!(text.contains(&format!("CREATE TABLE {}", qualified("main", TABLE))));
    // 在原连接回放，temp 和 ATTACH 同名表必须始终保持自己的列和值。
    sqlconnect_lib::commands::import::import_sql_file(
        &source,
        ID.into(),
        None,
        path.to_str().unwrap().into(),
        "error".into(),
    )
    .await
    .unwrap();
    for (schema, column, value, file) in [
        ("main", "main_value", "main source", "same-session-main.csv"),
        (
            AUX,
            "attach_value",
            "attach source",
            "same-session-attach.csv",
        ),
        ("temp", "temp_value", "temp source", "same-session-temp.csv"),
    ] {
        let csv = directory.path(file);
        canonical_export(&source, schema, TABLE, &csv, false)
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(csv).unwrap(),
            format!("\"id\",\"{column}\"\n1,\"{value}\"\n")
        );
    }
    let restored = ConnectionPoolManager::new(None);
    restored.create_connection(config()).await.unwrap();
    sqlconnect_lib::commands::import::import_sql_file(
        &restored,
        ID.into(),
        None,
        path.to_str().unwrap().into(),
        "error".into(),
    )
    .await
    .unwrap();
    let metadata = snapshot(&restored, "main").await;
    assert!(has_column(&metadata, "main_value"));
    assert!(!has_column(&metadata, "temp_value"));
    let result = restored
        .get_connection(ID)
        .await
        .unwrap()
        .query(
            &format!("SELECT main_value FROM {}", qualified("main", TABLE)),
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(result.rows[0].values[0], Some(json!("main source")));
    directory.no_temporary_files();
    source.disconnect(ID).await.unwrap();
    restored.disconnect(ID).await.unwrap();
}

#[tokio::test]
async fn canonical_non_main_sql_dump_rejects_unsupported_schema_and_preserves_old_file() {
    let directory = ExportDirectory::new();
    let manager = export_fixture().await;
    let old = b"\x00\xffold SQL output\n";
    for (schema, file) in [(AUX, "attach.sql"), ("temp", "temp.sql")] {
        let path = directory.path(file);
        std::fs::write(&path, old).unwrap();
        let error = canonical_export(&manager, schema, TABLE, &path, true)
            .await
            .unwrap_err();
        assert!(
            error.contains("EXPORT_SCHEMA_UNSUPPORTED"),
            "错误为：{error}"
        );
        assert_eq!(std::fs::read(&path).unwrap(), old);
        directory.no_temporary_files();
    }
    assert!(has_column(&snapshot(&manager, "main").await, "main_value"));
    assert_eq!(manager.tasks.active_count(), 0);
    manager.disconnect(ID).await.unwrap();
}

#[tokio::test]
async fn canonical_csv_keeps_cell_row_and_atomic_failure_limits() {
    let directory = ExportDirectory::new();
    let manager = export_fixture().await;
    let views = [
        (
            "cell limit .\" view",
            "SELECT zeroblob(1048577) AS payload".to_string(),
        ),
        (
            "row limit .\" view",
            format!(
                "SELECT {}",
                (0..17)
                    .map(|index| format!("zeroblob(1048576) AS c{index}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ),
    ];
    let path = directory.path("bounded.csv");
    let old = b"\x00\xffprevious exact output\n";
    for (view, select) in views {
        execute(
            &manager,
            &format!("CREATE VIEW {} AS {select}", qualified(AUX, view)),
        )
        .await;
        std::fs::write(&path, old).unwrap();
        let error = canonical_export(&manager, AUX, view, &path, false)
            .await
            .unwrap_err();
        assert!(error.contains("EXPORT_VALUE_LIMIT"), "错误为：{error}");
        assert_eq!(std::fs::read(&path).unwrap(), old);
        directory.no_temporary_files();
    }
    assert_eq!(manager.tasks.active_count(), 0);
    manager.disconnect(ID).await.unwrap();
}

#[tokio::test]
async fn sqlite_import_skip_checks_current_session_schema_and_repeated_create_targets() {
    let directory = ExportDirectory::new();
    let manager = ConnectionPoolManager::new(None);
    manager.create_connection(config()).await.unwrap();
    execute(
        &manager,
        &format!("ATTACH DATABASE ':memory:' AS {}", quote(AUX)),
    )
    .await;
    execute(
        &manager,
        &format!(
            "CREATE TABLE {} (temp_marker TEXT)",
            qualified("temp", TABLE)
        ),
    )
    .await;
    execute(
        &manager,
        &format!(
            "INSERT INTO {} VALUES ('temp original')",
            qualified("temp", TABLE)
        ),
    )
    .await;
    let other = "only attach .\" table";
    execute(
        &manager,
        &format!(
            "CREATE TABLE {} (attached_original TEXT)",
            qualified(AUX, other)
        ),
    )
    .await;
    execute(
        &manager,
        &format!(
            "INSERT INTO {} VALUES ('attach original')",
            qualified(AUX, other)
        ),
    )
    .await;
    let path = directory.path("skip.sql");
    let script = [
        format!("CREATE TABLE {} (main_marker TEXT);", quote(TABLE)),
        format!(
            "INSERT INTO {} VALUES ('main new');",
            qualified("main", TABLE)
        ),
        format!(
            "CREATE TABLE {} (wrong_main_column TEXT);",
            qualified("main", TABLE)
        ),
        format!(
            "CREATE TABLE {} (attach_marker TEXT);",
            qualified(AUX, TABLE)
        ),
        format!(
            "INSERT INTO {} VALUES ('attach new');",
            qualified(AUX, TABLE)
        ),
        format!(
            "CREATE TABLE {} (wrong_attach_column TEXT);",
            qualified(AUX, TABLE)
        ),
        format!(
            "CREATE TABLE {} (wrong_temp_column TEXT);",
            qualified("temp", TABLE)
        ),
        format!("CREATE TABLE {} (new_main_only TEXT);", quote(other)),
        format!(
            "INSERT INTO {} VALUES ('main only new');",
            qualified("main", other)
        ),
        format!(
            "CREATE TABLE {} (wrong_repeated_column TEXT);",
            qualified("main", other)
        ),
    ]
    .join("\n");
    std::fs::write(&path, script).unwrap();
    let summary = sqlconnect_lib::commands::import::import_sql_file(
        &manager,
        ID.into(),
        None,
        path.to_str().unwrap().into(),
        "skip".into(),
    )
    .await
    .unwrap();
    assert_eq!(summary.statements, 10);
    assert_eq!(summary.executed, 6);
    assert_eq!(summary.skipped, 4);
    for (schema, table, column, expected) in [
        ("main", TABLE, "main_marker", "main new"),
        (AUX, TABLE, "attach_marker", "attach new"),
        ("temp", TABLE, "temp_marker", "temp original"),
        ("main", other, "new_main_only", "main only new"),
        (AUX, other, "attached_original", "attach original"),
    ] {
        let metadata = metadata::read(&manager, ID, ":memory:", &[schema.into(), table.into()])
            .await
            .unwrap();
        assert_eq!(metadata.target.columns.len(), 1);
        assert_eq!(metadata.target.columns[0].name, column);
        let result = manager
            .get_connection(ID)
            .await
            .unwrap()
            .query(
                &format!("SELECT {} FROM {}", quote(column), qualified(schema, table)),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(result.rows[0].values[0], Some(json!(expected)));
    }
    directory.no_temporary_files();
    manager.disconnect(ID).await.unwrap();
}
