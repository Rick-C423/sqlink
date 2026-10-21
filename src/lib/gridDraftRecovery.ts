import type { QueryResult } from "@/types/database";
import type { GridDraft } from "@/stores/gridDraftStore";
import type { TypedValue } from "@/types/typedValue";
import { isTypedValue } from "@/lib/typedValue";
import { getGridEditPolicy } from "@/lib/gridEdits";
import { buildGridDraftChangeSet } from "@/lib/gridCellInput";

export interface GridDraftRecoveryPreview {
  result: QueryResult;
  edits: GridDraft["edits"];
  cellIntents: NonNullable<GridDraft["cellIntents"]>;
  newRows: GridDraft["newRows"];
  deletedRows: GridDraft["deletedRows"];
}

const uuid = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;
function rowSignature(values: readonly (TypedValue | null)[] | undefined, columns: number): string | undefined {
  if (!values || values.length !== columns || values.some((value) => !isTypedValue(value))) return undefined;
  return JSON.stringify(values.map((entry) => {
    const value = entry!;
    if (value.kind === "null") return ["null"];
    if (value.kind === "time") return [value.kind, value.value.value, value.value.precision];
    if (value.kind === "timestamp") return [value.kind, value.value.value, value.value.precision, value.value.timezone];
    return [value.kind, value.kind === "float" && Object.is(value.value, -0) ? "-0" : value.value];
  }));
}

/** 只匹配完整无损原行；不按行号、显示值或旧句柄把草稿迁到新结果。 */
export function prepareGridDraftRecovery(draft: GridDraft, next: QueryResult): GridDraftRecoveryPreview {
  const previous = draft.sourceResult;
  const before = previous?.resultContext;
  const after = next.resultContext;
  if (!previous || !before || !after || after.protocolVersion !== 1 || !uuid.test(after.resultId) ||
    before.resultId.toLowerCase() === after.resultId.toLowerCase() || !Number.isSafeInteger(after.revision) || after.revision < 1 ||
    !/^[0-9a-f]{64}$/.test(before.recoveryIdentity ?? "") || before.recoveryIdentity !== after.recoveryIdentity ||
    !Number.isSafeInteger(after.expiresAt) || after.expiresAt <= Date.now() || !getGridEditPolicy(next).editable ||
    before.connectionId !== after.connectionId || before.connectionGeneration !== after.connectionGeneration ||
    before.source.kind !== "table" || after.source.kind !== "table" ||
    before.source.database !== after.source.database || (before.source.schema ?? null) !== (after.source.schema ?? null) ||
    before.source.table !== after.source.table) throw new Error("新读取结果的来源、连接代次或编辑授权不匹配；原草稿已保留，请人工核对。");
  if (before.columns.length !== after.columns.length || next.columns.length !== after.columns.length ||
    before.columns.some((column, index) => {
      const other = after.columns[index];
      return column.displayName !== other.displayName || next.columns[index].name !== other.displayName ||
        column.databaseType !== other.databaseType || column.nullable !== other.nullable ||
        column.generated !== other.generated || column.writable !== other.writable ||
        column.origin?.database !== other.origin?.database || (column.origin?.schema ?? null) !== (other.origin?.schema ?? null) ||
        column.origin?.table !== other.origin?.table || column.origin?.column !== other.origin?.column;
    })) throw new Error("字段结构或来源已变化；不能自动恢复，原草稿已保留。");

  const touched = new Set(draft.deletedRows);
  for (const key of Object.keys(draft.edits)) {
    if (!/^[0-9]+:[0-9]+$/.test(key)) throw new Error("草稿单元格索引无效。");
    const [row, column] = key.split(":").map(Number);
    if (column >= before.columns.length) throw new Error("草稿字段已失效。");
    touched.add(row);
  }
  const candidates = new Map<string, number[]>();
  next.rows.forEach((_, index) => {
    const signature = rowSignature(next.typedRows?.[index], after.columns.length);
    if (signature !== undefined) candidates.set(signature, [...(candidates.get(signature) ?? []), index]);
  });
  const rows = new Map<number, number>();
  const used = new Set<number>();
  for (const row of touched) {
    const oldHandle = previous.rowHandles?.[row];
    const signature = rowSignature(previous.typedRows?.[row], before.columns.length);
    const matches = signature === undefined ? [] : candidates.get(signature) ?? [];
    if (!Number.isInteger(row) || row < 0 || row >= previous.rows.length || !oldHandle || !uuid.test(oldHandle) ||
      matches.length !== 1 || used.has(matches[0]) || !uuid.test(next.rowHandles?.[matches[0]] ?? "")) {
      throw new Error("原始行已变化、不在当前页或缺少完整无损身份；不能自动恢复。请复制草稿并人工核对，原输入仍保留。");
    }
    rows.set(row, matches[0]); used.add(matches[0]);
  }
  const edits: GridDraft["edits"] = {};
  const cellIntents: NonNullable<GridDraft["cellIntents"]> = {};
  for (const [key, value] of Object.entries(draft.edits)) {
    const [row, column] = key.split(":").map(Number);
    const mapped = `${rows.get(row)}:${column}`;
    edits[mapped] = value;
    if (draft.cellIntents?.[key]) cellIntents[mapped] = draft.cellIntents[key];
  }
  draft.newRows.forEach((row, index) => Object.keys(row).forEach((column) => {
    const key = `new:${index}:${column}`;
    if (draft.cellIntents?.[key]) cellIntents[key] = draft.cellIntents[key];
  }));
  const preview = { result: next, edits, cellIntents, newRows: draft.newRows.map((row) => ({ ...row })),
    deletedRows: new Set([...draft.deletedRows].map((row) => rows.get(row)!)) };
  // 沿用现有类型及操作门禁，校验只生成引用，不执行或发送新请求。
  buildGridDraftChangeSet({ ...draft, ...preview, sourceResult: next, editingCell: null });
  return preview;
}

/** 人工核对备份不包含旧行句柄，不能直接当作可重放的写入请求。 */
export function gridDraftRecoveryText(draft: GridDraft): string {
  const context = draft.sourceResult?.resultContext;
  const touched = [...new Set([...draft.deletedRows, ...Object.keys(draft.edits).map((key) => Number(key.split(":")[0]))])];
  return JSON.stringify({ purpose: "仅供人工核对，不能自动重放写入", source: context && {
    connectionId: context.connectionId, connectionGeneration: context.connectionGeneration, ...context.source,
  }, changeSetId: draft.pendingRequest?.changeSetId, columns: context?.columns.map((column) => column.displayName),
  edits: draft.edits, cellIntents: draft.cellIntents, newRows: draft.newRows, deletedRows: [...draft.deletedRows],
  originalRows: touched.map((index) => ({ index, values: draft.sourceResult?.typedRows?.[index] })),
  }, null, 2);
}
