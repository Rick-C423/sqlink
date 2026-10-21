//! 类型化变更预检。此模块只编译参数计划，不执行写入，不授予网格编辑权限。

use crate::database::{pool::ConnectionPoolManager, DatabaseDriver};
use crate::models::{ColumnInfo, ResolvedTable};
use crate::result_context::{ContextError, ResultContext, StoredRowIdentity};
use crate::table_result::{self, quote, source_parts, table_sql, ContextValidationRequest};
use crate::typed_value::{TimezoneKind, TypedValue};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use uuid::Uuid;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "mode", rename_all = "camelCase", deny_unknown_fields)]
pub enum CellInput {
    Set { value: TypedValue },
    Default,
}

// 重复字段不能在 JSON map 反序列化时被静默覆盖。
fn changes<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<BTreeMap<String, CellInput>, D::Error> {
    struct UniqueChanges;
    impl<'de> serde::de::Visitor<'de> for UniqueChanges {
        type Value = BTreeMap<String, CellInput>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("唯一 columnId 到变更意图的映射")
        }
        fn visit_map<M: serde::de::MapAccess<'de>>(
            self,
            mut map: M,
        ) -> Result<Self::Value, M::Error> {
            let mut result = BTreeMap::new();
            while let Some((key, value)) = map.next_entry::<String, CellInput>()? {
                if result.insert(key, value).is_some() {
                    return Err(serde::de::Error::custom("重复 columnId"));
                }
                if result.len() > 2000 {
                    return Err(serde::de::Error::custom("字段数量超限"));
                }
            }
            Ok(result)
        }
    }
    deserializer.deserialize_map(UniqueChanges)
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum ChangeOperation {
    Insert {
        #[serde(rename = "opId")]
        op_id: String,
        #[serde(deserialize_with = "changes")]
        changes: BTreeMap<String, CellInput>,
    },
    Update {
        #[serde(rename = "opId")]
        op_id: String,
        #[serde(rename = "rowHandle")]
        row_handle: String,
        #[serde(deserialize_with = "changes")]
        changes: BTreeMap<String, CellInput>,
    },
    Delete {
        #[serde(rename = "opId")]
        op_id: String,
        #[serde(rename = "rowHandle")]
        row_handle: String,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ChangeSetRequest {
    pub protocol_version: u32,
    pub change_set_id: String,
    pub result_id: String,
    pub revision: u64,
    pub operations: Vec<ChangeOperation>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChangeSetValidation {
    pub change_set_id: String,
    pub result_id: String,
    pub revision: u64,
    pub operation_count: usize,
    pub executable: bool,
    pub reason_codes: Vec<String>,
}

/// 不实现 Serialize；SQL、参数、真实键不能作为预检响应下发。
#[allow(dead_code)]
#[derive(Debug)]
pub(crate) struct BoundOperation {
    pub op_id: String,
    pub sql: String,
    pub params: Vec<TypedValue>,
}

fn error(code: &str, message: &str) -> ContextError {
    ContextError::new(code, message)
}
fn uuid(value: &str) -> bool {
    Uuid::parse_str(value).is_ok()
}

impl ChangeOperation {
    pub(crate) fn op_id(&self) -> &str {
        match self {
            Self::Insert { op_id, .. }
            | Self::Update { op_id, .. }
            | Self::Delete { op_id, .. } => op_id,
        }
    }
    pub(crate) fn handle(&self) -> Option<&str> {
        match self {
            Self::Update { row_handle, .. } | Self::Delete { row_handle, .. } => Some(row_handle),
            _ => None,
        }
    }
    pub(crate) fn changes(&self) -> Option<&BTreeMap<String, CellInput>> {
        match self {
            Self::Insert { changes, .. } | Self::Update { changes, .. } => Some(changes),
            _ => None,
        }
    }
}

pub(crate) fn validate_shape(request: &ChangeSetRequest) -> Result<Vec<String>, ContextError> {
    if request.protocol_version != 1 {
        return Err(error("UNSUPPORTED_PROTOCOL", "变更协议不兼容"));
    }
    if !uuid(&request.change_set_id) || !uuid(&request.result_id) || request.revision == 0 {
        return Err(error("INVALID_REFERENCE", "变更集或结果引用无效"));
    }
    if request.operations.is_empty() || request.operations.len() > 1000 {
        return Err(error(
            "INVALID_CHANGESET",
            "变更集必须包含 1 至 1000 个操作",
        ));
    }
    let mut ids = HashSet::new();
    let mut handles = HashSet::new();
    let mut ordered = Vec::new();
    let mut bytes = 0usize;
    for op in &request.operations {
        if !uuid(op.op_id()) || !ids.insert(op.op_id().to_ascii_lowercase()) {
            return Err(error("INVALID_OPERATION", "操作标识无效或重复"));
        }
        if let Some(handle) = op.handle() {
            if !uuid(handle) || !handles.insert(handle.to_ascii_lowercase()) {
                return Err(error("INVALID_ROW_HANDLE", "行引用无效或在变更集中重复"));
            }
            ordered.push(handle.to_owned());
        }
        if let Some(changes) = op.changes() {
            if changes.len() > 2000
                || (matches!(op, ChangeOperation::Update { .. }) && changes.is_empty())
            {
                return Err(error("INVALID_OPERATION", "修改字段不能为空或超限"));
            }
            for (column, input) in changes {
                if !uuid(column) {
                    return Err(error("INVALID_COLUMN", "字段引用无效"));
                }
                if let CellInput::Set { value } = input {
                    value.validate().map_err(|_| {
                        error("INVALID_TYPED_VALUE", "字段值格式、精度或大小不受支持")
                    })?;
                    bytes = bytes
                        .checked_add(value.allocated_bytes())
                        .ok_or_else(|| error("CHANGESET_TOO_LARGE", "变更载荷过大"))?;
                }
                bytes = bytes
                    .checked_add(256)
                    .ok_or_else(|| error("CHANGESET_TOO_LARGE", "变更载荷过大"))?;
            }
        }
    }
    if bytes > 8 * 1024 * 1024 {
        return Err(error("CHANGESET_TOO_LARGE", "变更载荷超过 8 MiB"));
    }
    Ok(ordered)
}

fn type_parts(declared: &str) -> Result<(String, Vec<u32>), ContextError> {
    let declared = declared.to_ascii_lowercase();
    let base = declared.split(['(', ' ']).next().unwrap_or("").to_owned();
    let args = if let Some((_, rest)) = declared.split_once('(') {
        let (args, _) = rest
            .split_once(')')
            .ok_or_else(|| error("COLUMN_TYPE_UNSUPPORTED", "字段类型限定无法无损解析"))?;
        if args == "max" && ["nvarchar", "varchar", "varbinary"].contains(&base.as_str()) {
            vec![]
        } else {
            args.split(',')
                .map(|v| v.trim().parse::<u32>())
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| {
                    error(
                        "COLUMN_TYPE_UNSUPPORTED",
                        "负 scale 或未知字段类型限定暂不支持",
                    )
                })?
        }
    } else {
        vec![]
    };
    Ok((base, args))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SqliteAffinity {
    Integer,
    Text,
    Blob,
    Real,
    Numeric,
}

/// SQLite 按顺序匹配声明类型；例如 FLOATING POINT 和 TEXTINT 都是 INTEGER affinity。
fn sqlite_affinity(declared: &str) -> SqliteAffinity {
    let declared = declared.to_ascii_lowercase();
    if declared.contains("int") {
        SqliteAffinity::Integer
    } else if ["char", "clob", "text"]
        .iter()
        .any(|part| declared.contains(part))
    {
        SqliteAffinity::Text
    } else if declared.is_empty() || declared.contains("blob") {
        SqliteAffinity::Blob
    } else if ["real", "floa", "doub"]
        .iter()
        .any(|part| declared.contains(part))
    {
        SqliteAffinity::Real
    } else {
        SqliteAffinity::Numeric
    }
}

/// 网格首批开放范围；比底层绑定范围更窄，新增类型须先完成实库提交验收。
pub(crate) fn grid_input_kind(driver: &str, column: &ColumnInfo) -> Option<&'static str> {
    let declared = column.data_type.to_ascii_lowercase();
    let (base, _) = type_parts(&declared).ok()?;
    match driver {
        "sqlite" => match sqlite_affinity(&declared) {
            SqliteAffinity::Integer => Some("integer"),
            SqliteAffinity::Text => Some("text"),
            SqliteAffinity::Blob => Some("binary"),
            SqliteAffinity::Real => Some("float"),
            SqliteAffinity::Numeric => None,
        },
        "postgres" | "postgresql" => match base.as_str() {
            "bigint" => Some("integer"),
            "varchar" | "text" => Some("text"),
            "character" if declared.starts_with("character varying") => Some("text"),
            "bytea" => Some("binary"),
            _ => None,
        },
        "mysql" => match base.as_str() {
            "bigint" if !declared.contains("unsigned") => Some("integer"),
            "varchar" => Some("text"),
            "varbinary" => Some("binary"),
            _ => None,
        },
        _ => None,
    }
}

/// 仅开放不会被目标 affinity 改成其他存储类型的现有绑定范围。
fn validate_sqlite_column_value(
    column: &ColumnInfo,
    value: &TypedValue,
    args: &[u32],
) -> Result<(), ContextError> {
    let affinity = sqlite_affinity(&column.data_type);
    let supported = match value {
        TypedValue::Null if !column.nullable => {
            return Err(error("NULL_NOT_ALLOWED", "非空字段不能写入 SQL NULL"));
        }
        TypedValue::Null => affinity != SqliteAffinity::Numeric,
        TypedValue::Integer(v) | TypedValue::UnsignedInteger(v) => {
            affinity == SqliteAffinity::Integer && v.parse::<i64>().is_ok()
        }
        TypedValue::Text(v) => {
            affinity == SqliteAffinity::Text
                && args
                    .first()
                    .is_none_or(|n| v.chars().count() <= *n as usize)
        }
        TypedValue::Binary(_) => {
            affinity == SqliteAffinity::Blob
                && value
                    .binary_bytes()
                    .is_ok_and(|v| args.first().is_none_or(|n| v.len() <= *n as usize))
        }
        TypedValue::Float(_) => affinity == SqliteAffinity::Real,
        _ => false,
    };
    if supported {
        Ok(())
    } else {
        Err(error(
            "COLUMN_TYPE_UNSUPPORTED",
            "SQLite 字段 affinity 不支持无损保存该类型化值",
        ))
    }
}

/// 只允许已实现无损绑定的列类型。尚未齐备的类型直接拒绝，不猜测转换。
fn validate_column_value(
    driver: &str,
    column: &ColumnInfo,
    value: &TypedValue,
) -> Result<(), ContextError> {
    value
        .validate()
        .map_err(|_| error("INVALID_TYPED_VALUE", "字段值不是合法类型化值"))?;
    let declared = column.data_type.to_ascii_lowercase();
    let (base, args) = type_parts(&declared)?;
    if driver == "sqlite" {
        return validate_sqlite_column_value(column, value, &args);
    }
    let mysql = matches!(driver, "mysql" | "mariadb");
    let pg = matches!(driver, "postgres" | "postgresql");
    let ms = matches!(driver, "mssql" | "sqlserver");
    if matches!(value, TypedValue::Null) {
        if !column.nullable {
            return Err(error("NULL_NOT_ALLOWED", "非空字段不能写入 SQL NULL"));
        }
        let supported = if ms {
            [
                "tinyint",
                "smallint",
                "int",
                "bigint",
                "bit",
                "decimal",
                "numeric",
                "real",
                "float",
                "nvarchar",
                "ntext",
                "binary",
                "varbinary",
                "image",
                "date",
                "time",
                "datetime2",
            ]
            .contains(&base.as_str())
        } else if pg {
            [
                "smallint",
                "integer",
                "bigint",
                "int2",
                "int4",
                "int8",
                "boolean",
                "bool",
                "numeric",
                "decimal",
                "real",
                "double",
                "float4",
                "float8",
                "varchar",
                "text",
                "bytea",
                "date",
                "timestamp",
                "timestamptz",
            ]
            .contains(&base.as_str())
                || declared.starts_with("character varying")
                || (base == "time" && !declared.contains("with time zone"))
        } else if mysql {
            [
                "tinyint",
                "smallint",
                "mediumint",
                "int",
                "integer",
                "bigint",
                "decimal",
                "numeric",
                "float",
                "double",
                "varchar",
                "text",
                "tinytext",
                "mediumtext",
                "longtext",
                "binary",
                "varbinary",
                "blob",
                "tinyblob",
                "mediumblob",
                "longblob",
                "date",
                "time",
                "datetime",
            ]
            .contains(&base.as_str())
        } else {
            false
        };
        return if supported {
            Ok(())
        } else {
            Err(error(
                "COLUMN_TYPE_UNSUPPORTED",
                "该字段尚不支持类型化 NULL 绑定",
            ))
        };
    }
    let integer_bits = match base.as_str() {
        "tinyint" => Some(8),
        "smallint" | "int2" => Some(16),
        "mediumint" => Some(24),
        "int" | "integer" | "int4" => Some(32),
        "bigint" | "int8" => Some(64),
        _ => None,
    };
    let supported = match value {
        TypedValue::Integer(v) | TypedValue::UnsignedInteger(v) if integer_bits.is_some() => {
            let bits = integer_bits.unwrap();
            let unsigned = (mysql && declared.contains("unsigned")) || (ms && base == "tinyint");
            let n = v.parse::<i128>().ok();
            n.is_some_and(|n| {
                if unsigned {
                    n >= 0 && n <= (1i128 << bits) - 1
                } else {
                    n >= -(1i128 << (bits - 1)) && n <= (1i128 << (bits - 1)) - 1
                }
            })
        }
        TypedValue::Boolean(_) => {
            (pg && ["boolean", "bool"].contains(&base.as_str())) || (ms && base == "bit")
        }
        TypedValue::Decimal(v) if ["decimal", "numeric"].contains(&base.as_str()) => {
            let digits = v.trim_start_matches('-');
            let (integer, fraction) = digits.split_once('.').unwrap_or((digits, ""));
            if args.is_empty() {
                pg
            } else {
                let precision = args[0] as usize;
                let scale = args.get(1).copied().unwrap_or(0) as usize;
                !(mysql && declared.contains("unsigned") && v.starts_with('-'))
                    && scale <= precision
                    && fraction.len() <= scale
                    && integer.trim_start_matches('0').len() <= precision - scale
                    && (!ms || (precision <= 38 && scale < 38))
            }
        }
        TypedValue::Float(_) if mysql && args.len() > 1 => false,
        TypedValue::Float(v) => match base.as_str() {
            "real" | "float4" => (*v as f32) as f64 == *v,
            "float" if ms && args.first().is_some_and(|n| *n <= 24) => (*v as f32) as f64 == *v,
            "float" if mysql => (*v as f32) as f64 == *v,
            "float" | "float8" | "double" => true,
            _ => false,
        },
        TypedValue::Text(v) => {
            let text = if ms {
                ["nvarchar", "ntext"].contains(&base.as_str())
            } else {
                ["varchar", "text", "tinytext", "mediumtext", "longtext"].contains(&base.as_str())
                    || (pg && declared.starts_with("character varying"))
            };
            let bytes_fit = if mysql {
                match base.as_str() {
                    "tinytext" => v.len() <= 255,
                    "text" => v.len() <= 65535,
                    _ => true,
                }
            } else {
                true
            };
            text && bytes_fit
                && (!pg || !v.contains('\0'))
                && args.first().is_none_or(|n| {
                    if ms {
                        v.encode_utf16().count() <= *n as usize
                    } else {
                        v.chars().count() <= *n as usize
                    }
                })
        }
        TypedValue::Binary(_) => {
            let binary = if pg {
                base == "bytea"
            } else {
                [
                    "binary",
                    "varbinary",
                    "blob",
                    "tinyblob",
                    "mediumblob",
                    "longblob",
                    "image",
                ]
                .contains(&base.as_str())
            };
            let bytes_fit = if mysql {
                value.binary_bytes().is_ok_and(|v| match base.as_str() {
                    "tinyblob" => v.len() <= 255,
                    "blob" => v.len() <= 65535,
                    _ => true,
                })
            } else {
                true
            };
            binary
                && bytes_fit
                && value.binary_bytes().is_ok_and(|v| {
                    args.first().is_none_or(|n| {
                        if base == "binary" {
                            v.len() == *n as usize
                        } else {
                            v.len() <= *n as usize
                        }
                    })
                })
        }
        TypedValue::Date(v) => base == "date" && (!mysql || v.as_str() >= "1000-01-01"),
        TypedValue::Time(v) => {
            base == "time"
                && !declared.contains("with time zone")
                && v.precision <= if ms { 7 } else { 6 }
                && v.precision as u32
                    <= args.first().copied().unwrap_or(if mysql {
                        0
                    } else if ms {
                        7
                    } else {
                        6
                    })
        }
        TypedValue::Timestamp(v) => {
            let zone_ok = if pg {
                if declared.contains("with time zone") || base == "timestamptz" {
                    v.timezone == TimezoneKind::Offset
                } else {
                    v.timezone == TimezoneKind::None
                }
            } else if ms && base == "datetimeoffset" {
                false
            } else {
                v.timezone == TimezoneKind::None
            };
            zone_ok
                && ((mysql && base == "datetime")
                    || (pg && ["timestamp", "timestamptz"].contains(&base.as_str()))
                    || (ms && base == "datetime2"))
                && (!mysql || v.value.as_str() >= "1000-01-01")
                && v.precision <= if ms { 7 } else { 6 }
                && v.precision as u32
                    <= args.first().copied().unwrap_or(if mysql {
                        0
                    } else if ms {
                        7
                    } else {
                        6
                    })
        }
        _ => false,
    };
    if supported {
        Ok(())
    } else {
        Err(error(
            "COLUMN_TYPE_UNSUPPORTED",
            "字段类型、数值范围或目标精度不支持该变更",
        ))
    }
}

pub(crate) fn placeholder(driver: &str, index: usize) -> String {
    match driver {
        "postgres" | "postgresql" => format!("${index}"),
        "mssql" | "sqlserver" => format!("@P{index}"),
        _ => "?".into(),
    }
}

/// 仅生成内部参数计划。公开 editPolicy 当前始终拒绝执行，调用者不得把此计划直接提交。
pub(crate) fn compile(
    driver: &dyn DatabaseDriver,
    context: &ResultContext,
    table: &ResolvedTable,
    identities: &HashMap<String, StoredRowIdentity>,
    request: &ChangeSetRequest,
) -> Result<Vec<BoundOperation>, ContextError> {
    if context.source.kind != "table"
        || table.object_kind.as_deref() != Some("table")
        || context.source.database != table.database
        || context.source.schema != table.schema
        || context.source.table != table.table
        || context.columns.len() != table.columns.len()
    {
        return Err(error(
            "SOURCE_NOT_BASE_TABLE",
            "变更必须指向原结果已验证的基础表",
        ));
    }
    let mut columns = HashMap::new();
    let mut names = HashSet::new();
    for (public, actual) in context.columns.iter().zip(&table.columns) {
        let origin = public
            .origin
            .as_ref()
            .ok_or_else(|| error("INVALID_COLUMN", "字段没有可信来源"))?;
        if origin.database != table.database
            || origin.schema != table.schema
            || origin.table != table.table
            || origin.column != actual.name
            || public.display_name != actual.name
            || public.database_type != actual.data_type
            || public.nullable != actual.nullable
            || !names.insert(actual.name.as_str())
            || columns
                .insert(public.column_id.as_str(), (public, actual))
                .is_some()
        {
            return Err(error("COLUMN_MAPPING_CHANGED", "字段映射不完整或发生变化"));
        }
    }
    let target = table_sql(driver, table);
    let kind = driver.driver_type();
    let mut operations = Vec::new();
    for op in &request.operations {
        let mut params = Vec::new();
        let mut fields = Vec::new();
        let mut expressions = Vec::new();
        if let Some(changes) = op.changes() {
            for (id, input) in changes {
                let (public, column) = columns
                    .get(id.as_str())
                    .ok_or_else(|| error("INVALID_COLUMN", "字段不属于当前结果"))?;
                let extra = column.extra.as_deref().unwrap_or("").to_ascii_lowercase();
                if public.generated
                    || extra.contains("generated")
                    || extra.contains("identity")
                    || extra.contains("auto_increment")
                    || (matches!(kind, "mssql" | "sqlserver")
                        && ["timestamp", "rowversion"]
                            .contains(&column.data_type.to_ascii_lowercase().as_str()))
                {
                    return Err(error("COLUMN_READ_ONLY", "生成列、标识列或版本列不能修改"));
                }
                // 默认值也不能绕过类型白名单；临时置可空只用于核验类型是否已有绑定实现。
                let mut type_probe = (**column).clone();
                type_probe.nullable = true;
                validate_column_value(kind, &type_probe, &TypedValue::Null)?;
                if matches!(input, CellInput::Default)
                    && matches!(op, ChangeOperation::Insert { .. })
                {
                    continue;
                }
                fields.push(quote(driver, &column.name));
                expressions.push(match input {
                    CellInput::Set { value } => {
                        validate_column_value(kind, column, value)?;
                        params.push(value.clone());
                        placeholder(kind, params.len())
                    }
                    CellInput::Default if kind == "sqlite" => {
                        return Err(error(
                            "DEFAULT_UNSUPPORTED",
                            "SQLite 不支持 UPDATE SET DEFAULT",
                        ))
                    }
                    CellInput::Default => "DEFAULT".into(),
                });
            }
            if matches!(op, ChangeOperation::Insert { .. }) {
                for (public, column) in context.columns.iter().zip(&table.columns) {
                    if !column.nullable
                        && column.default_value.is_none()
                        && !public.generated
                        && !matches!(changes.get(&public.column_id), Some(CellInput::Set { .. }))
                    {
                        return Err(error(
                            "REQUIRED_COLUMN_MISSING",
                            "插入缺少无默认值的非空字段",
                        ));
                    }
                }
            }
        }
        let predicate = if let Some(handle) = op.handle() {
            let identity = identities
                .get(handle)
                .ok_or_else(|| error("INVALID_ROW_HANDLE", "行引用不属于该结果"))?;
            let expected: Vec<_> = table.columns.iter().filter(|c| c.is_primary_key).collect();
            if expected.is_empty()
                || expected.len() != identity.keys.len()
                || identity.original_values.len() != table.columns.len()
            {
                return Err(error("INVALID_ROW_IDENTITY", "后端行身份不完整"));
            }
            let mut predicates = Vec::new();
            for (column, (name, value)) in expected.iter().zip(&identity.keys) {
                if name != &column.name || matches!(value, TypedValue::Null) {
                    return Err(error("INVALID_ROW_IDENTITY", "后端主键映射无效"));
                }
                validate_column_value(kind, column, value)?;
                params.push(value.clone());
                predicates.push(format!(
                    "{} = {}",
                    quote(driver, name),
                    placeholder(kind, params.len())
                ));
            }
            format!(" WHERE {}", predicates.join(" AND "))
        } else {
            String::new()
        };
        // 覆盖表约束自带的 REPLACE/IGNORE 策略，避免隐式删除其他行或吞掉约束冲突。
        let insert = if kind == "sqlite" {
            "INSERT OR ABORT"
        } else {
            "INSERT"
        };
        let update = if kind == "sqlite" {
            "UPDATE OR ABORT"
        } else {
            "UPDATE"
        };
        let sql = match op {
            ChangeOperation::Insert { .. } if fields.is_empty() => {
                if matches!(kind, "mysql" | "mariadb") {
                    format!("INSERT INTO {target} () VALUES ()")
                } else {
                    format!("{insert} INTO {target} DEFAULT VALUES")
                }
            }
            ChangeOperation::Insert { .. } => format!(
                "{insert} INTO {target} ({}) VALUES ({})",
                fields.join(", "),
                expressions.join(", ")
            ),
            ChangeOperation::Update { .. } => format!(
                "{update} {target} SET {}{predicate}",
                fields
                    .iter()
                    .zip(&expressions)
                    .map(|(c, p)| format!("{c} = {p}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            ChangeOperation::Delete { .. } => format!("DELETE FROM {target}{predicate}"),
        };
        crate::typed_value::validate_values(&params)
            .map_err(|_| error("INVALID_TYPED_VALUE", "变更参数无效或数量超限"))?;
        operations.push(BoundOperation {
            op_id: op.op_id().into(),
            sql,
            params,
        });
    }
    Ok(operations)
}

pub async fn validate_changeset(
    manager: &ConnectionPoolManager,
    request: ChangeSetRequest,
) -> Result<ChangeSetValidation, ContextError> {
    let handles = validate_shape(&request)?;
    let context = table_result::validate_context(
        manager,
        ContextValidationRequest {
            protocol_version: request.protocol_version,
            result_id: request.result_id.clone(),
            revision: request.revision,
            row_handles: handles.clone(),
        },
    )
    .await?;
    let snapshot = manager
        .active_connection_snapshot(&context.connection_id)
        .await?;
    if snapshot.generation != context.connection_generation {
        return Err(error("CONNECTION_CHANGED", "连接已变化，请重新读取"));
    }
    let source = ResolvedTable {
        database: context.source.database.clone(),
        schema: context.source.schema.clone(),
        table: context.source.table.clone(),
        columns: vec![],
        object_kind: Some(context.source.kind.clone()),
    };
    let table = snapshot
        .driver
        .resolve_table(
            &source.database,
            &source_parts(snapshot.driver.driver_type(), &source),
        )
        .await
        .map_err(|_| error("METADATA_UNAVAILABLE", "无法重新核验字段类型"))?;
    let fingerprint = manager.result_contexts.metadata_fingerprint(
        &context.result_id,
        context.revision,
        chrono::Utc::now().timestamp_millis(),
    )?;
    if table_result::metadata_fingerprint(&table)? != fingerprint {
        return Err(error("METADATA_CHANGED", "预检期间表结构发生变化"));
    }
    let identities = manager.validate_result_handles(&context, &handles).await?;
    let identities: HashMap<_, _> = handles.into_iter().zip(identities).collect();
    let _plan = compile(&*snapshot.driver, &context, &table, &identities, &request)?;
    // 只读预检结束再核对逻辑连接代次；实际提交仍须由后续独占会话消除竞态。
    manager.validate_result_handles(&context, &[]).await?;
    Ok(ChangeSetValidation {
        change_set_id: request.change_set_id,
        result_id: request.result_id,
        revision: request.revision,
        operation_count: request.operations.len(),
        executable: false,
        reason_codes: vec![
            "SESSION_LEASE_REQUIRED".into(),
            "OBJECT_IDENTITY_REQUIRED".into(),
            "COMMIT_RECEIPT_REQUIRED".into(),
            "CREDENTIAL_RECOVERY_REQUIRED".into(),
            "DRIVER_WRITE_VALIDATION_REQUIRED".into(),
        ],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::sqlite::SqliteDriver;
    use crate::models::ConnectionConfig;
    use crate::result_context::{ColumnOrigin, EditPolicy, ResultColumn, ResultSource};
    use crate::typed_value::{TimeValue, TimestampValue};
    use serde_json::json;

    fn column(name: &str, data_type: &str, nullable: bool, primary: bool) -> ColumnInfo {
        ColumnInfo {
            name: name.into(),
            data_type: data_type.into(),
            nullable,
            is_primary_key: primary,
            default_value: None,
            extra: None,
            comment: None,
        }
    }

    fn fixture() -> (
        ResultContext,
        ResolvedTable,
        HashMap<String, StoredRowIdentity>,
        String,
    ) {
        let source = ResultSource {
            database: ":memory:".into(),
            schema: Some("main".into()),
            table: "a.table\"name".into(),
            kind: "table".into(),
        };
        let table = ResolvedTable {
            database: source.database.clone(),
            schema: source.schema.clone(),
            table: source.table.clone(),
            columns: vec![
                column("id", "INTEGER", false, true),
                column("pay\"load", "TEXT", true, false),
            ],
            object_kind: Some("table".into()),
        };
        let context = ResultContext {
            protocol_version: 1,
            result_id: Uuid::new_v4().to_string(),
            revision: 1,
            connection_id: "c".into(),
            connection_generation: Uuid::new_v4().to_string(),
            recovery_identity: None,
            columns: table
                .columns
                .iter()
                .map(|c| ResultColumn {
                    column_id: Uuid::new_v4().to_string(),
                    display_name: c.name.clone(),
                    database_type: c.data_type.clone(),
                    nullable: c.nullable,
                    origin: Some(ColumnOrigin {
                        database: source.database.clone(),
                        schema: source.schema.clone(),
                        table: source.table.clone(),
                        column: c.name.clone(),
                    }),
                    writable: false,
                    generated: false,
                })
                .collect(),
            source,
            edit_policy: EditPolicy {
                allowed_operations: vec![],
                reason_codes: vec!["SESSION_LEASE_REQUIRED".into()],
                column_input_kinds: HashMap::new(),
            },
            created_at: 0,
            expires_at: 100,
        };
        let handle = Uuid::new_v4().to_string();
        let identities = HashMap::from([(
            handle.clone(),
            StoredRowIdentity {
                keys: vec![("id".into(), TypedValue::Integer("9007199254740993".into()))],
                original_values: vec![
                    Some(TypedValue::Integer("9007199254740993".into())),
                    Some(TypedValue::Text("旧值".into())),
                ],
            },
        )]);
        (context, table, identities, handle)
    }

    async fn driver() -> SqliteDriver {
        SqliteDriver::connect(&ConnectionConfig {
            id: None,
            name: "预检单测".into(),
            driver_type: "sqlite".into(),
            host: "".into(),
            port: 0,
            user: "".into(),
            password: "".into(),
            database: Some(":memory:".into()),
            encrypt: None,
            save_password: Some(false),
            group_name: None,
            color: None,
            mcp_allowed: Some(false),
        })
        .await
        .unwrap()
    }

    fn request(context: &ResultContext, op: ChangeOperation) -> ChangeSetRequest {
        ChangeSetRequest {
            protocol_version: 1,
            change_set_id: Uuid::new_v4().to_string(),
            result_id: context.result_id.clone(),
            revision: 1,
            operations: vec![op],
        }
    }

    fn update(context: &ResultContext, handle: &str, input: CellInput) -> ChangeSetRequest {
        request(
            context,
            ChangeOperation::Update {
                op_id: Uuid::new_v4().to_string(),
                row_handle: handle.into(),
                changes: BTreeMap::from([(context.columns[1].column_id.clone(), input)]),
            },
        )
    }

    #[tokio::test]
    async fn compiled_values_are_only_parameters_and_original_identity_is_fixed() {
        let (context, table, identities, handle) = fixture();
        let payload = "中文'\\\0; DELETE FROM users; --";
        let request = update(
            &context,
            &handle,
            CellInput::Set {
                value: TypedValue::Text(payload.into()),
            },
        );
        validate_shape(&request).unwrap();
        let plan = compile(&driver().await, &context, &table, &identities, &request).unwrap();
        assert_eq!(
            plan[0].sql,
            "UPDATE OR ABORT \"main\".\"a.table\"\"name\" SET \"pay\"\"load\" = ? WHERE \"id\" = ?"
        );
        assert_eq!(
            plan[0].params,
            vec![
                TypedValue::Text(payload.into()),
                TypedValue::Integer("9007199254740993".into())
            ]
        );
        assert!(!plan[0].sql.contains(payload));
        assert!(!context.columns[1].writable);
        assert!(context.edit_policy.allowed_operations.is_empty());
        assert_eq!(placeholder("postgres", 2), "$2");
        assert_eq!(placeholder("mssql", 2), "@P2");
    }

    #[tokio::test]
    async fn field_whitelist_origin_and_generated_columns_cannot_be_overridden() {
        let (mut context, table, identities, handle) = fixture();
        let driver = driver().await;
        let mut request = update(
            &context,
            &handle,
            CellInput::Set {
                value: TypedValue::Text("new".into()),
            },
        );
        if let ChangeOperation::Update { changes, .. } = &mut request.operations[0] {
            let value = changes.pop_first().unwrap().1;
            changes.insert(Uuid::new_v4().to_string(), value);
        }
        assert_eq!(
            compile(&driver, &context, &table, &identities, &request)
                .unwrap_err()
                .code,
            "INVALID_COLUMN"
        );
        let request = update(&context, &handle, CellInput::Default);
        context.columns[1].generated = true;
        assert_eq!(
            compile(&driver, &context, &table, &identities, &request)
                .unwrap_err()
                .code,
            "COLUMN_READ_ONLY"
        );
        context.columns[1].generated = false;
        context.columns[1].origin.as_mut().unwrap().table = "other".into();
        assert_eq!(
            compile(&driver, &context, &table, &identities, &request)
                .unwrap_err()
                .code,
            "COLUMN_MAPPING_CHANGED"
        );
    }

    #[tokio::test]
    async fn default_is_insert_omission_and_not_a_bound_string() {
        let (context, mut table, identities, handle) = fixture();
        let driver = driver().await;
        let request = update(&context, &handle, CellInput::Default);
        assert_eq!(
            compile(&driver, &context, &table, &identities, &request)
                .unwrap_err()
                .code,
            "DEFAULT_UNSUPPORTED"
        );
        let request = request_from_json(
            &context,
            json!({"kind":"insert","opId":Uuid::new_v4().to_string(),"changes":{}}),
        );
        assert_eq!(
            compile(&driver, &context, &table, &identities, &request)
                .unwrap_err()
                .code,
            "REQUIRED_COLUMN_MISSING"
        );
        table.columns[0].default_value = Some("42".into());
        let plan = compile(&driver, &context, &table, &identities, &request).unwrap();
        assert_eq!(
            plan[0].sql,
            "INSERT OR ABORT INTO \"main\".\"a.table\"\"name\" DEFAULT VALUES"
        );
        assert!(plan[0].params.is_empty());
    }

    fn request_from_json(context: &ResultContext, op: serde_json::Value) -> ChangeSetRequest {
        serde_json::from_value(json!({"protocolVersion":1,"changeSetId":Uuid::new_v4().to_string(),"resultId":context.result_id,"revision":1,"operations":[op]})).unwrap()
    }

    #[test]
    fn protocol_rejects_client_source_sql_unknown_tags_and_duplicate_fields() {
        let (context, _, _, handle) = fixture();
        let request = update(
            &context,
            &handle,
            CellInput::Set {
                value: TypedValue::Null,
            },
        );
        let valid = serde_json::to_value(&request).unwrap();
        for field in [
            "sql",
            "connectionId",
            "table",
            "source",
            "primaryKeys",
            "connectionGeneration",
        ] {
            let mut value = valid.clone();
            value[field] = json!("injected");
            assert!(
                serde_json::from_value::<ChangeSetRequest>(value).is_err(),
                "{field}"
            );
        }
        let duplicate = format!("{{\"protocolVersion\":1,\"changeSetId\":\"{}\",\"resultId\":\"{}\",\"revision\":1,\"operations\":[{{\"kind\":\"insert\",\"opId\":\"{}\",\"changes\":{{\"{}\":{{\"mode\":\"default\"}},\"{}\":{{\"mode\":\"default\"}}}}}}]}}",request.change_set_id,context.result_id,Uuid::new_v4(),context.columns[0].column_id,context.columns[0].column_id);
        assert!(serde_json::from_str::<ChangeSetRequest>(&duplicate).is_err());
        let mut duplicated = request.clone();
        duplicated.operations.push(request.operations[0].clone());
        assert!(validate_shape(&duplicated).is_err());
        if let ChangeOperation::Update { op_id, .. } = &mut duplicated.operations[1] {
            *op_id = Uuid::new_v4().to_string();
        }
        assert_eq!(
            validate_shape(&duplicated).unwrap_err().code,
            "INVALID_ROW_HANDLE"
        );
    }

    #[test]
    fn target_types_reject_rounding_range_loss_and_unsupported_conversions() {
        let check = |driver: &str, typ: &str, value: TypedValue| {
            validate_column_value(driver, &column("c", typ, false, false), &value)
        };
        assert!(check(
            "mysql",
            "bigint unsigned",
            TypedValue::UnsignedInteger(u64::MAX.to_string())
        )
        .is_ok());
        assert!(check(
            "postgres",
            "bigint",
            TypedValue::UnsignedInteger(u64::MAX.to_string())
        )
        .is_err());
        assert!(check("mssql", "tinyint", TypedValue::Integer("256".into())).is_err());
        assert!(check(
            "postgres",
            "numeric(5,2)",
            TypedValue::Decimal("123.45".into())
        )
        .is_ok());
        assert!(check(
            "postgres",
            "numeric(3,-1)",
            TypedValue::Decimal("123".into())
        )
        .is_err());
        assert!(check("postgres", "numeric(3,-1)", TypedValue::Null).is_err());
        assert!(check(
            "mysql",
            "decimal(10,2) unsigned",
            TypedValue::Decimal("-1.25".into())
        )
        .is_err());
        assert!(check(
            "mysql",
            "decimal(10,2) unsigned",
            TypedValue::Decimal("1.25".into())
        )
        .is_ok());
        assert!(check("mysql", "tinytext", TypedValue::Text("中".repeat(86))).is_err());
        assert!(check("mysql", "tinyblob", TypedValue::binary(&vec![0; 256])).is_err());
        for value in ["1234.56", "123.456"] {
            assert!(check(
                "postgres",
                "numeric(5,2)",
                TypedValue::Decimal(value.into())
            )
            .is_err());
        }
        assert!(check("sqlite", "NUMERIC", TypedValue::Decimal("123.45".into())).is_err());
        assert!(check("postgres", "text", TypedValue::Text("NULL".into())).is_ok());
        assert!(check("postgres", "text", TypedValue::Null).is_err());
        assert!(check("postgres", "text", TypedValue::Text("nul\0byte".into())).is_err());
        assert!(check("mssql", "varchar(10)", TypedValue::Text("中文".into())).is_err());
        assert!(check("mssql", "nvarchar(1)", TypedValue::Text("😀".into())).is_err());
        assert!(check("mssql", "binary(3)", TypedValue::binary(&[0, 1])).is_err());
        assert!(check("mssql", "varbinary(3)", TypedValue::binary(&[0, 1])).is_ok());
        assert!(check("postgres", "real", TypedValue::Float(0.1)).is_err());
        assert!(check("postgres", "real", TypedValue::Float(0.5)).is_ok());
        assert!(check(
            "mysql",
            "time",
            TypedValue::Time(TimeValue {
                value: "12:00:00.123".into(),
                precision: 3
            })
        )
        .is_err());
        assert!(check(
            "mysql",
            "time(3)",
            TypedValue::Time(TimeValue {
                value: "12:00:00.123".into(),
                precision: 3
            })
        )
        .is_ok());
        assert!(check(
            "postgres",
            "timestamp(3) without time zone",
            TypedValue::Timestamp(TimestampValue {
                value: "2024-01-01T00:00:00.123".into(),
                precision: 3,
                timezone: TimezoneKind::None
            })
        )
        .is_ok());
        assert!(check(
            "mssql",
            "datetimeoffset(7)",
            TypedValue::Timestamp(TimestampValue {
                value: "2024-01-01T00:00:00Z".into(),
                precision: 0,
                timezone: TimezoneKind::Offset
            })
        )
        .is_err());
        assert!(check("mysql", "date", TypedValue::Date("0001-01-01".into())).is_err());
    }

    #[test]
    fn sqlite_affinity_order_drives_one_lossless_type_whitelist() {
        for declared in ["TEXTINT", "VARCHARINT", "FLOATING POINT", "DOUBLE INT"] {
            assert_eq!(sqlite_affinity(declared), SqliteAffinity::Integer);
            let column = column("value", declared, true, false);
            for value in [
                TypedValue::Text("001".into()),
                TypedValue::Float(1.0),
                TypedValue::binary(&[1]),
            ] {
                assert_eq!(
                    validate_column_value("sqlite", &column, &value)
                        .unwrap_err()
                        .code,
                    "COLUMN_TYPE_UNSUPPORTED"
                );
            }
            assert!(validate_column_value(
                "sqlite",
                &column,
                &TypedValue::Integer(i64::MAX.to_string())
            )
            .is_ok());
            assert!(validate_column_value(
                "sqlite",
                &column,
                &TypedValue::UnsignedInteger(u64::MAX.to_string())
            )
            .is_err());
        }
        for (declared, affinity, value) in [
            ("TEXT", SqliteAffinity::Text, TypedValue::Text("001".into())),
            (
                "CHARBLOB",
                SqliteAffinity::Text,
                TypedValue::Text("001".into()),
            ),
            ("BLOB", SqliteAffinity::Blob, TypedValue::binary(&[0, 255])),
            ("", SqliteAffinity::Blob, TypedValue::binary(&[0, 255])),
            (
                "BLOBREAL",
                SqliteAffinity::Blob,
                TypedValue::binary(&[0, 255]),
            ),
            ("REAL", SqliteAffinity::Real, TypedValue::Float(1.0)),
            (
                "DOUBLE PRECISION",
                SqliteAffinity::Real,
                TypedValue::Float(0.1),
            ),
        ] {
            assert_eq!(sqlite_affinity(declared), affinity);
            assert!(validate_column_value(
                "sqlite",
                &column("value", declared, true, false),
                &value
            )
            .is_ok());
        }
        assert_eq!(sqlite_affinity("NUMERIC"), SqliteAffinity::Numeric);
        assert!(validate_column_value(
            "sqlite",
            &column("value", "NUMERIC", true, false),
            &TypedValue::Null
        )
        .is_err());
    }
}
