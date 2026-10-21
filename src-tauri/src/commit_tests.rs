//! 真实 SQLite 贯穿受控读取、原值核对、事务提交和独立回执登记。

use super::*;
use crate::changeset::{CellInput, ChangeOperation, ChangeSetRequest};
use crate::commit_receipt::{CommitReceipt, CommitStatus, OperationOutcome};
use crate::database::pool::ConnectionPoolManager;
use crate::models::ConnectionConfig;
use crate::result_context::ContextError;
use crate::table_result::{self, TablePage, TablePageRequest};
use crate::task_registry::{CleanupState, CommitState};
use crate::typed_value::TypedValue;
use std::collections::BTreeMap;
use uuid::Uuid;

const CONNECTION: &str = "受控提交实库";
const CREATE_TABLE: &str = "CREATE TABLE entries(id INTEGER PRIMARY KEY NOT NULL, payload TEXT NOT NULL UNIQUE CHECK(payload <> '禁止'), nullable_text TEXT, bytes BLOB, big_number INTEGER)";

fn config() -> ConnectionConfig {
    serde_json::from_value(serde_json::json!({
        "id": CONNECTION, "name": "SQLite 安全提交测试", "driverType": "sqlite",
        "host": "", "port": 0, "user": "", "password": "", "database": ":memory:"
    }))
    .unwrap()
}

async fn initialize(manager: &ConnectionPoolManager) {
    let driver = manager.get_connection(CONNECTION).await.unwrap();
    // 安全写入夹具明确启用外键；读取身份后不再更改连接设置。
    driver
        .execute_bound("PRAGMA foreign_keys=ON", &[], None)
        .await
        .unwrap();
    driver.execute_bound(CREATE_TABLE, &[], None).await.unwrap();
    for (id, text) in [
        ("1", "第一行"),
        ("2", "第二行"),
        ("9007199254740993", "大整数行"),
    ] {
        driver
            .execute_bound(
                "INSERT INTO entries(id,payload,nullable_text,bytes,big_number) VALUES(?,?,?,?,?)",
                &[
                    TypedValue::Integer(id.into()),
                    TypedValue::Text(text.into()),
                    TypedValue::Text("初值".into()),
                    TypedValue::binary(&[0, 255, 0]),
                    TypedValue::Integer("0".into()),
                ],
                None,
            )
            .await
            .unwrap();
    }
}

async fn fixture() -> (ConnectionPoolManager, TablePage) {
    let manager = ConnectionPoolManager::new(None);
    manager.create_connection(config()).await.unwrap();
    initialize(&manager).await;
    let page = read(&manager).await;
    (manager, page)
}

async fn read(manager: &ConnectionPoolManager) -> TablePage {
    table_result::read_table_page(
        manager,
        TablePageRequest {
            protocol_version: 1,
            connection_id: CONNECTION.into(),
            database: "main".into(),
            table_parts: vec!["entries".into()],
            page: 1,
            page_size: 20,
            sort: None,
        },
    )
    .await
    .unwrap()
}

async fn stored(manager: &ConnectionPoolManager) -> Vec<Vec<Option<TypedValue>>> {
    manager
        .get_connection(CONNECTION)
        .await
        .unwrap()
        .query_typed(
            "SELECT id,payload,nullable_text,bytes,big_number FROM main.entries ORDER BY id",
            &[],
            None,
        )
        .await
        .unwrap()
        .typed_rows
}

fn row_index(page: &TablePage, key: &str) -> usize {
    page.typed_rows
        .iter()
        .position(|row| row[0] == Some(TypedValue::Integer(key.into())))
        .expect("夹具行应存在")
}

fn change(page: &TablePage, entries: &[(&str, TypedValue)]) -> BTreeMap<String, CellInput> {
    entries
        .iter()
        .map(|(name, value)| {
            let column = page
                .context
                .columns
                .iter()
                .find(|column| column.display_name == *name)
                .unwrap();
            (
                column.column_id.clone(),
                CellInput::Set {
                    value: value.clone(),
                },
            )
        })
        .collect()
}

