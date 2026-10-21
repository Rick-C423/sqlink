//! 真实 SQLite 读取、注册表与连接管理的端到端验证，不使用驱动 mock。
use serde_json::json;
use sqlconnect_lib::database::pool::ConnectionPoolManager;
use sqlconnect_lib::models::ConnectionConfig;
use sqlconnect_lib::result_context::{ResultContext, ResultContextSeed, RowIdentityValue};
use sqlconnect_lib::table_result::{
    self, ContextReference, ContextValidationRequest, TablePageRequest,
};

fn config(id: &str) -> ConnectionConfig {
    serde_json::from_value(json!({"id":id,"name":"可信结果测试","driverType":"sqlite","host":"","port":0,"user":"","password":"","database":":memory:"})).unwrap()
}

async fn setup(id: &str, ddl: &str) -> ConnectionPoolManager {
    let manager = ConnectionPoolManager::new(None);
    manager.create_connection(config(id)).await.unwrap();
    execute(&manager, id, ddl).await;
    manager
}

async fn execute(manager: &ConnectionPoolManager, id: &str, sql: &str) {
    // 驱动 execute 是单语句接口；夹具脚本必须显式分句，保证后续插入确实执行。
    let driver = manager.get_connection(id).await.unwrap();
    let mut reader = std::io::Cursor::new(sql.as_bytes());
    let mut splitter = sqlconnect_lib::database::dump::SqlSplitter::new(false);
    while let Some(statement) = splitter.next_statement(&mut reader).unwrap() {
        driver.execute(&statement, None, None).await.unwrap();
    }
}

fn request(id: &str, table: &str) -> TablePageRequest {
    serde_json::from_value(json!({"protocolVersion":1,"connectionId":id,"database":"main","tableParts":[table],"page":1,"pageSize":20,"sort":null})).unwrap()
}

fn reference(context: &ResultContext, handles: Vec<String>) -> ContextValidationRequest {
    ContextValidationRequest {
        protocol_version: 1,
        result_id: context.result_id.clone(),
        revision: context.revision,
        row_handles: handles,
    }
}

#[tokio::test]
async fn sqlite_controlled_read_keeps_bigint_identity_private_and_exact() {
    let manager = setup("A", "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT); INSERT INTO users VALUES(9007199254740993,'原值');").await;
    let mut page = table_result::read_table_page(&manager, request("A", "users"))
        .await
        .unwrap();
    assert_eq!(page.context.source.database, ":memory:");
    assert_eq!(page.context.source.schema.as_deref(), Some("main"));
    assert_eq!(page.context.source.kind, "table");
    assert_eq!(page.total_rows, None);
    assert!(page.context.edit_policy.allowed_operations.is_empty());
    assert!(page.context.columns.iter().all(|column| !column.writable));
    assert_eq!(
        page.context.columns[0].origin.as_ref().unwrap().column,
        "id"
    );
    assert_eq!(
        page.result.rows[0].values[0],
        Some(json!("9007199254740993"))
    );
    let handle = page.row_handles[0].clone().unwrap();
    assert!(!handle.contains("9007199254740993"));
    table_result::validate_context(&manager, reference(&page.context, vec![handle.clone()]))
        .await
        .unwrap();
    // 修改显示键不改变后端真实定位；真实主键和原快照不通过 validate IPC 返回。
    page.result.rows[0].values[0] = Some(json!(2));
    let identities = manager
        .validate_result_handles(&page.context, &[handle])
        .await
        .unwrap();
    assert_eq!(
        identities[0].keys,
        vec![(
            "id".to_string(),
            RowIdentityValue::Integer("9007199254740993".into())
        )]
    );
    assert_eq!(
        identities[0].original_values[0],
        Some(RowIdentityValue::Integer("9007199254740993".into()))
    );
    let public = serde_json::to_value(&page.context).unwrap();
    assert!(public.get("keys").is_none());
    assert!(public.get("originalValues").is_none());
    assert!(!serde_json::to_string(&public)
        .unwrap()
        .contains("9007199254740993"));
    manager.disconnect("A").await.unwrap();
    assert_eq!(
        manager
            .result_contexts
            .stats(chrono::Utc::now().timestamp_millis())
            .unwrap()
            .contexts,
        0
    );
}

