//! 真实 SQLite 贯穿受控读取、类型快照和变更预检；预检成功也必须零写入。
use serde_json::json;
use sqlconnect_lib::changeset::{self, ChangeSetRequest};
use sqlconnect_lib::database::pool::ConnectionPoolManager;
use sqlconnect_lib::models::ConnectionConfig;
use sqlconnect_lib::table_result::{self, TablePage, TablePageRequest};
use sqlconnect_lib::typed_value::TypedValue;
use uuid::Uuid;

async fn fixture() -> (ConnectionPoolManager, TablePage) {
    let manager = ConnectionPoolManager::new(None);
    let config: ConnectionConfig = serde_json::from_value(json!({"id":"A","name":"变更预检实库","driverType":"sqlite","host":"","port":0,"user":"","password":"","database":":memory:"})).unwrap();
    manager.create_connection(config).await.unwrap();
    let driver = manager.get_connection("A").await.unwrap();
    driver.execute("CREATE TABLE entries(id INTEGER PRIMARY KEY NOT NULL, payload TEXT, bytes BLOB, computed TEXT GENERATED ALWAYS AS (payload || '!') STORED)",None,None).await.unwrap();
    driver
        .execute_bound(
            "INSERT INTO entries(id,payload,bytes) VALUES(?,?,?)",
            &[
                TypedValue::Integer("9007199254740993".into()),
                TypedValue::Text("原值".into()),
                TypedValue::binary(&[0, 255, 0]),
            ],
            None,
        )
        .await
        .unwrap();
    let request: TablePageRequest = serde_json::from_value(json!({"protocolVersion":1,"connectionId":"A","database":"main","tableParts":["entries"],"page":1,"pageSize":20})).unwrap();
    let page = table_result::read_table_page(&manager, request)
        .await
        .unwrap();
    (manager, page)
}

fn request(page: &TablePage, value: serde_json::Value) -> ChangeSetRequest {
    serde_json::from_value(json!({"protocolVersion":1,"changeSetId":Uuid::new_v4().to_string(),"resultId":page.context.result_id,"revision":page.context.revision,"operations":[{
        "opId":Uuid::new_v4().to_string(),"kind":"update","rowHandle":page.row_handles[0],"changes":{page.context.columns[1].column_id.clone():{"mode":"set","value":value}}
    }]})).unwrap()
}

async fn assert_original(manager: &ConnectionPoolManager) {
    let result = manager
        .get_connection("A")
        .await
        .unwrap()
        .query_typed("SELECT payload, bytes FROM entries", &[], None)
        .await
        .unwrap();
    assert_eq!(
        result.typed_rows,
        vec![vec![
            Some(TypedValue::Text("原值".into())),
            Some(TypedValue::binary(&[0, 255, 0]))
        ]]
    );
}

#[tokio::test]
async fn typed_context_and_preview_are_exact_and_never_execute_dml() {
    let (manager, mut page) = fixture().await;
    assert_eq!(
        page.typed_rows[0][0],
        Some(TypedValue::Integer("9007199254740993".into()))
    );
    assert_eq!(
        page.typed_rows[0][2],
        Some(TypedValue::binary(&[0, 255, 0]))
    );
    assert!(page.context.columns[3].generated);
    let handle = page.row_handles[0].clone().unwrap();
    // 不可信显示数据与公开类型载荷都不能替换后端行定位和原快照。
    page.result.rows[0].values[0] = Some(json!(2));
    page.typed_rows[0][0] = Some(TypedValue::Integer("2".into()));
    let identities = manager
        .validate_result_handles(&page.context, &[handle])
        .await
        .unwrap();
    assert_eq!(
        identities[0].keys[0].1,
        TypedValue::Integer("9007199254740993".into())
    );
    assert_eq!(
        identities[0].original_values[0],
        Some(TypedValue::Integer("9007199254740993".into()))
    );
    for value in [
        json!({"kind":"text","value":"NULL"}),
        json!({"kind":"text","value":""}),
        json!({"kind":"null"}),
        json!({"kind":"text","value":"中文'\\; DELETE FROM entries; --"}),
    ] {
        let preview = changeset::validate_changeset(&manager, request(&page, value))
            .await
            .unwrap();
        assert!(!preview.executable);
        assert_eq!(preview.operation_count, 1);
        assert!(preview
            .reason_codes
            .contains(&"SESSION_LEASE_REQUIRED".into()));
        let public = serde_json::to_value(preview).unwrap();
        for private in [
            "sql",
            "params",
            "keys",
            "originalValues",
            "connectionId",
            "source",
        ] {
            assert!(public.get(private).is_none());
        }
        assert_original(&manager).await;
    }
    assert!(page.context.edit_policy.allowed_operations.is_empty());
}

#[tokio::test]
async fn generated_unknown_fields_and_failed_whole_plan_remain_read_only() {
    let (manager, page) = fixture().await;
    for (id, expected) in [
        (
            page.context.columns[3].column_id.clone(),
            "COLUMN_READ_ONLY",
        ),
        (Uuid::new_v4().to_string(), "INVALID_COLUMN"),
    ] {
        let mut request = request(&page, json!({"kind":"text","value":"改动"}));
        if let changeset::ChangeOperation::Update { changes, .. } = &mut request.operations[0] {
            let value = changes.pop_first().unwrap().1;
            changes.insert(id, value);
        }
        assert_eq!(
            changeset::validate_changeset(&manager, request)
                .await
                .unwrap_err()
                .code,
            expected
        );
        assert_original(&manager).await;
    }
    let mut request = request(&page, json!({"kind":"text","value":"改动"}));
    request.operations.push(serde_json::from_value(json!({"kind":"insert","opId":Uuid::new_v4().to_string(),"changes":{page.context.columns[0].column_id.clone():{"mode":"set","value":{"kind":"float","value":9007199254740992.0}}}})).unwrap());
    assert_eq!(
        changeset::validate_changeset(&manager, request)
            .await
            .unwrap_err()
            .code,
        "COLUMN_TYPE_UNSUPPORTED"
    );
    assert_original(&manager).await;
}

#[tokio::test]
async fn preview_rejects_forged_stale_and_changed_contexts() {
    let (manager, page) = fixture().await;
    let mut forged = request(&page, json!({"kind":"text","value":"改动"}));
    if let changeset::ChangeOperation::Update { row_handle, .. } = &mut forged.operations[0] {
        *row_handle = Uuid::new_v4().to_string();
    }
    assert_eq!(
        changeset::validate_changeset(&manager, forged)
            .await
            .unwrap_err()
            .code,
        "INVALID_ROW_HANDLE"
    );
    let mut stale = request(&page, json!({"kind":"text","value":"改动"}));
    stale.revision += 1;
    assert_eq!(
        changeset::validate_changeset(&manager, stale)
            .await
            .unwrap_err()
            .code,
        "STALE_REVISION"
    );
    manager
        .get_connection("A")
        .await
        .unwrap()
        .execute("ALTER TABLE entries ADD COLUMN extra TEXT", None, None)
        .await
        .unwrap();
    assert_eq!(
        changeset::validate_changeset(
            &manager,
            request(&page, json!({"kind":"text","value":"改动"}))
        )
        .await
        .unwrap_err()
        .code,
        "METADATA_CHANGED"
    );
    assert_original(&manager).await;
}
