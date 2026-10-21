//! 真实 SQLite：首屏/统计分离、可信续页、预算与任务清理。
use serde_json::json;
use sqlconnect_lib::database::pool::ConnectionPoolManager;
use sqlconnect_lib::dataset::{self, DatasetPageRequest, TableCountRequest};
use sqlconnect_lib::models::ConnectionConfig;
use sqlconnect_lib::table_result::{self, ContextReference};
use sqlconnect_lib::task_registry::{CleanupState, TaskPhase};
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

async fn setup() -> Arc<ConnectionPoolManager> {
    let manager = Arc::new(ConnectionPoolManager::new(None));
    let config: ConnectionConfig = serde_json::from_value(json!({"id":"A","name":"数据集测试","driverType":"sqlite","host":"","port":0,"user":"","password":"","database":":memory:"})).unwrap();
    manager.create_connection(config).await.unwrap();
    let driver = manager.get_connection("A").await.unwrap();
    for sql in [
        "CREATE TABLE items(id INTEGER NOT NULL PRIMARY KEY, rank INTEGER NOT NULL, label TEXT)",
        "INSERT INTO items VALUES(1,2,'一'),(2,2,'二'),(3,1,'三')",
    ] {
        driver.execute(sql, None, None).await.unwrap();
    }
    manager
}

fn page(
    table: &str,
    number: u32,
    size: u32,
    cursor: Option<String>,
    task: Option<String>,
) -> DatasetPageRequest {
    serde_json::from_value(json!({"request":{"protocolVersion":1,"connectionId":"A","database":"main","tableParts":[table],"page":number,"pageSize":size,"sort":{"column":"rank","direction":"desc"}},"cursor":cursor,"taskId":task})).unwrap()
}

fn count(page: &dataset::DatasetPage, task: Option<String>) -> TableCountRequest {
    TableCountRequest {
        protocol_version: 1,
        result_id: page.page.context.result_id.clone(),
        revision: page.page.context.revision,
        task_id: task,
    }
}

#[tokio::test]
async fn first_page_has_no_total_probe_row_never_gets_a_handle_and_count_is_independent() {
    let manager = setup().await;
    let page_task = Uuid::new_v4().to_string();
    let first = dataset::read_page(&manager, page("items", 1, 1, None, Some(page_task.clone())))
        .await
        .unwrap();
    assert_eq!(first.page.total_rows, None);
    assert!(first.has_more);
    assert_eq!(first.page.result.rows.len(), 1);
    assert_eq!(first.page.typed_rows.len(), 1);
    assert_eq!(first.page.row_handles.len(), 1);
    assert_eq!(
        manager
            .result_contexts
            .stats(chrono::Utc::now().timestamp_millis())
            .unwrap()
            .row_handles,
        1
    );
    assert_eq!(first.pagination_mode, "offset");
    assert_eq!(first.observed_order.len(), 2);
    assert!(first.next_cursor.is_some());
    assert!(first.approx_bytes > 0);
    assert!(first.truncated_reason.is_none());
    let state = manager.tasks.snapshot(&page_task).unwrap();
    assert_eq!(state.operation, "tableRead");
    assert_eq!(state.phase, TaskPhase::Succeeded);
    assert_eq!(state.cleanup_state, CleanupState::Succeeded);
    let count_task = Uuid::new_v4().to_string();
    let total = dataset::count_table(&manager, count(&first, Some(count_task.clone())))
        .await
        .unwrap();
    assert_eq!(total.total_rows, "3");
    assert_eq!(
        total.connection_generation,
        first.page.context.connection_generation
    );
    let counted = manager.tasks.snapshot(&count_task).unwrap();
    assert_eq!(counted.operation, "tableCount");
    assert_ne!(state.session_id, counted.session_id);
    assert_eq!(manager.tasks.active_count(), 0);
}

