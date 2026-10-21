import type { QueryResult } from "@/types/database";
import type { CellInput, ChangeSetOperation, ChangeSetRequest } from "@/types/typedValue";
import { cloneCellInput } from "@/lib/typedValue";

export interface ChangeSetCellChange {
  columnIndex: number;
  input: CellInput;
}

export type ChangeSetDraftOperation =
  | { opId?: string; kind: "insert"; changes: readonly ChangeSetCellChange[] }
  | { opId?: string; kind: "update"; rowIndex: number; changes: readonly ChangeSetCellChange[] }
  | { opId?: string; kind: "delete"; rowIndex: number };

export interface ChangeSetBuildOptions {
  changeSetId?: string;
  generateId?: () => string;
}

const uuid = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;
function requireUuid(value: string, name: string): string {
  if (typeof value !== "string" || !uuid.test(value)) throw new Error(`${name} 必须是 UUID。`);
  return value;
}

/**
 * 构造预检和提交共用的引用请求，不生成 SQL，也不授予写入权限。
 * 网格另行核对后端发布的操作与字段能力；预检 executable:false 不能成为执行授权。
 * 只读取列身份与行句柄，绝不从显示值推断类型、主键或原值。
 */
export function buildChangeSetRequest(
  result: QueryResult,
  operations: readonly ChangeSetDraftOperation[],
  options: ChangeSetBuildOptions = {},
): ChangeSetRequest {
  const context = result.resultContext;
  if (!context || context.protocolVersion !== 1 || result.error || !Number.isSafeInteger(context.revision) || context.revision < 1) {
    throw new Error("变更预检需要有效的后端结果上下文。");
  }
  requireUuid(context.resultId, "结果身份");
  if (context.source.kind !== "table") throw new Error("变更预检需要受控基础表来源。");
  if (context.columns.length !== result.columns.length ||
    context.columns.some((column, index) => !column.columnId || column.displayName !== result.columns[index].name) ||
    new Set(context.columns.map((column) => column.columnId)).size !== context.columns.length) {
    throw new Error("列身份与受控结果不一致。");
  }
  for (const column of context.columns) {
    requireUuid(column.columnId, "列身份");
    const origin = column.origin;
    if (!origin || !origin.column || origin.column !== column.displayName ||
      origin.database !== context.source.database || origin.table !== context.source.table ||
      (origin.schema ?? null) !== (context.source.schema ?? null)) throw new Error("字段来源映射不完整或与原结果不一致。");
  }
  const generateId = options.generateId ?? (() => globalThis.crypto.randomUUID());
  const changeSetId = requireUuid(options.changeSetId ?? generateId(), "变更集身份");
  const opIds = new Set<string>();
  const rowHandles = new Set<string>();
  const mapped: ChangeSetOperation[] = operations.map((operation) => {
    const opId = requireUuid(operation.opId ?? generateId(), "操作身份");
    if (opIds.has(opId.toLowerCase())) throw new Error("变更集不能包含重复操作身份。");
    opIds.add(opId.toLowerCase());
    let rowHandle: string | undefined;
    if (operation.kind === "update" || operation.kind === "delete") {
      if (!Number.isInteger(operation.rowIndex) || operation.rowIndex < 0 || operation.rowIndex >= result.rows.length ||
        !result.rowHandles || result.rowHandles.length !== result.rows.length) throw new Error("变更行没有有效的后端句柄映射。");
      const handle = result.rowHandles[operation.rowIndex];
      if (handle === null) throw new Error("该行没有可无损定位的后端句柄。");
      rowHandle = requireUuid(handle, "行句柄");
      if (rowHandles.has(rowHandle.toLowerCase())) throw new Error("同一行句柄不能重复变更。");
      rowHandles.add(rowHandle.toLowerCase());
    }
    if (operation.kind === "delete") return { opId, kind: "delete", rowHandle: rowHandle! };
    if (operation.kind !== "insert" && operation.kind !== "update") throw new Error("不支持的变更操作。");
    const changes: Record<string, CellInput> = Object.create(null);
    const indexes = new Set<number>();
    for (const change of operation.changes) {
      if (!Number.isInteger(change.columnIndex) || change.columnIndex < 0 || change.columnIndex >= context.columns.length ||
        indexes.has(change.columnIndex)) throw new Error("变更列索引未知或重复。");
      indexes.add(change.columnIndex);
      const column = context.columns[change.columnIndex];
      if (column.generated) throw new Error("生成列或标识列不能构造变更。");
      changes[column.columnId] = cloneCellInput(change.input);
    }
    return operation.kind === "insert" ? { opId, kind: "insert", changes } :
      { opId, kind: "update", rowHandle: rowHandle!, changes };
  });
  return { protocolVersion: 1, changeSetId, resultId: context.resultId, revision: context.revision, operations: mapped };
}