fn update(page: &TablePage, key: &str, entries: &[(&str, TypedValue)]) -> ChangeOperation {
    ChangeOperation::Update {
        op_id: Uuid::new_v4().to_string(),
        row_handle: page.row_handles[row_index(page, key)]
            .clone()
            .expect("夹具行应有可信句柄"),
        changes: change(page, entries),
    }
}

fn request(page: &TablePage, operations: Vec<ChangeOperation>) -> ChangeSetRequest {
    ChangeSetRequest {
        protocol_version: 1,
        change_set_id: Uuid::new_v4().to_string(),
        result_id: page.context.result_id.clone(),
        revision: page.context.revision,
        operations,
    }
}

fn one_update(page: &TablePage, key: &str, value: &str) -> ChangeSetRequest {
    request(
        page,
        vec![update(
            page,
            key,
            &[("payload", TypedValue::Text(value.into()))],
        )],
    )
}

fn assert_rejected(result: Result<CommitReceipt, ContextError>) -> String {
    match result {
        Err(error) => error.code,
        Ok(receipt) => {
            assert!(
                matches!(
                    receipt.status,
                    CommitStatus::FailedBeforeCommit
                        | CommitStatus::Conflict
                        | CommitStatus::RolledBack
                ),
                "{:?}",
                receipt
            );
            assert_ne!(receipt.commit_state, CommitState::Committed);
            assert!(
                !receipt
                    .operation_results
                    .iter()
                    .any(|result| result.outcome == OperationOutcome::Applied),
                "失败回执不能报告持久化写入：{:?}",
                receipt
            );
            receipt
                .error_code
                .unwrap_or_else(|| receipt.reason_codes.join(","))
        }
    }
}

#[tokio::test]
async fn exact_bigint_null_text_and_binary_survive_commit_and_database_readback() {
    let (manager, page) = fixture().await;
    let changes = [
        ("payload", TypedValue::Text("NULL".into())),
        ("nullable_text", TypedValue::Null),
        ("bytes", TypedValue::binary(&[0, 255, 0, 128])),
        ("big_number", TypedValue::Integer("9007199254740993".into())),
    ];
    let req = request(&page, vec![update(&page, "9007199254740993", &changes)]);
    let receipt = commit_internal(&manager, req, None, true).await.unwrap();
    assert_eq!(receipt.status, CommitStatus::Committed);
    assert_eq!(receipt.commit_state, CommitState::Committed);
    assert_eq!(receipt.cleanup_state, CleanupState::Succeeded);
    assert_eq!(
        receipt.operation_results[0].outcome,
        OperationOutcome::Applied
    );
    assert_eq!(receipt.operation_results[0].matched_rows, Some(1));
    assert_eq!(receipt.operation_results[0].affected_rows, Some(1));
    assert_eq!(
        stored(&manager).await[2],
        vec![
            Some(TypedValue::Integer("9007199254740993".into())),
            Some(TypedValue::Text("NULL".into())),
            Some(TypedValue::Null),
            Some(TypedValue::binary(&[0, 255, 0, 128])),
            Some(TypedValue::Integer("9007199254740993".into())),
        ]
    );
}

#[tokio::test]
async fn externally_changed_original_value_reports_conflict_without_overwrite() {
    let (manager, page) = fixture().await;
    manager
        .get_connection(CONNECTION)
        .await
        .unwrap()
        .execute_bound(
            "UPDATE entries SET nullable_text=? WHERE id=1",
            &[TypedValue::Text("外部已修改".into())],
            None,
        )
        .await
        .unwrap();
    let before = stored(&manager).await;
    let receipt = commit_internal(&manager, one_update(&page, "1", "本次修改"), None, true)
        .await
        .unwrap();
    assert_eq!(receipt.status, CommitStatus::Conflict);
    assert_eq!(receipt.error_code.as_deref(), Some("ROW_CHANGED"));
    assert_eq!(
        receipt.operation_results[0].outcome,
        OperationOutcome::Conflict
    );
    assert_eq!(receipt.operation_results[0].matched_rows, Some(1));
    assert_eq!(stored(&manager).await, before);
}

