/** 数据库无损读取值；DEFAULT 只属于 CellInput，不能出现在读取结果中。 */
export type TypedValue =
  | { kind: "null" }
  | { kind: "text"; value: string }
  | { kind: "boolean"; value: boolean }
  | { kind: "int64" | "uint64" | "decimal"; value: string }
  | { kind: "float"; value: number }
  | { kind: "binary"; value: string }
  | { kind: "date"; value: string }
  | { kind: "time"; value: { value: string; precision: number } }
  | { kind: "timestamp"; value: { value: string; precision: number; timezone: "none" | "offset" } };

export type CellInput = { mode: "set"; value: TypedValue } | { mode: "default" };

export type ChangeSetOperation =
  | { opId: string; kind: "insert"; changes: Record<string, CellInput> }
  | { opId: string; kind: "update"; rowHandle: string; changes: Record<string, CellInput> }
  | { opId: string; kind: "delete"; rowHandle: string };

/** 只有后端结果身份、列身份和行句柄，没有客户端连接、表或主键声明。 */
export interface ChangeSetRequest {
  protocolVersion: 1;
  changeSetId: string;
  resultId: string;
  revision: number;
  operations: ChangeSetOperation[];
}

/** 当前阶段仅校验协议与可信上下文，不能执行，也不返回 SQL。 */
export interface ChangeSetValidationResult {
  changeSetId: string;
  resultId: string;
  revision: number;
  operationCount: number;
  executable: false;
  reasonCodes: string[];
}