#[tokio::test]
async fn sqlite_handles_reject_cross_result_cross_connection_fake_and_stale_revision() {
    let manager = setup(
        "A",
        "CREATE TABLE users(id INTEGER PRIMARY KEY); INSERT INTO users VALUES(1);",
    )
    .await;
    manager.create_connection(config("B")).await.unwrap();
    execute(
        &manager,
        "B",
        "CREATE TABLE users(id INTEGER PRIMARY KEY); INSERT INTO users VALUES(1);",
    )
    .await;
    let first = table_result::read_table_page(&manager, request("A", "users"))
        .await
        .unwrap();
    let second = table_result::read_table_page(&manager, request("A", "users"))
        .await
        .unwrap();
    let other = table_result::read_table_page(&manager, request("B", "users"))
        .await
        .unwrap();
    let handle = first.row_handles[0].clone().unwrap();
    for context in [&second.context, &other.context] {
        let error =
            table_result::validate_context(&manager, reference(context, vec![handle.clone()]))
                .await
                .unwrap_err();
        assert_eq!(error.code, "INVALID_ROW_HANDLE");
    }
    let error = table_result::validate_context(
        &manager,
        reference(&first.context, vec![uuid::Uuid::new_v4().to_string()]),
    )
    .await
    .unwrap_err();
    assert_eq!(error.code, "INVALID_ROW_HANDLE");
    let mut stale = reference(&first.context, vec![handle]);
    stale.revision += 1;
    assert_eq!(
        table_result::validate_context(&manager, stale)
            .await
            .unwrap_err()
            .code,
        "STALE_REVISION"
    );
    // 请求体不能借连接或表名重新绑定已存在结果。
    assert!(serde_json::from_value::<ContextValidationRequest>(json!({"protocolVersion":1,"resultId":first.context.result_id,"revision":1,"rowHandles":[],"connectionId":"B"})).is_err());
}

#[tokio::test]
async fn sqlite_reconnect_invalidates_old_context_and_late_registration() {
    let manager = setup(
        "A",
        "CREATE TABLE users(id INTEGER PRIMARY KEY); INSERT INTO users VALUES(1);",
    )
    .await;
    let old_snapshot = manager.get_connection_snapshot("A").await.unwrap();
    let page = table_result::read_table_page(&manager, request("A", "users"))
        .await
        .unwrap();
    manager.create_connection(config("A")).await.unwrap();
    execute(
        &manager,
        "A",
        "CREATE TABLE users(id INTEGER PRIMARY KEY); INSERT INTO users VALUES(2);",
    )
    .await;
    let current = table_result::read_table_page(&manager, request("A", "users"))
        .await
        .unwrap();
    assert_ne!(
        page.context.connection_generation,
        current.context.connection_generation
    );
    assert_eq!(
        table_result::validate_context(&manager, reference(&page.context, vec![]))
            .await
            .unwrap_err()
            .code,
        "RESULT_CONTEXT_NOT_FOUND"
    );
    let seed = ResultContextSeed {
        revision: 1,
        connection_id: "A".into(),
        connection_generation: old_snapshot.generation.clone(),
        source: page.context.source,
        columns: page.context.columns,
        metadata_fingerprint: "old".into(),
        reason_codes: vec![],
    };
    assert_eq!(
        manager
            .register_result_context(&old_snapshot, seed, vec![])
            .await
            .unwrap_err()
            .code,
        "CONNECTION_CHANGED"
    );
    manager.disconnect("A").await.unwrap();
    assert!(manager.active_connection_snapshot("A").await.is_err());
    // validate 不允许用旧引用触发自动重连。
    assert_eq!(
        table_result::validate_context(&manager, reference(&current.context, vec![]))
            .await
            .unwrap_err()
            .code,
        "RESULT_CONTEXT_NOT_FOUND"
    );
}

#[tokio::test]
async fn sqlite_metadata_change_rejects_and_reclaims_existing_context() {
    let manager = setup(
        "A",
        "CREATE TABLE users(id INTEGER PRIMARY KEY); INSERT INTO users VALUES(1);",
    )
    .await;
    let page = table_result::read_table_page(&manager, request("A", "users"))
        .await
        .unwrap();
    execute(&manager, "A", "ALTER TABLE users ADD COLUMN changed TEXT").await;
    let error = table_result::validate_context(
        &manager,
        reference(&page.context, vec![page.row_handles[0].clone().unwrap()]),
    )
    .await
    .unwrap_err();
    assert_eq!(error.code, "METADATA_CHANGED");
    assert_eq!(
        manager
            .result_contexts
            .stats(chrono::Utc::now().timestamp_millis())
            .unwrap()
            .contexts,
        0
    );
}