#[tokio::test]
async fn externally_deleted_row_reports_zero_match_conflict() {
    let (manager, page) = fixture().await;
    manager
        .get_connection(CONNECTION)
        .await
        .unwrap()
        .execute_bound("DELETE FROM entries WHERE id=1", &[], None)
        .await
        .unwrap();
    let before = stored(&manager).await;
    let receipt = commit_internal(&manager, one_update(&page, "1", "不得重新插入"), None, true)
        .await
        .unwrap();
    assert_eq!(receipt.status, CommitStatus::Conflict);
    assert_eq!(receipt.error_code.as_deref(), Some("ROW_DELETED"));
    assert_eq!(
        receipt.operation_results[0].outcome,
        OperationOutcome::Conflict
    );
    assert_eq!(receipt.operation_results[0].matched_rows, Some(0));
    assert_eq!(stored(&manager).await, before);
}

#[tokio::test]
async fn unchanged_update_reports_matched_one_affected_zero() {
    let (manager, page) = fixture().await;
    let before = stored(&manager).await;
    let receipt = commit_internal(&manager, one_update(&page, "1", "第一行"), None, true)
        .await
        .unwrap();
    assert_eq!(receipt.status, CommitStatus::Committed);
    assert_eq!(
        receipt.operation_results[0].outcome,
        OperationOutcome::Unchanged
    );
    assert_eq!(receipt.operation_results[0].matched_rows, Some(1));
    assert_eq!(receipt.operation_results[0].affected_rows, Some(0));
    assert_eq!(stored(&manager).await, before);
}

#[tokio::test]
async fn later_constraint_failure_rolls_back_the_entire_changeset() {
    let (manager, page) = fixture().await;
    let before = stored(&manager).await;
    let req = request(
        &page,
        vec![
            update(
                &page,
                "1",
                &[("payload", TypedValue::Text("先改成功".into()))],
            ),
            update(&page, "2", &[("payload", TypedValue::Text("禁止".into()))]),
        ],
    );
    let receipt = commit_internal(&manager, req, None, true).await.unwrap();
    assert_eq!(receipt.status, CommitStatus::RolledBack);
    assert_eq!(receipt.error_code.as_deref(), Some("OPERATION_FAILED"));
    assert_eq!(receipt.commit_state, CommitState::RolledBack);
    assert_eq!(receipt.rollback_state, CleanupState::Succeeded);
    assert_eq!(
        receipt.operation_results[0].outcome,
        OperationOutcome::RolledBack
    );
    assert_eq!(stored(&manager).await, before);
}

#[tokio::test]
async fn later_row_conflict_prevents_all_writes_before_any_operation_is_executed() {
    let (manager, page) = fixture().await;
    manager
        .get_connection(CONNECTION)
        .await
        .unwrap()
        .execute_bound("DELETE FROM entries WHERE id=2", &[], None)
        .await
        .unwrap();
    let before = stored(&manager).await;
    let req = request(
        &page,
        vec![
            update(
                &page,
                "1",
                &[("payload", TypedValue::Text("先改成功".into()))],
            ),
            update(
                &page,
                "2",
                &[("payload", TypedValue::Text("随后冲突".into()))],
            ),
        ],
    );
    let receipt = commit_internal(&manager, req, None, true).await.unwrap();
    assert_eq!(receipt.status, CommitStatus::Conflict);
    assert_eq!(receipt.commit_state, CommitState::RolledBack);
    assert_eq!(receipt.rollback_state, CleanupState::Succeeded);
    assert_eq!(
        receipt.operation_results[0].outcome,
        OperationOutcome::NotExecuted
    );
    assert_eq!(
        receipt.operation_results[1].outcome,
        OperationOutcome::Conflict
    );
    assert_eq!(stored(&manager).await, before);
}

