//! 查询结果预算：逐行检查，拒绝整行，不裁剪单元格或改变真实值。
use crate::models::{ColumnMetadata, DatabaseError, RowData};
use crate::typed_value::{TypedValue, MAX_CELL_BYTES};
use serde::Serialize;
use serde_json::Value;
use std::io::{self, Write};
use std::mem::size_of;

pub const MAX_QUERY_ROWS: usize = 10_000;
pub const MAX_QUERY_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_QUERY_CELL_BYTES: usize = MAX_CELL_BYTES;

pub(crate) struct QueryBudget {
    rows: usize,
    bytes: usize,
    columns: usize,
    max_rows: usize,
    max_bytes: usize,
}

/// 只计数，不先构造完整 JSON 字符串；达到预算立即停止编码。
struct ByteCounter {
    bytes: usize,
    limit: usize,
    exceeded: bool,
}
impl Write for ByteCounter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.bytes) {
            self.exceeded = true;
            return Err(io::Error::other("查询结果字节预算已用尽"));
        }
        self.bytes += bytes.len();
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn encoded_bytes(value: &impl Serialize, limit: usize) -> Result<Option<usize>, DatabaseError> {
    let mut counter = ByteCounter {
        bytes: 0,
        limit,
        exceeded: false,
    };
    match serde_json::to_writer(&mut counter, value) {
        Ok(()) => Ok(Some(counter.bytes)),
        Err(_) if counter.exceeded => Ok(None),
        Err(_) => Err(DatabaseError::QueryError(
            "RESULT_ENCODING_FAILED: 结果无法编码".into(),
        )),
    }
}

fn json_heap_bytes(value: &Value) -> usize {
    match value {
        Value::String(text) => text.capacity(),
        Value::Array(values) => values.iter().fold(
            values.capacity().saturating_mul(size_of::<Value>()),
            |sum, value| sum.saturating_add(json_heap_bytes(value)),
        ),
        Value::Object(values) => values.iter().fold(0usize, |sum, (key, value)| {
            // 包含键值节点与保守的树指针开销；预算不是进程 RSS 硬上限。
            sum.saturating_add(size_of::<(String, Value)>() + 4 * size_of::<usize>())
                .saturating_add(key.capacity())
                .saturating_add(json_heap_bytes(value))
        }),
        _ => 0,
    }
}

impl QueryBudget {
    pub(crate) fn new(columns: &[ColumnMetadata]) -> Result<Self, DatabaseError> {
        Self::with_limits(columns, MAX_QUERY_ROWS, MAX_QUERY_BYTES)
    }

    fn with_limits(
        columns: &[ColumnMetadata],
        max_rows: usize,
        max_bytes: usize,
    ) -> Result<Self, DatabaseError> {
        let retained = columns
            .iter()
            .fold(
                columns.len().saturating_mul(size_of::<ColumnMetadata>()),
                |sum, column| {
                    sum.saturating_add(column.name.capacity())
                        .saturating_add(column.data_type.capacity())
                        .saturating_add(column.unified_type.capacity())
                },
            )
            .saturating_add(512);
        let encoded =
            encoded_bytes(&columns, max_bytes.saturating_sub(retained))?.ok_or_else(|| {
                DatabaseError::QueryError("RESULT_METADATA_LIMIT: 列元数据超出查询预算".into())
            })?;
        let bytes = retained.saturating_add(encoded);
        if bytes > max_bytes {
            return Err(DatabaseError::QueryError(
                "RESULT_METADATA_LIMIT: 列元数据超出查询预算".into(),
            ));
        }
        Ok(Self {
            rows: 0,
            bytes,
            columns: columns.len(),
            max_rows,
            max_bytes,
        })
    }

    pub(crate) fn row_limit_reached(&self) -> bool {
        self.rows >= self.max_rows
    }

    /// 在 JSON、hex、Base64 或无损值复制之前检查原始载荷；不分配额外的载荷副本。
    pub(crate) fn allows_raw_row(&self, lengths: impl IntoIterator<Item = usize>) -> bool {
        let remaining = self.max_bytes.saturating_sub(self.bytes);
        let mut total = 0usize;
        for length in lengths {
            if length > MAX_QUERY_CELL_BYTES || length > remaining.saturating_sub(total) {
                return false;
            }
            total += length;
        }
        true
    }

