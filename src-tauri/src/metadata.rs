//! 元数据来源、连接代次与修订。显式结构 SQL 仍沿用原执行权限，不授予网格写权限。
use crate::database::{pool::ConnectionPoolManager, DatabaseDriver};
use crate::models::{DatabaseError, ForeignKeyInfo, IndexInfo, ResolvedTable};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ObjectIdentity {
    pub database: String,
    pub schema: Option<String>,
    pub table: String,
}
impl ObjectIdentity {
    pub fn parts(&self, driver: &str) -> Vec<String> {
        match (driver, self.schema.as_ref()) {
            ("mysql" | "mariadb", _) => vec![self.database.clone(), self.table.clone()],
            ("mssql", Some(schema)) => {
                vec![self.database.clone(), schema.clone(), self.table.clone()]
            }
            (_, Some(schema)) => vec![schema.clone(), self.table.clone()],
            _ => vec![self.table.clone()],
        }
    }
}
impl From<&ResolvedTable> for ObjectIdentity {
    fn from(table: &ResolvedTable) -> Self {
        Self {
            database: table.database.clone(),
            schema: table.schema.clone(),
            table: table.table.clone(),
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MetadataSnapshot {
    pub connection_id: String,
    pub connection_generation: String,
    pub revision: String,
    pub target: ResolvedTable,
    pub indexes: Vec<IndexInfo>,
    pub foreign_keys: Vec<ForeignKeyInfo>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExecuteDdlRequest {
    pub connection_id: String,
    pub connection_generation: String,
    pub database: String,
    pub table_parts: Vec<String>,
    pub revision: String,
    pub statements: Vec<String>,
}

fn error(code: &str, message: &str) -> DatabaseError {
    DatabaseError::QueryError(format!("{code}: {message}"))
}

/// 旧接口中的 table 是一段真实目录名，绝不再按点号拆开。
pub async fn resolve_legacy<D: DatabaseDriver + ?Sized>(
    driver: &D,
    database: Option<&str>,
    table: &str,
) -> Result<ResolvedTable, DatabaseError> {
    let db = crate::database::current_database(driver, database).await?;
    driver.resolve_table(&db, &[table.to_owned()]).await
}

pub async fn read(
    manager: &ConnectionPoolManager,
    connection_id: &str,
    database: &str,
    parts: &[String],
) -> Result<MetadataSnapshot, DatabaseError> {
    if parts.is_empty() || parts.len() > 3 || parts.iter().any(|part| part.is_empty()) {
        return Err(error(
            "METADATA_IDENTITY_INVALID",
            "对象身份需要一至三段非空名称",
        ));
    }
    let snapshot = manager.get_connection_snapshot(connection_id).await?;
    let database = if snapshot.driver.driver_type() == "sqlite" {
        snapshot.database.as_deref().unwrap_or(database)
    } else {
        database
    };
    let target = snapshot.driver.resolve_table(database, parts).await?;
    let indexes = snapshot.driver.get_indexes_for(&target).await?;
    let foreign_keys = snapshot.driver.get_foreign_keys_for(&target).await?;
    let encoded = serde_json::to_vec(&(
        connection_id,
        &snapshot.generation,
        &target,
        &indexes,
        &foreign_keys,
    ))
    .map_err(|_| error("METADATA_ENCODING_FAILED", "元数据无法编码"))?;
    if encoded.len() > 16 * 1024 * 1024 {
        return Err(error(
            "METADATA_BUDGET_EXCEEDED",
            "单个对象元数据超过 16 MiB，未发布不完整快照",
        ));
    }
    let current = manager
        .active_connection_snapshot(connection_id)
        .await
        .map_err(|_| error("METADATA_CONNECTION_CHANGED", "读取期间连接已断开"))?;
    if current.generation != snapshot.generation {
        return Err(error(
            "METADATA_CONNECTION_CHANGED",
            "读取期间连接代次已改变",
        ));
    }
    Ok(MetadataSnapshot {
        connection_id: connection_id.into(),
        connection_generation: snapshot.generation,
        revision: format!("{:x}", Sha256::digest(encoded)),
        target,
        indexes,
        foreign_keys,
    })
}

/// 提交前核对目录修订，并在实际租约获取时再次核对连接代次。
/// 这不是数据库 DDL 的跨会话原子锁；数据库仍负责每条 DDL 的并发与隐式提交语义。
pub async fn execute_ddl(
    manager: &ConnectionPoolManager,
    request: ExecuteDdlRequest,
) -> Result<u64, String> {
    let original = manager
        .get_connection_snapshot(&request.connection_id)
        .await
        .map_err(|error| error.to_string())?;
    if original.generation != request.connection_generation {
        return Err("METADATA_CONNECTION_CHANGED: 结构草稿属于旧连接，请重新读取".into());
    }
    let current = read(
        manager,
        &request.connection_id,
        &request.database,
        &request.table_parts,
    )
    .await
    .map_err(|error| error.to_string())?;
    if current.connection_generation != request.connection_generation
        || current.revision != request.revision
    {
        return Err("METADATA_REVISION_CHANGED: 对象结构已变化，保留草稿，请重新读取后核对".into());
    }
    let outcome = crate::query_tasks::execute_batch_checked(
        manager,
        request.connection_id.clone(),
        Some(current.target.database),
        request.statements,
        None,
        None,
        Some(request.connection_generation.clone()),
    )
    .await;
    // DDL 可能隐式或部分提交，失败也不能继续沿用旧结果身份。
    if manager
        .active_connection_snapshot(&request.connection_id)
        .await
        .is_ok_and(|snapshot| snapshot.generation == request.connection_generation)
    {
        let _ = manager.result_contexts.invalidate_connection(
            &request.connection_id,
            chrono::Utc::now().timestamp_millis(),
        );
    }
    outcome
}