#[tokio::test]
async fn identical_retry_returns_original_receipt_and_changed_content_cannot_reuse_id() {
    let (manager, page) = fixture().await;
    let req = request(
        &page,
        vec![ChangeOperation::Insert {
            op_id: Uuid::new_v4().to_string(),
            changes: change(
                &page,
                &[
                    ("id", TypedValue::Integer("3".into())),
                    ("payload", TypedValue::Text("仅插一次".into())),
                ],
            ),
        }],
    );
    let original = commit_internal(&manager, req.clone(), None, true)
        .await
        .unwrap();
    assert_eq!(original.status, CommitStatus::Committed);
    let after = stored(&manager).await;
    let repeated = commit_internal(&manager, req.clone(), None, true)
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(original).unwrap(),
        serde_json::to_value(repeated).unwrap()
    );
    assert_eq!(stored(&manager).await, after);
    let mut altered = req;
    if let ChangeOperation::Insert { changes, .. } = &mut altered.operations[0] {
        changes.insert(
            page.context.columns[1].column_id.clone(),
            CellInput::Set {
                value: TypedValue::Text("不同内容".into()),
            },
        );
    }
    assert_eq!(
        commit_internal(&manager, altered, None, true)
            .await
            .unwrap_err()
            .code,
        "CHANGESET_ID_REUSED"
    );
    assert_eq!(stored(&manager).await, after);
}

#[tokio::test]
async fn receipts_remain_queryable_after_context_release_and_old_handles_stay_invalid() {
    let (manager, page) = fixture().await;
    let req = one_update(&page, "1", "已保存");
    let receipt = commit_internal(&manager, req.clone(), None, true)
        .await
        .unwrap();
    assert_eq!(receipt.status, CommitStatus::Committed);
    assert!(
        manager
            .result_contexts
            .get_context(
                &req.result_id,
                req.revision,
                chrono::Utc::now().timestamp_millis()
            )
            .is_err(),
        "确认保存后必须失效旧结果及行句柄"
    );
    manager
        .result_contexts
        .release(
            &req.result_id,
            req.revision,
            chrono::Utc::now().timestamp_millis(),
        )
        .unwrap();
    let recovered = manager
        .commit_receipts
        .get(&req.change_set_id, &req.result_id, req.revision)
        .unwrap();
    assert_eq!(
        serde_json::to_value(receipt).unwrap(),
        serde_json::to_value(recovered).unwrap()
    );
    let after = stored(&manager).await;
    let mut another = req;
    another.change_set_id = Uuid::new_v4().to_string();
    assert_rejected(commit_internal(&manager, another, None, true).await);
    assert_eq!(stored(&manager).await, after);
}

#[tokio::test]
async fn same_name_same_columns_recreation_cannot_accept_an_old_result_identity() {
    let (manager, page) = fixture().await;
    let driver = manager.get_connection(CONNECTION).await.unwrap();
    driver
        .execute_bound("DROP TABLE entries", &[], None)
        .await
        .unwrap();
    driver.execute_bound(CREATE_TABLE, &[], None).await.unwrap();
    driver
        .execute_bound(
            "INSERT INTO entries VALUES(1,'第一行','初值',x'00ff00',0)",
            &[],
            None,
        )
        .await
        .unwrap();
    let before = stored(&manager).await;
    let code = assert_rejected(
        commit_internal(
            &manager,
            one_update(&page, "1", "不得写入新对象"),
            None,
            true,
        )
        .await,
    );
    assert_eq!(code, "OBJECT_IDENTITY_CHANGED");
    assert_eq!(stored(&manager).await, before);
}

#[tokio::test]
async fn reconnecting_the_logical_source_rejects_previous_generation_without_writes() {
    let (manager, page) = fixture().await;
    manager.create_connection(config()).await.unwrap();
    initialize(&manager).await;
    let before = stored(&manager).await;
    assert_rejected(
        commit_internal(&manager, one_update(&page, "1", "旧连接提交"), None, true).await,
    );
    assert_eq!(stored(&manager).await, before);
}

#[tokio::test]
async fn fabricated_handles_and_unknown_columns_cannot_write() {
    let (manager, page) = fixture().await;
    let before = stored(&manager).await;
    let mut invalid_row = one_update(&page, "1", "伪造行");
    if let ChangeOperation::Update { row_handle, .. } = &mut invalid_row.operations[0] {
        *row_handle = Uuid::new_v4().to_string();
    }
    assert_eq!(
        assert_rejected(commit_internal(&manager, invalid_row, None, true).await),
        "INVALID_ROW_HANDLE"
    );
    let mut invalid_column = one_update(&page, "1", "未知列");
    if let ChangeOperation::Update { changes, .. } = &mut invalid_column.operations[0] {
        changes.clear();
        changes.insert(
            Uuid::new_v4().to_string(),
            CellInput::Set {
                value: TypedValue::Text("未知列".into()),
            },
        );
    }
    assert_eq!(
        assert_rejected(commit_internal(&manager, invalid_column, None, true).await),
        "INVALID_COLUMN"
    );
    assert_eq!(stored(&manager).await, before);
}

