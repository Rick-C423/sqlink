//! 无损单元格协议；DEFAULT 由变更意图单独表达，不能作为数据库读取值。

use crate::models::{integer_display_json, DatabaseError, QueryResult};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde::{Deserialize, Serialize};

pub const MAX_CELL_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "camelCase",
    deny_unknown_fields
)]
pub enum TypedValue {
    Null,
    Text(String),
    Boolean(bool),
    #[serde(rename = "int64")]
    Integer(String),
    #[serde(rename = "uint64")]
    UnsignedInteger(String),
    Decimal(String),
    Float(f64),
    Binary(String),
    Date(String),
    Time(TimeValue),
    Timestamp(TimestampValue),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TimeValue {
    pub value: String,
    pub precision: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TimezoneKind {
    None,
    Offset,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TimestampValue {
    pub value: String,
    pub precision: u8,
    pub timezone: TimezoneKind,
}

/// typed_rows 中 None 表示该驱动无法无损解码；SQL NULL 必须是 Some(Null)。
#[derive(Debug)]
pub struct TypedQueryResult {
    pub result: QueryResult,
    pub typed_rows: Vec<Vec<Option<TypedValue>>>,
}

fn invalid(message: &str) -> DatabaseError {
    DatabaseError::TypeError(message.into())
}

impl TypedValue {
    pub fn binary(bytes: &[u8]) -> Self {
        Self::Binary(STANDARD.encode(bytes))
    }

    pub fn binary_bytes(&self) -> Result<Vec<u8>, DatabaseError> {
        match self {
            Self::Binary(value) => {
                let bytes = STANDARD
                    .decode(value)
                    .map_err(|_| invalid("二进制载荷必须是规范 Base64"))?;
                if bytes.len() > MAX_CELL_BYTES || STANDARD.encode(&bytes) != *value {
                    return Err(invalid("二进制载荷格式或大小不受支持"));
                }
                Ok(bytes)
            }
            _ => Err(invalid("该值不是二进制")),
        }
    }

    pub fn validate(&self) -> Result<(), DatabaseError> {
        match self {
            Self::Null | Self::Boolean(_) => Ok(()),
            Self::Text(value) if value.len() <= MAX_CELL_BYTES => Ok(()),
            Self::Integer(value) => value
                .parse::<i64>()
                .ok()
                .filter(|n| n.to_string() == *value)
                .map(|_| ())
                .ok_or_else(|| invalid("int64 必须是范围内的规范十进制字符串")),
            Self::UnsignedInteger(value) => value
                .parse::<u64>()
                .ok()
                .filter(|n| n.to_string() == *value)
                .map(|_| ())
                .ok_or_else(|| invalid("uint64 必须是范围内的规范十进制字符串")),
            Self::Decimal(value) if valid_decimal(value) => Ok(()),
            Self::Float(value) if value.is_finite() => Ok(()),
            Self::Binary(value) if value.len() <= MAX_CELL_BYTES.div_ceil(3) * 4 => {
                self.binary_bytes().map(|_| ())
            }
            Self::Date(value) if valid_date(value) => Ok(()),
            Self::Time(value) if valid_time(&value.value, value.precision) => Ok(()),
            Self::Timestamp(value) if valid_timestamp(value) => Ok(()),
            _ => Err(invalid("类型化值格式、精度或大小不受支持")),
        }
    }

    /// 仅供界面显示；身份和后续参数绑定始终使用 TypedValue 原值。
    pub fn display_json(&self) -> serde_json::Value {
        use serde_json::Value;
        match self {
            Self::Null => Value::Null,
            Self::Boolean(value) => Value::Bool(*value),
            Self::Float(value) => serde_json::json!(value),
            Self::Integer(value) => value
                .parse::<i64>()
                .ok()
                .map(|value| integer_display_json(&value.into()))
                .unwrap_or_else(|| Value::String(value.clone())),
            Self::UnsignedInteger(value) => value
                .parse::<u64>()
                .ok()
                .map(|value| integer_display_json(&value.into()))
                .unwrap_or_else(|| Value::String(value.clone())),
            Self::Time(value) => Value::String(value.value.clone()),
            Self::Timestamp(value) => Value::String(value.value.clone()),
            Self::Binary(_) => Value::String(
                self.binary_bytes()
                    .map(|bytes| {
                        format!(
                            "\\x{}",
                            bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
                        )
                    })
                    .unwrap_or_default(),
            ),
            Self::Text(value) | Self::Decimal(value) | Self::Date(value) => {
                Value::String(value.clone())
            }
        }
    }

    pub fn allocated_bytes(&self) -> usize {
        match self {
            Self::Text(v)
            | Self::Integer(v)
            | Self::UnsignedInteger(v)
            | Self::Decimal(v)
            | Self::Binary(v)
            | Self::Date(v) => v.capacity(),
            Self::Time(v) => v.value.capacity(),
            Self::Timestamp(v) => v.value.capacity(),
            _ => 0,
        }
    }
}

pub fn validate_values(values: &[TypedValue]) -> Result<(), DatabaseError> {
    if values.len() > 2000 {
        return Err(invalid("单条语句参数数量超限"));
    }
    for value in values {
        value.validate()?;
    }
    Ok(())
}

pub fn valid_decimal(value: &str) -> bool {
    if value.len() > 1024 {
        return false;
    }
    let unsigned = value.strip_prefix('-').unwrap_or(value);
    let mut parts = unsigned.split('.');
    let integer = parts.next().unwrap_or("");
    let fraction = parts.next();
    !integer.is_empty()
        && integer.bytes().all(|b| b.is_ascii_digit())
        && (integer == "0" || !integer.starts_with('0'))
        && fraction.is_none_or(|v| !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()))
        && parts.next().is_none()
        && !(value.starts_with('-') && unsigned.bytes().all(|b| b == b'0' || b == b'.'))
}

fn valid_date(value: &str) -> bool {
    value.len() == 10
        && value.as_bytes()[4] == b'-'
        && value.as_bytes()[7] == b'-'
        && value
            .bytes()
            .enumerate()
            .all(|(i, b)| i == 4 || i == 7 || b.is_ascii_digit())
        && chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d").is_ok()
        && !value.starts_with("0000")
}

fn valid_time(value: &str, precision: u8) -> bool {
    if precision > 9
        || value.len()
            != 8 + if precision == 0 {
                0
            } else {
                1 + precision as usize
            }
    {
        return false;
    }
    let bytes = value.as_bytes();
    bytes[2] == b':'
        && bytes[5] == b':'
        && (precision == 0 || bytes[8] == b'.')
        && bytes
            .iter()
            .enumerate()
            .all(|(i, b)| i == 2 || i == 5 || (precision != 0 && i == 8) || b.is_ascii_digit())
        && value[0..2].parse::<u8>().is_ok_and(|v| v < 24)
        && value[3..5].parse::<u8>().is_ok_and(|v| v < 60)
        && value[6..8].parse::<u8>().is_ok_and(|v| v < 60)
}

fn valid_timestamp(value: &TimestampValue) -> bool {
    if value.value.len() < 19 || !value.value.is_ascii() {
        return false;
    }
    let (date, rest) = value.value.split_at(10);
    if !valid_date(date) || !rest.starts_with('T') {
        return false;
    }
    let rest = &rest[1..];
    match value.timezone {
        TimezoneKind::None => valid_time(rest, value.precision),
        TimezoneKind::Offset => {
            let time = if let Some(time) = rest.strip_suffix('Z') {
                time
            } else if rest.len() >= 6 && matches!(rest.as_bytes()[rest.len() - 6], b'+' | b'-') {
                &rest[..rest.len() - 6]
            } else {
                return false;
            };
            valid_time(time, value.precision)
                && chrono::DateTime::parse_from_rfc3339(&value.value).is_ok()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn integer_and_decimal_protocol_never_uses_json_number() {
        for value in [
            TypedValue::Integer("9007199254740993".into()),
            TypedValue::Integer(i64::MIN.to_string()),
            TypedValue::UnsignedInteger(u64::MAX.to_string()),
            TypedValue::Decimal("12345678901234567890.1234500".into()),
        ] {
            value.validate().unwrap();
            let json = serde_json::to_value(&value).unwrap();
            assert!(json["value"].is_string());
            assert_eq!(serde_json::from_value::<TypedValue>(json).unwrap(), value);
        }
        assert!(serde_json::from_value::<TypedValue>(
            json!({"kind":"int64","value":9007199254740993u64})
        )
        .is_err());
        for invalid in ["+1", "01", "-0", "1e2", "9223372036854775808", "1.0", " 1"] {
            assert!(
                TypedValue::Integer(invalid.into()).validate().is_err(),
                "{invalid}"
            );
        }
        for invalid in [
            "1e2", "+1", "01.2", ".2", "1.", "-0", "-0.000", "NaN", "Infinity",
        ] {
            assert!(
                TypedValue::Decimal(invalid.into()).validate().is_err(),
                "{invalid}"
            );
        }
        assert!(TypedValue::UnsignedInteger("18446744073709551616".into())
            .validate()
            .is_err());
        assert!(TypedValue::UnsignedInteger("-1".into()).validate().is_err());
    }

    #[test]
    fn null_empty_text_binary_and_default_are_distinct() {
        let values = [
            TypedValue::Null,
            TypedValue::Text("NULL".into()),
            TypedValue::Text("".into()),
            TypedValue::binary(&[]),
            TypedValue::binary(&[0, 255, 0]),
            TypedValue::Integer("0".into()),
        ];
        let encoded: std::collections::HashSet<_> = values
            .iter()
            .map(|v| serde_json::to_string(v).unwrap())
            .collect();
        assert_eq!(encoded.len(), values.len());
        assert_eq!(values[4].binary_bytes().unwrap(), vec![0, 255, 0]);
        for invalid in ["YQ", "YR==", " YQ==", "YQ==\n", "!!!!"] {
            assert!(TypedValue::Binary(invalid.into()).validate().is_err());
        }
        assert!(TypedValue::binary(&vec![0; MAX_CELL_BYTES + 1])
            .validate()
            .is_err());
        assert!(TypedValue::Text("中".repeat(MAX_CELL_BYTES / 3 + 1))
            .validate()
            .is_err());
        assert!(serde_json::from_value::<TypedValue>(json!({"kind":"default"})).is_err());
        assert!(serde_json::from_value::<TypedValue>(
            json!({"kind":"text","value":"x","sql":"injected"})
        )
        .is_err());
        assert!(TypedValue::Float(f64::NAN).validate().is_err());
        assert!(TypedValue::Float(f64::INFINITY).validate().is_err());
    }

    #[test]
    fn temporal_values_require_exact_precision_and_timezone() {
        for date in ["2024-02-29", "0001-01-01", "9999-12-31"] {
            TypedValue::Date(date.into()).validate().unwrap();
        }
        for date in [
            "2023-02-29",
            "0000-01-01",
            "2024-1-01",
            "2024-01-01T00:00:00",
        ] {
            assert!(TypedValue::Date(date.into()).validate().is_err());
        }
        let time = TypedValue::Time(TimeValue {
            value: "12:34:56.123456789".into(),
            precision: 9,
        });
        time.validate().unwrap();
        for (value, precision) in [
            ("24:00:00", 0),
            ("23:59:60", 0),
            ("12:34:56.123", 2),
            ("12:34:56Z", 0),
            ("12:34:56.0000000000", 10),
        ] {
            assert!(TypedValue::Time(TimeValue {
                value: value.into(),
                precision
            })
            .validate()
            .is_err());
        }
        for (value, timezone, valid) in [
            ("2024-02-29T12:34:56.123+08:00", TimezoneKind::Offset, true),
            ("2024-02-29T12:34:56.123Z", TimezoneKind::Offset, true),
            ("2024-02-29T12:34:56.123", TimezoneKind::None, true),
            ("2024-02-29T12:34:56.123Z", TimezoneKind::None, false),
            ("2024-02-29T12:34:56.123", TimezoneKind::Offset, false),
            ("2024-02-29T12:34:56.123+24:00", TimezoneKind::Offset, false),
        ] {
            assert_eq!(
                TypedValue::Timestamp(TimestampValue {
                    value: value.into(),
                    precision: 3,
                    timezone
                })
                .validate()
                .is_ok(),
                valid
            );
        }
    }
}
