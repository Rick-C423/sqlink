import type { TableInfo, TreeNode } from "@/types/database";

/** UI 键只作索引，操作来源由 source 传递，不再反解拼接字符串。 */
export const databaseNodeId = (connectionId: string, database: string): string =>
  JSON.stringify(["database", connectionId, database]);

export const relationTabId = (kind: "table" | "view", connectionId: string, database: string, table: string, schema?: string): string =>
  JSON.stringify([kind, connectionId, database, schema ?? null, table]);

export function relationNode(parentId: string, kind: "table" | "view", connectionId: string, database: string, info: TableInfo): TreeNode {
  return {
    id: JSON.stringify([parentId, kind, info.schema ?? null, info.name]),
    type: kind,
    name: info.schema ? `${info.schema}.${info.name}` : info.name,
    source: { connectionId, database, schema: info.schema, table: info.name },
    metadata: { rowCount: info.rowCount, comment: info.comment },
  };
}