#[tokio::test]
async fn undecodable_original_snapshot_cannot_be_silently_overwritten() {
    let (manager, _) = fixture().await;
    manager
        .get_connection(CONNECTION)
        .await
        .unwrap()
        .execute_bound(
            "UPDATE entries SET nullable_text=CAST(x'80' AS TEXT) WHERE id=1",
            &[],
            None,
        )
        .await
        .unwrap();
    let page = read(&manager).await;
    assert!(page.typed_rows[row_index(&page, "1")][2].is_none());
    let before = manager
        .get_connection(CONNECTION)
        .await
        .unwrap()
        .query(
            "SELECT hex(nullable_text) FROM entries WHERE id=1",
            None,
            None,
        )
        .await
        .unwrap()
        .rows[0]
        .values[0]
        .clone();
    let code = assert_rejected(
        commit_internal(
            &manager,
            one_update(&page, "1", "不能跳过原快照"),
            None,
            true,
        )
        .await,
    );
    assert_eq!(code, "ORIGINAL_VALUE_UNSUPPORTED");
    let after = manager
        .get_connection(CONNECTION)
        .await
        .unwrap()
        .query(
            "SELECT hex(nullable_text) FROM entries WHERE id=1",
            None,
            None,
        )
        .await
        .unwrap()
        .rows[0]
        .values[0]
        .clone();
    assert_eq!(before, after);
    assert_eq!(
        stored(&manager).await[0][1],
        Some(TypedValue::Text("第一行".into()))
    );
}

#[tokio::test]
async fn missing_credential_protection_stays_closed_after_internal_success_and_rejects_ipc_override(
) {
    let (manager, page) = fixture().await;
    let receipt = commit_internal(
        &manager,
        one_update(&page, "1", "内部夹具已保存"),
        None,
        true,
    )
    .await
    .unwrap();
    assert_eq!(receipt.status, CommitStatus::Committed);
    let page = read(&manager).await;
    let before = stored(&manager).await;
    let req = one_update(&page, "2", "生产入口不得写入");
    let mut forged = serde_json::to_value(&req).unwrap();
    forged
        .as_object_mut()
        .unwrap()
        .insert("permitted".into(), serde_json::Value::Bool(true));
    assert!(
        serde_json::from_value::<ChangeSetRequest>(forged).is_err(),
        "IPC载荷不得新增授权开关"
    );
    let receipt = commit_changeset(&manager, req, None).await.unwrap();
    assert_eq!(receipt.status, CommitStatus::FailedBeforeCommit);
    assert_eq!(receipt.commit_state, CommitState::NotStarted);
    assert_eq!(
        receipt.error_code.as_deref(),
        Some("CREDENTIAL_RECOVERY_REQUIRED")
    );
    assert!(receipt
        .reason_codes
        .iter()
        .any(|code| code == "CREDENTIAL_RECOVERY_REQUIRED"));
    assert!(receipt
        .operation_results
        .iter()
        .all(|operation| operation.outcome == OperationOutcome::NotExecuted));
    assert_eq!(stored(&manager).await, before);
    let second = commit_changeset(&manager, one_update(&page, "2", "再次验证仍封闭"), None)
        .await
        .unwrap();
    assert_eq!(
        second.error_code.as_deref(),
        Some("CREDENTIAL_RECOVERY_REQUIRED")
    );
    assert_eq!(stored(&manager).await, before);
}

