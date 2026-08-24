use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionConfig {
    pub id: Option<String>,
    pub name: String,
    pub driver_type: String,
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
    pub database: Option<String>,
    pub save_password: Option<bool>,
    pub group_name: Option<String>,
    pub color: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ColumnMetadata {
    pub name: String,
    pub data_type: String,
    pub unified_type: String,
    pub nullable: bool,
    pub length: Option<usize>,
    pub precision: Option<u8>,
    pub scale: Option<u8>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RowData {
    pub values: Vec<Option<serde_json::Value>>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QueryResult {
    pub columns: Vec<ColumnMetadata>,
    pub rows: Vec<RowData>,
    pub total_count: Option<usize>,
    pub execution_time_ms: u64,
    pub rows_affected: Option<u64>,
    pub last_insert_id: Option<u64>,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TableInfo {
    pub name: String,
    pub schema: Option<String>,
    pub row_count: Option<i64>,
    pub engine: Option<String>,
    pub comment: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ColumnInfo {
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
    pub is_primary_key: bool,
    pub default_value: Option<String>,
    pub extra: Option<String>,
    pub comment: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexInfo {
    pub name: String,
    pub columns: Vec<String>,
    pub is_unique: bool,
    pub is_primary: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ForeignKeyInfo {
    pub name: String,
    pub columns: Vec<String>,
    pub ref_table: String,
    pub ref_schema: Option<String>,
    pub ref_columns: Vec<String>,
    pub on_update: Option<String>,
    pub on_delete: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryEntry {
    pub id: String,
    pub connection_id: String,
    pub database: Option<String>,
    pub sql: String,
    /// epoch 毫秒
    pub executed_at: i64,
    pub duration_ms: u64,
    /// 返回行数（查询）或影响行数（写入）
    pub row_count: Option<u64>,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportSummary {
    /// 拆分出的语句总数
    pub statements: u64,
    /// 实际执行成功的条数
    pub executed: u64,
    /// 因表已存在而跳过的 CREATE TABLE 条数（on_conflict = skip）
    pub skipped: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DumpSummary {
    /// 导出的表数量
    pub tables: u64,
    /// 导出的数据行总数
    pub rows: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum DatabaseError {
    #[error("连接错误: {0}")]
    ConnectionError(String),
    #[error("查询错误: {0}")]
    QueryError(String),
    #[error("执行错误: {0}")]
    ExecuteError(String),
    #[error("类型错误: {0}")]
    TypeError(String),
    #[error("连接已关闭")]
    ConnectionClosed,
    #[error("查询已取消")]
    QueryCancelled,
    #[error("超时")]
    Timeout,
    #[error("不支持的驱动: {0}")]
    DriverNotSupported(String),
    #[error("未知错误: {0}")]
    Unknown(String),
}

impl serde::Serialize for DatabaseError {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::ser::Serializer,
    {
        serializer.serialize_str(self.to_string().as_ref())
    }
}