#[tokio::test]
async fn composite_mixed_direction_keyset_does_not_repeat_after_an_insert_before_anchor() {
    let manager = setup().await;
    let first = dataset::read_page(&manager, page("items", 1, 1, None, None))
        .await
        .unwrap();
    assert_eq!(first.page.result.rows[0].values[0], Some(json!(1)));
    manager
        .get_connection("A")
        .await
        .unwrap()
        .execute("INSERT INTO items VALUES(0,3,'提前插入')", None, None)
        .await
        .unwrap();
    let second = dataset::read_page(
        &manager,
        page("items", 2, 1, first.next_cursor.clone(), None),
    )
    .await
    .unwrap();
    assert_eq!(second.pagination_mode, "keyset");
    assert_eq!(second.page.result.rows[0].values[0], Some(json!(2)));
    let third = dataset::read_page(&manager, page("items", 3, 1, second.next_cursor, None))
        .await
        .unwrap();
    assert_eq!(third.page.result.rows[0].values[0], Some(json!(3)));
    assert!(!third.has_more);
    assert!(third.next_cursor.is_none());
    // 任意跳页明确走 offset，与连续 next 的语义保持独立。
    let jump = dataset::read_page(&manager, page("items", 3, 1, None, None))
        .await
        .unwrap();
    assert_eq!(jump.pagination_mode, "offset");
    assert_eq!(jump.page.result.rows[0].values[0], Some(json!(2)));
}

#[tokio::test]
async fn cursor_rejects_changed_conditions_foreign_tokens_released_context_and_changed_metadata() {
    let manager = setup().await;
    let first = dataset::read_page(&manager, page("items", 1, 1, None, None))
        .await
        .unwrap();
    let token = first.next_cursor.clone();
    assert_eq!(
        dataset::read_page(&manager, page("items", 2, 2, token.clone(), None))
            .await
            .unwrap_err()
            .code,
        "CURSOR_SOURCE_MISMATCH"
    );
    assert_eq!(
        dataset::read_page(
            &manager,
            page("items", 2, 1, Some(Uuid::new_v4().to_string()), None)
        )
        .await
        .unwrap_err()
        .code,
        "CURSOR_EXPIRED"
    );
    let mut altered = page("items", 2, 1, token.clone(), None);
    altered.request.sort.as_mut().unwrap().direction = table_result::SortDirection::Asc;
    assert_eq!(
        dataset::read_page(&manager, altered)
            .await
            .unwrap_err()
            .code,
        "CURSOR_SOURCE_MISMATCH"
    );
    manager
        .get_connection("A")
        .await
        .unwrap()
        .execute("ALTER TABLE items ADD COLUMN added INTEGER", None, None)
        .await
        .unwrap();
    assert_eq!(
        dataset::read_page(&manager, page("items", 2, 1, token.clone(), None))
            .await
            .unwrap_err()
            .code,
        "METADATA_CHANGED"
    );
    assert_eq!(
        dataset::count_table(&manager, count(&first, None))
            .await
            .unwrap_err()
            .code,
        "METADATA_CHANGED"
    );
    table_result::release_context(
        &manager,
        ContextReference {
            protocol_version: 1,
            result_id: first.page.context.result_id,
            revision: 1,
        },
    )
    .unwrap();
    assert_eq!(
        dataset::read_page(&manager, page("items", 2, 1, token, None))
            .await
            .unwrap_err()
            .code,
        "RESULT_CONTEXT_NOT_FOUND"
    );
}

#[tokio::test]
async fn nullable_or_text_order_falls_back_without_expanding_identity_types() {
    let manager = setup().await;
    let driver = manager.get_connection("A").await.unwrap();
    driver
        .execute(
            "CREATE TABLE unsafe(id INTEGER PRIMARY KEY, rank INTEGER)",
            None,
            None,
        )
        .await
        .unwrap();
    driver
        .execute("INSERT INTO unsafe VALUES(1,NULL),(2,1)", None, None)
        .await
        .unwrap();
    let fallback = dataset::read_page(&manager, page("unsafe", 1, 1, None, None))
        .await
        .unwrap();
    assert_eq!(
        fallback.pagination_fallback_reason.as_deref(),
        Some("KEYSET_NULLABLE_ORDER")
    );
    assert!(fallback.next_cursor.is_none());
    let mut request = page("items", 1, 1, None, None);
    request.request.sort.as_mut().unwrap().column = "label".into();
    let fallback = dataset::read_page(&manager, request).await.unwrap();
    assert!(fallback.next_cursor.is_none());
    assert!(fallback.pagination_fallback_reason.is_some());
}