#[tokio::test]
async fn public_permission_requires_backend_operation_column_and_input_kind() {
    let (manager, page) = fixture().await;
    let request = one_update(&page, "1", "类型化输入");
    let mut context = page.context.clone();
    assert_eq!(
        validate_public_permission(&request, &context),
        Err("DRIVER_WRITE_VALIDATION_REQUIRED")
    );
    context.edit_policy.allowed_operations = vec!["update".into()];
    assert_eq!(
        validate_public_permission(&request, &context),
        Err("COLUMN_NOT_WRITABLE")
    );
    let column = context
        .columns
        .iter_mut()
        .find(|column| column.display_name == "payload")
        .unwrap();
    column.writable = true;
    let id = column.column_id.clone();
    context
        .edit_policy
        .column_input_kinds
        .insert(id.clone(), "integer".into());
    assert_eq!(
        validate_public_permission(&request, &context),
        Err("GRID_VALUE_TYPE_NOT_VALIDATED")
    );
    context
        .edit_policy
        .column_input_kinds
        .insert(id, "text".into());
    assert_eq!(validate_public_permission(&request, &context), Ok(()));
    context
        .columns
        .iter_mut()
        .find(|column| column.display_name == "payload")
        .unwrap()
        .generated = true;
    assert_eq!(
        validate_public_permission(&request, &context),
        Err("COLUMN_NOT_WRITABLE")
    );
    assert_eq!(stored(&manager).await.len(), 3);
}

#[tokio::test]
async fn unsafe_sqlite_target_keeps_a_real_snapshot_but_refuses_dml() {
    let (manager, _) = fixture().await;
    manager
        .get_connection(CONNECTION)
        .await
        .unwrap()
        .execute_bound("PRAGMA foreign_keys=OFF", &[], None)
        .await
        .unwrap();
    let page = read(&manager).await;
    let before = stored(&manager).await;
    let receipt = commit_internal(
        &manager,
        one_update(&page, "1", "不安全设置不得写入"),
        None,
        true,
    )
    .await
    .unwrap();
    assert_eq!(receipt.status, CommitStatus::FailedBeforeCommit);
    assert_eq!(
        receipt.error_code.as_deref(),
        Some("TARGET_WRITE_UNSUPPORTED")
    );
    assert!(receipt
        .reason_codes
        .iter()
        .any(|reason| reason.contains("foreign_keys")));
    assert_eq!(stored(&manager).await, before);
}

async fn sqlite_schema_fixture(definition: &str) -> (ConnectionPoolManager, TablePage) {
    let manager = ConnectionPoolManager::new(None);
    manager.create_connection(config()).await.unwrap();
    let driver = manager.get_connection(CONNECTION).await.unwrap();
    driver
        .execute_bound("PRAGMA foreign_keys=ON", &[], None)
        .await
        .unwrap();
    driver.execute_bound(definition, &[], None).await.unwrap();
    for (id, payload) in [("1", "第一行"), ("2", "第二行")] {
        driver
            .execute_bound(
                "INSERT INTO entries(id,payload) VALUES(?,?)",
                &[
                    TypedValue::Integer(id.into()),
                    TypedValue::Text(payload.into()),
                ],
                None,
            )
            .await
            .unwrap();
    }
    let page = read(&manager).await;
    (manager, page)
}

#[tokio::test]
async fn sqlite_constraint_replace_and_ignore_cannot_delete_or_silently_skip_grid_writes() {
    for policy in ["REPLACE", "IGNORE"] {
        let definition = format!(
            "CREATE TABLE entries(id INTEGER PRIMARY KEY NOT NULL DEFAULT 3, payload TEXT NOT NULL UNIQUE ON CONFLICT {policy} DEFAULT '第二行', nullable_text TEXT, bytes BLOB, big_number INTEGER)"
        );
        let (manager, _) = sqlite_schema_fixture(&definition).await;
        let before = stored(&manager).await;
        // 普通插入、更新及省略全部字段的默认插入都必须覆盖约束策略。
        for operation_kind in ["update", "insert", "defaultInsert"] {
            let page = read(&manager).await;
            let operation = match operation_kind {
                "update" => update(
                    &page,
                    "1",
                    &[("payload", TypedValue::Text("第二行".into()))],
                ),
                "insert" => ChangeOperation::Insert {
                    op_id: Uuid::new_v4().to_string(),
                    changes: change(
                        &page,
                        &[
                            ("id", TypedValue::Integer("3".into())),
                            ("payload", TypedValue::Text("第二行".into())),
                        ],
                    ),
                },
                _ => ChangeOperation::Insert {
                    op_id: Uuid::new_v4().to_string(),
                    changes: BTreeMap::new(),
                },
            };
            let receipt = commit_internal(&manager, request(&page, vec![operation]), None, true)
                .await
                .unwrap();
            assert_eq!(
                receipt.status,
                CommitStatus::RolledBack,
                "{policy}/{operation_kind}: {receipt:?}"
            );
            assert_eq!(receipt.error_code.as_deref(), Some("OPERATION_FAILED"));
            assert_eq!(
                receipt.operation_results[0].outcome,
                OperationOutcome::RolledBack
            );
            assert_eq!(
                stored(&manager).await,
                before,
                "{policy}/{operation_kind} 不得改变原行或删除未选中行"
            );
        }
    }
}

