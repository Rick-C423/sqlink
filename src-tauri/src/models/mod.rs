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
    /// SQL Server 是否启用 TLS；旧配置缺省为 false，避免先等待 TLS 握手失败。
    pub encrypt: Option<bool>,
    pub save_password: Option<bool>,
    pub group_name: Option<String>,
    pub color: Option<String>,
    /// 是否允许内置 MCP 服务访问该连接；旧配置缺省视为允许，保持历史行为。
    pub mcp_allowed: Option<bool>,
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
    #[serde(serialize_with = "serialize_query_values")]
    pub values: Vec<Option<serde_json::Value>>,
}

/// JavaScript 的整数安全范围；超出范围时，结果协议使用十进制字符串。
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

pub(crate) fn integer_display_json(number: &serde_json::Number) -> serde_json::Value {
    let unsafe_integer = number
        .as_i64()
        .is_some_and(|value| value.unsigned_abs() > MAX_SAFE_INTEGER)
        || number
            .as_u64()
            .is_some_and(|value| value > MAX_SAFE_INTEGER);
    if unsafe_integer {
        serde_json::Value::String(number.to_string())
    } else {
        serde_json::Value::Number(number.clone())
    }
}

/// 只在结果传输时保护标量整数，保留后端原值供 SQL 转储和数据库类型判断使用。
fn serialize_query_values<S>(
    values: &[Option<serde_json::Value>],
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    use serde::ser::SerializeSeq;
    struct QueryValue<'a>(&'a Option<serde_json::Value>);
    impl Serialize for QueryValue<'_> {
        fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            match self.0 {
                Some(serde_json::Value::Number(number)) => {
                    Some(integer_display_json(number)).serialize(serializer)
                }
                value => value.serialize(serializer),
            }
        }
    }
    let mut sequence = serializer.serialize_seq(Some(values.len()))?;
    for value in values {
        sequence.serialize_element(&QueryValue(value))?;
    }
    sequence.end()
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

/// 数据库名称及当前登录名是否有权访问。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DatabaseInfo {
    pub name: String,
    pub accessible: bool,
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

/// 查询执行上下文中已解析的完整表身份及列信息。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedTable {
    pub database: String,
    pub schema: Option<String>,
    pub table: String,
    pub columns: Vec<ColumnInfo>,
    /// 来自真实系统目录；未知来源不能建立可定位的行身份。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub object_kind: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexKeyPart {
    pub column: Option<String>,
    /// 未能无损分解表达式时与 column 同为 None，完整定义由 definition 保存。
    pub expression: Option<String>,
    pub descending: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexInfo {
    pub name: String,
    pub columns: Vec<String>,
    pub is_unique: bool,
    pub is_primary: bool,
    pub key_parts: Vec<IndexKeyPart>,
    pub included_columns: Vec<String>,
    pub definition: Option<String>,
    pub predicate: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ForeignKeyInfo {
    pub name: String,
    pub columns: Vec<String>,
    pub ref_table: String,
    pub ref_schema: Option<String>,
    pub ref_database: Option<String>,
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
    /// 语句成功数不是提交回执，未闭合事务在租约归还时会回滚。
    pub cleanup_notice: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DumpSummary {
    /// 导出的表数量
    pub tables: u64,
    /// 导出的数据行总数
    pub rows: u64,
}

/// 数据库账号信息（服务器级）
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DbUserInfo {
    pub name: String,
    /// MySQL 的 host 部分（如 %、localhost）；其他驱动为 None
    pub host: Option<String>,
    /// 附加说明（如 PG 的 superuser / SQL Server 的类型）
    pub note: Option<String>,
}

/// 当前会话用户及其授权语句（可读文本列表）
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CurrentUserInfo {
    pub user: String,
    pub grants: Vec<String>,
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

#[cfg(test)]
mod query_integer_tests {
    use super::*;
    use serde_json::{json, Value};

    #[test]
    fn signed_integer_wire_values_match_javascript_fixture_without_mutating_native_values() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/plain-query-integers.json"
        ))
        .unwrap();
        for expected in fixture["rows"].as_array().unwrap() {
            let text = expected["values"][1].as_str().unwrap();
            let integer = text.parse::<i64>().unwrap();
            let row = RowData {
                values: vec![Some(json!(integer)), Some(json!(text))],
            };
            assert_eq!(serde_json::to_value(&row).unwrap(), *expected);
            assert_eq!(row.values[0].as_ref().unwrap().as_i64(), Some(integer));
            assert_eq!(
                crate::typed_value::TypedValue::Integer(text.into()).display_json(),
                expected["values"][0]
            );
        }
    }

    #[test]
    fn unsigned_integer_wire_values_keep_safe_numbers_and_exact_uint64_strings() {
        for (integer, expected) in [
            (0, json!(0)),
            (9_007_199_254_740_991, json!(9_007_199_254_740_991u64)),
            (9_007_199_254_740_992, json!("9007199254740992")),
            (9_007_199_254_740_993, json!("9007199254740993")),
            (u64::MAX, json!("18446744073709551615")),
        ] {
            let row = RowData {
                values: vec![Some(json!(integer))],
            };
            assert_eq!(serde_json::to_value(&row).unwrap()["values"][0], expected);
            assert_eq!(row.values[0].as_ref().unwrap().as_u64(), Some(integer));
            assert_eq!(
                crate::typed_value::TypedValue::UnsignedInteger(integer.to_string()).display_json(),
                expected
            );
        }
    }

    #[test]
    fn integer_wire_protection_does_not_reinterpret_text_floats_booleans_or_null() {
        let values = vec![
            Some(json!("9007199254740993")),
            Some(json!("0009007199254740993")),
            Some(json!("18446744073709551615.00")),
            Some(json!(9_007_199_254_740_992f64)),
            Some(json!(1.25)),
            Some(json!(true)),
            Some(Value::Null),
            None,
        ];
        let row = RowData {
            values: values.clone(),
        };
        assert_eq!(serde_json::to_value(&row).unwrap()["values"], json!(values));
        assert_eq!(row.values, values);
    }
}