#[tokio::test]
async fn sqlite_unprovable_keys_and_views_receive_context_without_row_identity() {
    let manager = setup("A", "CREATE TABLE users(id INTEGER PRIMARY KEY); INSERT INTO users VALUES(1); CREATE VIEW user_view AS SELECT id+1 AS id FROM users; CREATE TABLE no_key(value TEXT); INSERT INTO no_key VALUES('a'); CREATE TABLE text_key(id TEXT PRIMARY KEY); INSERT INTO text_key VALUES('1'); CREATE TABLE nullable_key(id INT, tenant INT, PRIMARY KEY(id,tenant)); INSERT INTO nullable_key VALUES(1,NULL);").await;
    for (table, reason) in [
        ("user_view", "SOURCE_NOT_BASE_TABLE"),
        ("no_key", "NO_PRIMARY_KEY"),
        ("text_key", "ROW_IDENTITY_UNAVAILABLE"),
        ("nullable_key", "ROW_IDENTITY_UNAVAILABLE"),
    ] {
        let page = table_result::read_table_page(&manager, request("A", table))
            .await
            .unwrap();
        assert_eq!(page.row_handles, vec![None], "{table}");
        assert!(
            page.context
                .edit_policy
                .reason_codes
                .iter()
                .any(|code| code == reason),
            "{table}"
        );
        assert!(page.context.edit_policy.allowed_operations.is_empty());
    }
}

#[tokio::test]
async fn sqlite_structured_read_rejects_injection_and_preserves_escaped_table_identity() {
    let manager = setup("A", "CREATE TABLE \"odd\"\".table\"(id INTEGER PRIMARY KEY, name TEXT); INSERT INTO \"odd\"\".table\" VALUES(1,'a'); CREATE TABLE users(id INTEGER PRIMARY KEY);").await;
    let mut req = request("A", "odd\".table");
    req.sort =
        serde_json::from_value(json!({"column":"id); DELETE FROM users; --","direction":"desc"}))
            .ok();
    assert_eq!(
        table_result::read_table_page(&manager, req)
            .await
            .unwrap_err()
            .code,
        "INVALID_SORT"
    );
    let mut req = request("A", "odd\".table");
    req.sort = serde_json::from_value(json!({"column":"ID","direction":"desc"})).ok();
    let page = table_result::read_table_page(&manager, req).await.unwrap();
    assert_eq!(page.context.source.table, "odd\".table");
    assert_eq!(page.result.rows[0].values[0], Some(json!(1)));
    let mut req = request("A", "users");
    req.protocol_version = 2;
    assert_eq!(
        table_result::read_table_page(&manager, req)
            .await
            .unwrap_err()
            .code,
        "UNSUPPORTED_PROTOCOL"
    );
    let mut req = request("A", "users");
    req.page = 0;
    assert_eq!(
        table_result::read_table_page(&manager, req)
            .await
            .unwrap_err()
            .code,
        "INVALID_PAGE"
    );
    let mut req = request("A", "users");
    req.database = "伪造文件.db".into();
    assert!(table_result::read_table_page(&manager, req).await.is_err());
    table_result::release_context(
        &manager,
        ContextReference {
            protocol_version: 1,
            result_id: page.context.result_id.clone(),
            revision: 1,
        },
    )
    .unwrap();
    table_result::release_context(
        &manager,
        ContextReference {
            protocol_version: 1,
            result_id: page.context.result_id,
            revision: 1,
        },
    )
    .unwrap();
    assert_eq!(
        manager
            .result_contexts
            .stats(chrono::Utc::now().timestamp_millis())
            .unwrap()
            .contexts,
        0
    );
    // 普通手工读取不进入结果注册表，不能因列名相同获得句柄。
    let raw = manager
        .get_connection("A")
        .await
        .unwrap()
        .query("SELECT id+1 AS id FROM users", None, None)
        .await
        .unwrap();
    assert_eq!(serde_json::to_value(raw).unwrap().get("context"), None);
}

#[tokio::test]
async fn sqlite_controlled_projection_preserves_generated_columns_and_source_schema() {
    let manager = setup("A", "CREATE TABLE users(id INTEGER PRIMARY KEY,a INT,b INT GENERATED ALWAYS AS(a*2) VIRTUAL,c INT GENERATED ALWAYS AS(a*3) STORED); INSERT INTO users(id,a) VALUES(1,5); CREATE TEMP TABLE users(id INTEGER PRIMARY KEY, wrong TEXT); INSERT INTO temp.users VALUES(9,'错误来源');").await;
    // 表浏览请求 main，不能被 temp 同名对象遮蔽。
    let page = table_result::read_table_page(&manager, request("A", "users"))
        .await
        .unwrap();
    assert_eq!(page.context.source.schema.as_deref(), Some("main"));
    assert_eq!(
        page.result
            .columns
            .iter()
            .map(|column| column.name.as_str())
            .collect::<Vec<_>>(),
        vec!["id", "a", "b", "c"]
    );
    assert_eq!(
        page.result.rows[0].values,
        vec![
            Some(json!(1)),
            Some(json!(5)),
            Some(json!(10)),
            Some(json!(15))
        ]
    );
    assert!(!page.context.columns[0].generated);
    assert!(page.context.columns[2].generated && page.context.columns[3].generated);
    assert!(page.context.columns.iter().all(|column| !column.writable));
    assert!(page.row_handles[0].is_some());
}
