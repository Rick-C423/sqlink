import type { ColumnMetadata, RowData } from "@/types/database";

type CellValue = RowData["values"][number] | undefined;

function integerColumn(column: ColumnMetadata): boolean {
  const type = column.dataType.trim().toLowerCase().replace(/\s+/g, " ");
  // 只使用明确的整数元数据；数字文本、未知类型和表达式不能凭外观改按整数排序。
  return /^(?:tinyint|smallint|mediumint|int|integer|bigint|int2|int4|int8|intn)(?:\([0-9]+\))?(?: unsigned)?$/.test(type) ||
    type === "unsigned big int" || /^mysql_type_(?:tiny|short|long|longlong|int24)$/.test(type);
}

function exactInteger(value: CellValue): bigint | null {
  if (typeof value === "number") return Number.isSafeInteger(value) ? BigInt(value) : null;
  if (typeof value !== "string" || value.length > 20 || !/^-?(?:0|[1-9][0-9]*)$/.test(value)) return null;
  return BigInt(value);
}

/** 本地排序不将大整数字符串转回 Number；NULL 沿用升降序均置后的规则。 */
export function compareGridValues(column: ColumnMetadata, a: CellValue, b: CellValue, direction: "asc" | "desc"): number {
  if (a == null && b == null) return 0;
  if (a == null) return 1;
  if (b == null) return -1;
  const sign = direction === "asc" ? 1 : -1;
  if (integerColumn(column)) {
    const left = exactInteger(a);
    const right = exactInteger(b);
    if (left !== null && right !== null) return (left < right ? -1 : left > right ? 1 : 0) * sign;
  }
  if (typeof a === "number" && typeof b === "number") return (a - b) * sign;
  return String(a).localeCompare(String(b)) * sign;
}