    /// 只有返回 true 后，调用方才能同时保留显示行和 typed 行；失败不消耗预算。
    pub(crate) fn try_add_row(
        &mut self,
        row: &RowData,
        typed: Option<&[Option<TypedValue>]>,
    ) -> Result<bool, DatabaseError> {
        if row.values.len() != self.columns
            || typed.is_some_and(|values| values.len() != self.columns)
        {
            return Err(DatabaseError::QueryError(
                "RESULT_COLUMN_MISMATCH: 查询行与列映射不一致".into(),
            ));
        }
        if self.row_limit_reached() {
            return Ok(false);
        }
        let mut remaining = self.max_bytes.saturating_sub(self.bytes);
        let mut cost = row.values.iter().flatten().fold(
            2 * size_of::<RowData>()
                + row
                    .values
                    .capacity()
                    .saturating_mul(size_of::<Option<Value>>()),
            |sum, value| sum.saturating_add(json_heap_bytes(value)),
        );
        if let Some(values) = typed {
            cost = values.iter().flatten().fold(
                cost.saturating_add(2 * size_of::<Vec<Option<TypedValue>>>())
                    .saturating_add(values.len().saturating_mul(size_of::<Option<TypedValue>>())),
                |sum, value| sum.saturating_add(value.allocated_bytes()),
            );
        }
        if cost > remaining {
            return Ok(false);
        }
        remaining -= cost;
        let Some(display_bytes) = encoded_bytes(row, remaining)? else {
            return Ok(false);
        };
        cost += display_bytes;
        remaining -= display_bytes;
        if let Some(values) = typed {
            let Some(typed_bytes) = encoded_bytes(&values, remaining)? else {
                return Ok(false);
            };
            cost += typed_bytes;
        }
        self.bytes += cost;
        self.rows += 1;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn columns() -> Vec<ColumnMetadata> {
        vec![ColumnMetadata {
            name: "value".into(),
            data_type: "text".into(),
            unified_type: "Unknown".into(),
            nullable: true,
            length: None,
            precision: None,
            scale: None,
        }]
    }
    fn row(value: Value) -> RowData {
        RowData {
            values: vec![Some(value)],
        }
    }

    #[test]
    fn row_limit_accepts_exact_boundary_and_rejection_does_not_consume_budget() {
        let mut budget = QueryBudget::with_limits(&columns(), 2, MAX_QUERY_BYTES).unwrap();
        assert!(budget.try_add_row(&row(json!(1)), None).unwrap());
        assert!(budget.try_add_row(&row(json!(2)), None).unwrap());
        let bytes = budget.bytes;
        assert!(budget.row_limit_reached());
        assert!(!budget.try_add_row(&row(json!(3)), None).unwrap());
        assert_eq!(budget.rows, 2);
        assert_eq!(budget.bytes, bytes);
    }

    #[test]
    fn byte_boundary_counts_escaping_and_typed_copy_without_partial_rows() {
        let row = row(json!("\0\"\\中".repeat(100)));
        let typed = vec![Some(TypedValue::Text("\0\"\\中".repeat(100)))];
        let mut measured = QueryBudget::new(&columns()).unwrap();
        let initial = measured.bytes;
        assert!(measured.try_add_row(&row, Some(&typed)).unwrap());
        let exact = measured.bytes;
        let mut fit = QueryBudget::with_limits(&columns(), 10, exact).unwrap();
        assert!(fit.try_add_row(&row, Some(&typed)).unwrap());
        let mut short = QueryBudget::with_limits(&columns(), 10, exact - 1).unwrap();
        assert!(!short.try_add_row(&row, Some(&typed)).unwrap());
        assert_eq!(short.rows, 0);
        assert_eq!(short.bytes, initial);
        let mut plain = QueryBudget::new(&columns()).unwrap();
        assert!(plain.try_add_row(&row, None).unwrap());
        assert!(plain.bytes < exact);
    }

    #[test]
    fn raw_cells_and_remaining_bytes_are_checked_before_conversion() {
        let budget = QueryBudget::new(&columns()).unwrap();
        assert!(budget.allows_raw_row([MAX_QUERY_CELL_BYTES]));
        assert!(!budget.allows_raw_row([MAX_QUERY_CELL_BYTES + 1]));
        assert!(!budget.allows_raw_row(std::iter::repeat_n(MAX_QUERY_CELL_BYTES, 17)));
        assert!(!budget.allows_raw_row([usize::MAX]));
    }

    #[test]
    fn metadata_and_row_mapping_cannot_bypass_budget() {
        assert!(QueryBudget::with_limits(&columns(), 10, 32).is_err());
        let mut budget = QueryBudget::new(&columns()).unwrap();
        assert!(budget
            .try_add_row(&RowData { values: vec![] }, None)
            .is_err());
        assert!(budget.try_add_row(&row(json!(1)), Some(&[])).is_err());
        let value = row(json!({"nested": ["x".repeat(1000)]}));
        assert!(budget.try_add_row(&value, None).unwrap());
        assert!(budget.bytes > 2000);
    }
}