#[tokio::test]
async fn readonly_view_count_uses_backend_source_and_preserves_readonly_policy() {
    let manager = setup().await;
    manager
        .get_connection("A")
        .await
        .unwrap()
        .execute(
            "CREATE VIEW visible_items AS SELECT * FROM items",
            None,
            None,
        )
        .await
        .unwrap();
    let view = dataset::read_page(&manager, page("visible_items", 1, 1, None, None))
        .await
        .unwrap();
    assert!(view.page.context.edit_policy.allowed_operations.is_empty());
    assert!(view.next_cursor.is_none());
    assert_eq!(
        view.pagination_fallback_reason.as_deref(),
        Some("KEYSET_SOURCE_NOT_BASE_TABLE")
    );
    let total = dataset::count_table(&manager, count(&view, None))
        .await
        .unwrap();
    assert_eq!(total.total_rows, "3");
    assert_eq!(total.result_id, view.page.context.result_id);
    assert_eq!(manager.tasks.active_count(), 0);
}

#[tokio::test]
async fn over_budget_never_publishes_context_and_next_read_recovers() {
    let manager = setup().await;
    manager
        .get_connection("A")
        .await
        .unwrap()
        .execute(
            "INSERT INTO items VALUES(4,9,replace(hex(zeroblob(600000)),'0','x'))",
            None,
            None,
        )
        .await
        .unwrap();
    let before = manager
        .result_contexts
        .stats(chrono::Utc::now().timestamp_millis())
        .unwrap();
    assert_eq!(
        dataset::read_page(&manager, page("items", 1, 1, None, None))
            .await
            .unwrap_err()
            .code,
        "RESULT_BUDGET_EXCEEDED"
    );
    assert_eq!(
        manager
            .result_contexts
            .stats(chrono::Utc::now().timestamp_millis())
            .unwrap(),
        before
    );
    assert_eq!(manager.tasks.active_count(), 0);
    manager
        .get_connection("A")
        .await
        .unwrap()
        .execute("DELETE FROM items WHERE id=4", None, None)
        .await
        .unwrap();
    dataset::read_page(&manager, page("items", 1, 1, None, None))
        .await
        .unwrap();
}

#[tokio::test]
async fn queued_read_and_count_cancel_or_abort_return_resources_to_baseline() {
    let manager = setup().await;
    let first = dataset::read_page(&manager, page("items", 1, 1, None, None))
        .await
        .unwrap();
    let driver = manager.get_connection("A").await.unwrap();
    let lease = driver.lease_session(Some(":memory:")).await.unwrap();
    for is_count in [false, true] {
        let task_id = Uuid::new_v4().to_string();
        let worker_manager = Arc::clone(&manager);
        let request = page("items", 1, 1, None, Some(task_id.clone()));
        let count_request = count(&first, Some(task_id.clone()));
        let worker = tokio::spawn(async move {
            if is_count {
                dataset::count_table(&worker_manager, count_request)
                    .await
                    .map(|_| ())
            } else {
                dataset::read_page(&worker_manager, request)
                    .await
                    .map(|_| ())
            }
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            while manager.tasks.snapshot(&task_id).is_err() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        manager.tasks.request_cancel(&task_id).await.unwrap();
        assert!(tokio::time::timeout(Duration::from_secs(2), worker)
            .await
            .unwrap()
            .unwrap()
            .is_err());
        assert_eq!(
            manager.tasks.snapshot(&task_id).unwrap().phase,
            TaskPhase::Cancelled
        );
        assert_eq!(manager.tasks.active_count(), 0);
    }
    let task_id = Uuid::new_v4().to_string();
    let worker_manager = Arc::clone(&manager);
    let request = page("items", 1, 1, None, Some(task_id.clone()));
    let worker = tokio::spawn(async move { dataset::read_page(&worker_manager, request).await });
    tokio::time::timeout(Duration::from_secs(2), async {
        while manager.tasks.snapshot(&task_id).is_err() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    worker.abort();
    let _ = worker.await;
    assert_eq!(manager.tasks.active_count(), 0);
    lease.release().await.unwrap();
    dataset::read_page(&manager, page("items", 1, 1, None, None))
        .await
        .unwrap();
    assert_eq!(manager.tasks.active_count(), 0);
}

#[test]
fn count_protocol_cannot_accept_an_alternate_source() {
    assert!(serde_json::from_value::<TableCountRequest>(json!({"protocolVersion":1,"resultId":Uuid::new_v4().to_string(),"revision":1,"taskId":null,"tableParts":["other"]})).is_err());
}
