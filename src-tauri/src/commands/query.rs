use crate::database::pool::ConnectionPoolManager;
use crate::models::*;
use crate::result_context::{ContextError, ResultContext};
use crate::table_result::{
    ContextReference, ContextValidationRequest, TablePage, TablePageRequest,
};
use tauri::State;

#[tauri::command]
pub async fn changeset_commit(
    request: crate::changeset::ChangeSetRequest,
    task_id: Option<String>,
    manager: State<'_, ConnectionPoolManager>,
) -> Result<crate::commit_receipt::CommitReceipt, ContextError> {
    crate::commit::commit_changeset(&manager, request, task_id).await
}

#[tauri::command]
pub async fn changeset_receipt(
    change_set_id: String,
    result_id: String,
    revision: u64,
    manager: State<'_, ConnectionPoolManager>,
) -> Result<crate::commit_receipt::CommitReceipt, ContextError> {
    manager
        .commit_receipts
        .get(&change_set_id, &result_id, revision)
}

#[tauri::command]
pub async fn changeset_validate(
    request: crate::changeset::ChangeSetRequest,
    manager: State<'_, ConnectionPoolManager>,
) -> Result<crate::changeset::ChangeSetValidation, ContextError> {
    crate::changeset::validate_changeset(&manager, request).await
}

#[tauri::command]
pub async fn table_read_page(
    request: TablePageRequest,
    manager: State<'_, ConnectionPoolManager>,
) -> Result<TablePage, ContextError> {
    crate::table_result::read_table_page(&manager, request).await
}

#[tauri::command]
pub async fn table_read_dataset_page(
    request: crate::dataset::DatasetPageRequest,
    manager: State<'_, ConnectionPoolManager>,
) -> Result<crate::dataset::DatasetPage, ContextError> {
    crate::dataset::read_page(&manager, request).await
}

#[tauri::command]
pub async fn table_count(
    request: crate::dataset::TableCountRequest,
    manager: State<'_, ConnectionPoolManager>,
) -> Result<crate::dataset::TableCountResult, ContextError> {
    crate::dataset::count_table(&manager, request).await
}

