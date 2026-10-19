import type { EditContext, QueryResult } from "@/types/database";
import { quoteIdent, sqlLiteral } from "@/lib/utils";
import { quoteTable } from "@/lib/sqlTable";

/** 使用结果来源中的完整表身份生成写语句，不重新读取当前连接。 */
export function buildEditStatements(
  result: QueryResult,
  context: EditContext,
  edits: Record<string, string>,
  newRows: Record<number, string>[],
  deletedRows: ReadonlySet<number>
): string[] {
  const { primaryKeys, driverType } = context;
  const columns = result.columns;
  const statements: string[] = [];
  const q = (name: string) => quoteIdent(name, driverType);
  const table = quoteTable(context);
  const pkWhere = (rowIdx: number) => primaryKeys.map((pk) => {
    const pkCol = columns.findIndex((column) => column.name === pk);
    return `${q(pk)} = ${sqlLiteral(result.rows[rowIdx].values[pkCol])}`;
  }).join(" AND ");

  const byRow = new Map<number, Map<number, string>>();
  for (const [key, value] of Object.entries(edits)) {
    const [row, column] = key.split(":").map(Number);
    if (deletedRows.has(row)) continue;
    if (!byRow.has(row)) byRow.set(row, new Map());
    byRow.get(row)!.set(column, value);
  }
  for (const [row, changedColumns] of byRow) {
    const sets = [...changedColumns.entries()].map(([column, value]) =>
      `${q(columns[column].name)} = ${sqlLiteral(value)}`
    );
    statements.push(`UPDATE ${table} SET ${sets.join(", ")} WHERE ${pkWhere(row)};`);
  }
  for (const row of deletedRows) {
    statements.push(`DELETE FROM ${table} WHERE ${pkWhere(row)};`);
  }
  for (const row of newRows) {
    const entries = Object.entries(row);
    if (!entries.length) continue;
    const names = entries.map(([column]) => q(columns[Number(column)].name));
    const values = entries.map(([, value]) => sqlLiteral(value));
    statements.push(`INSERT INTO ${table} (${names.join(", ")}) VALUES (${values.join(", ")});`);
  }
  return statements;
}