#[tokio::test]
async fn sqlite_affinity_precedence_rejects_values_that_would_change_storage_type() {
    for declared in ["TEXTINT", "VARCHARINT", "FLOATING POINT", "DOUBLE INT"] {
        let definition = format!(
            "CREATE TABLE entries(id INTEGER PRIMARY KEY NOT NULL, payload {declared}, nullable_text TEXT, bytes BLOB, big_number INTEGER)"
        );
        let (manager, _) = sqlite_schema_fixture(&definition).await;
        let before = stored(&manager).await;
        for value in [TypedValue::Text("001".into()), TypedValue::Float(1.0)] {
            let page = read(&manager).await;
            let operation = update(&page, "1", &[("payload", value)]);
            let receipt = commit_internal(&manager, request(&page, vec![operation]), None, true)
                .await
                .unwrap();
            assert_eq!(
                receipt.status,
                CommitStatus::FailedBeforeCommit,
                "{declared}: {receipt:?}"
            );
            assert_eq!(
                receipt.error_code.as_deref(),
                Some("COLUMN_TYPE_UNSUPPORTED")
            );
            assert_eq!(
                receipt.operation_results[0].outcome,
                OperationOutcome::NotExecuted
            );
            assert_eq!(stored(&manager).await, before);
        }
        // 这些声明实际为 INTEGER affinity，仍允许精确的 64 位整数。
        let page = read(&manager).await;
        let operation = update(
            &page,
            "1",
            &[("payload", TypedValue::Integer("9007199254740993".into()))],
        );
        let receipt = commit_internal(&manager, request(&page, vec![operation]), None, true)
            .await
            .unwrap();
        assert_eq!(receipt.status, CommitStatus::Committed);
        assert_eq!(
            stored(&manager).await[0][1],
            Some(TypedValue::Integer("9007199254740993".into()))
        );
    }
}

#[tokio::test]
async fn sqlite_supported_affinities_keep_exact_integer_text_real_binary_and_null() {
    let (manager, page) = sqlite_schema_fixture(
        "CREATE TABLE entries(id INTEGER PRIMARY KEY NOT NULL, payload TEXT, nullable_text REAL, bytes BLOB, big_number INTEGER)"
    ).await;
    let operation = update(
        &page,
        "1",
        &[
            ("payload", TypedValue::Text("001".into())),
            ("nullable_text", TypedValue::Float(1.0)),
            ("bytes", TypedValue::binary(&[0, 255, 128])),
            ("big_number", TypedValue::Integer(i64::MAX.to_string())),
        ],
    );
    let receipt = commit_internal(&manager, request(&page, vec![operation]), None, true)
        .await
        .unwrap();
    assert_eq!(receipt.status, CommitStatus::Committed);
    assert_eq!(
        stored(&manager).await[0],
        vec![
            Some(TypedValue::Integer("1".into())),
            Some(TypedValue::Text("001".into())),
            Some(TypedValue::Float(1.0)),
            Some(TypedValue::binary(&[0, 255, 128])),
            Some(TypedValue::Integer(i64::MAX.to_string())),
        ]
    );
    let page = read(&manager).await;
    let operation = update(&page, "1", &[("nullable_text", TypedValue::Null)]);
    let receipt = commit_internal(&manager, request(&page, vec![operation]), None, true)
        .await
        .unwrap();
    assert_eq!(receipt.status, CommitStatus::Committed);
    assert_eq!(stored(&manager).await[0][2], Some(TypedValue::Null));
}