async fn cancel_scoped_task(
    manager: &ConnectionPoolManager,
    connection_id: &str,
    task_id: &str,
    operations: &[&str],
) -> Result<(), String> {
    let task = manager
        .tasks
        .snapshot(task_id)
        .map_err(|error| error.to_string())?;
    if task.connection_id != connection_id || !operations.contains(&task.operation.as_str()) {
        return Err("TASK_SOURCE_MISMATCH: 取消任务的来源或操作不一致".into());
    }
    manager
        .tasks
        .request_cancel(task_id)
        .await
        .map(|_| ())
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn table_task_cancel(
    connection_id: String,
    task_id: String,
    manager: State<'_, ConnectionPoolManager>,
) -> Result<(), String> {
    cancel_scoped_task(
        &manager,
        &connection_id,
        &task_id,
        &["tableRead", "tableCount"],
    )
    .await
}

#[tauri::command]
pub async fn import_cancel(
    connection_id: String,
    task_id: String,
    manager: State<'_, ConnectionPoolManager>,
) -> Result<(), String> {
    cancel_scoped_task(
        &manager,
        &connection_id,
        &task_id,
        &["import", "importPlan"],
    )
    .await
}

#[cfg(test)]
mod dataset_cancel_tests {
    use super::*;

    #[tokio::test]
    async fn cancellation_requires_original_connection_and_operation() {
        let manager = ConnectionPoolManager::new(None);
        let page = manager.tasks.start(None, "A", None, "tableRead").unwrap();
        assert!(
            cancel_scoped_task(&manager, "B", page.task_id(), &["tableRead", "tableCount"])
                .await
                .is_err()
        );
        assert!(!page.cancel_requested());
        assert!(
            cancel_scoped_task(&manager, "A", page.task_id(), &["import", "importPlan"])
                .await
                .is_err()
        );
        assert!(!page.cancel_requested());
        cancel_scoped_task(&manager, "A", page.task_id(), &["tableRead", "tableCount"])
            .await
            .unwrap();
        assert!(page.cancel_requested());
        let import = manager.tasks.start(None, "A", None, "import").unwrap();
        assert!(cancel_scoped_task(
            &manager,
            "A",
            import.task_id(),
            &["tableRead", "tableCount"]
        )
        .await
        .is_err());
        assert!(!import.cancel_requested());
        cancel_scoped_task(&manager, "A", import.task_id(), &["import", "importPlan"])
            .await
            .unwrap();
        assert!(import.cancel_requested());
    }
}

#[tauri::command]
pub async fn result_context_validate(
    request: ContextValidationRequest,
    manager: State<'_, ConnectionPoolManager>,
) -> Result<ResultContext, ContextError> {
    crate::table_result::validate_context(&manager, request).await
}

#[tauri::command]
pub async fn result_context_release(
    request: ContextReference,
    manager: State<'_, ConnectionPoolManager>,
) -> Result<(), ContextError> {
    crate::table_result::release_context(&manager, request)
}

#[tauri::command]
pub async fn query_execute(
    connection_id: String,
    sql: String,
    database: Option<String>,
    query_id: Option<u64>,
    // 为 true 时记录到查询历史（仅编辑器手动执行传 true，内部加载不传）
    record: Option<bool>,
    task_id: Option<String>,
    manager: State<'_, ConnectionPoolManager>,
) -> Result<crate::query_tasks::TaskQueryResult, String> {
    crate::query_tasks::execute_query(
        &manager,
        connection_id,
        sql,
        database,
        query_id,
        task_id,
        record == Some(true),
    )
    .await
}

#[tauri::command]
pub async fn query_cancel(
    connection_id: String,
    query_id: u64,
    task_id: Option<String>,
    manager: State<'_, ConnectionPoolManager>,
) -> Result<(), String> {
    let id = manager
        .tasks
        .resolve_cancel_id(&connection_id, query_id, task_id.as_deref())
        .map_err(|e| e.to_string())?;
    manager
        .tasks
        .request_cancel(&id)
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn query_execute_batch(
    connection_id: String,
    database: Option<String>,
    statements: Vec<String>,
    query_id: Option<u64>,
    task_id: Option<String>,
    manager: State<'_, ConnectionPoolManager>,
) -> Result<u64, String> {
    crate::query_tasks::execute_batch(
        &manager,
        connection_id,
        database,
        statements,
        query_id,
        task_id,
    )
    .await
}

#[tauri::command]
pub async fn task_get(
    task_id: String,
    manager: State<'_, ConnectionPoolManager>,
) -> Result<crate::task_registry::TaskSnapshot, crate::task_registry::TaskError> {
    manager.tasks.snapshot(&task_id)
}

#[tauri::command]
pub async fn connection_session_capabilities(
    connection_id: String,
    manager: State<'_, ConnectionPoolManager>,
) -> Result<crate::session::SessionCapabilities, String> {
    let snapshot = manager
        .get_connection_snapshot(&connection_id)
        .await
        .map_err(|e| e.to_string())?;
    Ok(snapshot.driver.session_capabilities())
}

/// 网格不能提交显示值重建的 SQL；受控提交门禁未齐备时失败关闭。
/// 通用 SQL 执行仍供用户在编辑器中主动执行脚本，不作为网格保存入口。
#[tauri::command]
pub async fn query_apply_grid_edits() -> Result<(), String> {
    Err("GRID_EDIT_CONTEXT_REQUIRED: 受控提交安全校验尚未完成，网格暂为只读".into())
}

#[cfg(test)]
mod grid_edit_tests {
    #[tokio::test]
    async fn grid_write_without_trusted_context_is_rejected() {
        let error = super::query_apply_grid_edits().await.unwrap_err();
        assert!(error.starts_with("GRID_EDIT_CONTEXT_REQUIRED:"));
    }
}

/// 整表导出 CSV，返回导出行数
#[tauri::command]
pub async fn table_export_csv(
    connection_id: String,
    database: Option<String>,
    table: String,
    path: String,
    task_id: Option<String>,
    schema: Option<String>,
    manager: State<'_, ConnectionPoolManager>,
) -> Result<u64, String> {
    let sources = if let Some(schema) = schema {
        let driver = manager
            .get_connection(&connection_id)
            .await
            .map_err(|error| error.to_string())?;
        Some(vec![crate::metadata::ObjectIdentity {
            database: crate::database::current_database(&*driver, database.as_deref())
                .await
                .map_err(|error| error.to_string())?,
            schema: Some(schema),
            table: table.clone(),
        }])
    } else {
        None
    };
    crate::export::export_job_sources(
        &manager,
        connection_id,
        database,
        Some(vec![table]),
        sources,
        path,
        false,
        false,
        true,
        task_id,
    )
    .await
    .map(|summary| summary.rows)
}

/// 导出取消仅接受原任务的连接身份，不能借此取消编辑器或其他连接任务。
#[tauri::command]
pub async fn export_cancel(
    connection_id: String,
    task_id: String,
    manager: State<'_, ConnectionPoolManager>,
) -> Result<(), String> {
    let snapshot = manager
        .tasks
        .snapshot(&task_id)
        .map_err(|error| error.to_string())?;
    if snapshot.connection_id != connection_id || snapshot.operation != "export" {
        return Err("TASK_SOURCE_MISMATCH: 导出取消来源与原任务不一致".into());
    }
    manager
        .tasks
        .request_cancel(&task_id)
        .await
        .map(|_| ())
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn schema_get_databases(
    connection_id: String,
    manager: State<'_, ConnectionPoolManager>,
) -> Result<Vec<String>, String> {
    let driver = manager
        .get_connection(&connection_id)
        .await
        .map_err(|e| e.to_string())?;
    driver.get_databases().await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn schema_get_database_infos(
    connection_id: String,
    manager: State<'_, ConnectionPoolManager>,
) -> Result<Vec<DatabaseInfo>, String> {
    let driver = manager
        .get_connection(&connection_id)
        .await
        .map_err(|e| e.to_string())?;
    driver.get_database_infos().await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn schema_get_tables(
    connection_id: String,
    database: Option<String>,
    manager: State<'_, ConnectionPoolManager>,
) -> Result<Vec<TableInfo>, String> {
    let driver = manager
        .get_connection(&connection_id)
        .await
        .map_err(|e| e.to_string())?;
    driver
        .get_tables(database.as_deref())
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn schema_resolve_table(
    connection_id: String,
    database: String,
    table_parts: Vec<String>,
    manager: State<'_, ConnectionPoolManager>,
) -> Result<ResolvedTable, String> {
    let driver = manager
        .get_connection(&connection_id)
        .await
        .map_err(|e| e.to_string())?;
    driver
        .resolve_table(&database, &table_parts)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn schema_get_columns(
    connection_id: String,
    database: Option<String>,
    table: String,
    manager: State<'_, ConnectionPoolManager>,
) -> Result<Vec<ColumnInfo>, String> {
    let driver = manager
        .get_connection(&connection_id)
        .await
        .map_err(|e| e.to_string())?;
    driver
        .get_columns(database.as_deref(), &table)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn schema_get_table_metadata(
    connection_id: String,
    database: String,
    table_parts: Vec<String>,
    manager: State<'_, ConnectionPoolManager>,
) -> Result<crate::metadata::MetadataSnapshot, String> {
    crate::metadata::read(&manager, &connection_id, &database, &table_parts)
        .await
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn schema_execute_ddl(
    request: crate::metadata::ExecuteDdlRequest,
    manager: State<'_, ConnectionPoolManager>,
) -> Result<u64, String> {
    crate::metadata::execute_ddl(&manager, request).await
}

#[tauri::command]
pub async fn schema_get_indexes(
    connection_id: String,
    database: Option<String>,
    table: String,
    manager: State<'_, ConnectionPoolManager>,
) -> Result<Vec<IndexInfo>, String> {
    let driver = manager
        .get_connection(&connection_id)
        .await
        .map_err(|e| e.to_string())?;
    driver
        .get_indexes(database.as_deref(), &table)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn schema_get_foreign_keys(
    connection_id: String,
    database: Option<String>,
    table: String,
    manager: State<'_, ConnectionPoolManager>,
) -> Result<Vec<ForeignKeyInfo>, String> {
    let driver = manager
        .get_connection(&connection_id)
        .await
        .map_err(|e| e.to_string())?;
    driver
        .get_foreign_keys(database.as_deref(), &table)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn schema_get_views(
    connection_id: String,
    database: Option<String>,
    manager: State<'_, ConnectionPoolManager>,
) -> Result<Vec<TableInfo>, String> {
    let driver = manager
        .get_connection(&connection_id)
        .await
        .map_err(|e| e.to_string())?;
    driver
        .get_views(database.as_deref())
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn schema_get_view_definition(
    connection_id: String,
    database: Option<String>,
    view: String,
    schema: Option<String>,
    manager: State<'_, ConnectionPoolManager>,
) -> Result<String, String> {
    let driver = manager
        .get_connection(&connection_id)
        .await
        .map_err(|e| e.to_string())?;
    if let Some(schema) = schema {
        let db = crate::database::current_database(&*driver, database.as_deref())
            .await
            .map_err(|error| error.to_string())?;
        let target = driver
            .resolve_table(&db, &[schema, view])
            .await
            .map_err(|error| error.to_string())?;
        driver
            .get_view_definition_for(&target)
            .await
            .map_err(|error| error.to_string())
    } else {
        driver
            .get_view_definition(database.as_deref(), &view)
            .await
            .map_err(|error| error.to_string())
    }
}

#[tauri::command]
pub async fn schema_get_triggers(
    connection_id: String,
    database: Option<String>,
    manager: State<'_, ConnectionPoolManager>,
) -> Result<Vec<TableInfo>, String> {
    let driver = manager
        .get_connection(&connection_id)
        .await
        .map_err(|e| e.to_string())?;
    driver
        .get_triggers(database.as_deref())
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn schema_get_trigger_definition(
    connection_id: String,
    database: Option<String>,
    trigger: String,
    manager: State<'_, ConnectionPoolManager>,
) -> Result<String, String> {
    let driver = manager
        .get_connection(&connection_id)
        .await
        .map_err(|e| e.to_string())?;
    driver
        .get_trigger_definition(database.as_deref(), &trigger)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn schema_get_procedures(
    connection_id: String,
    database: Option<String>,
    manager: State<'_, ConnectionPoolManager>,
) -> Result<Vec<TableInfo>, String> {
    let driver = manager
        .get_connection(&connection_id)
        .await
        .map_err(|e| e.to_string())?;
    driver
        .get_procedures(database.as_deref())
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn schema_get_procedure_definition(
    connection_id: String,
    database: Option<String>,
    procedure: String,
    manager: State<'_, ConnectionPoolManager>,
) -> Result<String, String> {
    let driver = manager
        .get_connection(&connection_id)
        .await
        .map_err(|e| e.to_string())?;
    driver
        .get_procedure_definition(database.as_deref(), &procedure)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn schema_get_functions(
    connection_id: String,
    database: Option<String>,
    manager: State<'_, ConnectionPoolManager>,
) -> Result<Vec<TableInfo>, String> {
    let driver = manager
        .get_connection(&connection_id)
        .await
        .map_err(|e| e.to_string())?;
    driver
        .get_functions(database.as_deref())
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn schema_get_function_definition(
    connection_id: String,
    database: Option<String>,
    function: String,
    manager: State<'_, ConnectionPoolManager>,
) -> Result<String, String> {
    let driver = manager
        .get_connection(&connection_id)
        .await
        .map_err(|e| e.to_string())?;
    driver
        .get_function_definition(database.as_deref(), &function)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn schema_get_users(
    connection_id: String,
    database: Option<String>,
    manager: State<'_, ConnectionPoolManager>,
) -> Result<Vec<DbUserInfo>, String> {
    let driver = manager
        .get_connection(&connection_id)
        .await
        .map_err(|e| e.to_string())?;
    driver
        .get_users(database.as_deref())
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn schema_get_current_user(
    connection_id: String,
    database: Option<String>,
    manager: State<'_, ConnectionPoolManager>,
) -> Result<CurrentUserInfo, String> {
    let driver = manager
        .get_connection(&connection_id)
        .await
        .map_err(|e| e.to_string())?;
    driver
        .get_current_user(database.as_deref())
        .await
        .map_err(|e| e.to_string())
}
