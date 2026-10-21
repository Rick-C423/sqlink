import type { ColumnInfo } from "../types/database";
import { quoteIdent } from "./utils";

/** 与网格排序状态结构兼容，纯函数不依赖 React 组件。 */
export interface TableSort {
  key: string;
  direction: "asc" | "desc";
}

/** 优先按完整列名匹配；只有不区分大小写的匹配唯一时才接受别名。 */
function resolveColumnIndex(columns: ColumnInfo[], key: string): number {
  const exact = columns.flatMap((column, index) => column.name === key ? [index] : []);
  if (exact.length === 1) return exact[0];
  const matches = exact.length > 0 ? exact : columns.flatMap((column, index) =>
    column.name.toLowerCase() === key.toLowerCase() ? [index] : []
  );
  if (matches.length !== 1) {
    throw new Error(matches.length === 0 ? `排序字段不存在：${key}` : `排序字段存在歧义：${key}`);
  }
  return matches[0];
}

/** 用户排序在前，补齐完整主键；按元数据列身份去重，不比较 SQL 片段。 */
export function buildStableSort(columns: ColumnInfo[], sort: TableSort | null): TableSort[] {
  const result: TableSort[] = [];
  const seen = new Set<number>();
  if (sort) {
    if (sort.direction !== "asc" && sort.direction !== "desc") {
      throw new Error("排序方向必须为 asc 或 desc");
    }
    const index = resolveColumnIndex(columns, sort.key);
    result.push({ key: columns[index].name, direction: sort.direction });
    seen.add(index);
  }
  columns.forEach((column, index) => {
    if (column.isPrimaryKey && !seen.has(index)) {
      result.push({ key: column.name, direction: "asc" });
      seen.add(index);
    }
  });
  return result;
}

/** 返回排序列表正文；没有主键时只能保留用户排序，不能保证跨页唯一顺序。 */
export function buildOrderBy(driverType: string, columns: ColumnInfo[], sort: TableSort | null): string {
  return buildStableSort(columns, sort)
    .map((item) => `${quoteIdent(item.key, driverType)} ${item.direction.toUpperCase()}`)
    .join(", ");
}

/** 四种驱动统一下推稳定排序；SQL Server 沿用 SQL Server 2005 起支持的 ROW_NUMBER 分页。 */
export function buildPageSql(
  driverType: string,
  table: string,
  where: string,
  page: number,
  pageSize: number,
  columns: ColumnInfo[],
  sort: TableSort | null,
  probeRows = 0
): string {
  const offset = (page - 1) * pageSize;
  const stableOrderBy = buildOrderBy(driverType, columns, sort);
  if (driverType !== "sqlserver" && driverType !== "mssql") {
    const orderBy = stableOrderBy ? ` ORDER BY ${stableOrderBy}` : "";
    return `SELECT * FROM ${table}${where}${orderBy} LIMIT ${pageSize + probeRows} OFFSET ${offset}`;
  }

  const rowNumber = quoteIdent("__sqlconnect_row_number", driverType);
  const pageAlias = quoteIdent("__sqlconnect_page", driverType);
  const selectedColumns = columns
    .map((column) => `${pageAlias}.${quoteIdent(column.name, driverType)}`)
    .join(", ");
  // 无主键且无用户排序时保留原有兼容行为，此时数据库无法提供唯一顺序。
  const orderBy = stableOrderBy || "(SELECT 1)";
  return `SELECT ${selectedColumns} FROM (` +
    `SELECT *, ROW_NUMBER() OVER (ORDER BY ${orderBy}) AS ${rowNumber} ` +
    `FROM ${table}${where}` +
    `) AS ${pageAlias} ` +
    `WHERE ${pageAlias}.${rowNumber} BETWEEN ${offset + 1} AND ${offset + pageSize + probeRows} ` +
    `ORDER BY ${pageAlias}.${rowNumber}`;
}
