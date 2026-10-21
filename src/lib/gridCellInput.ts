import type { QueryResult } from "@/types/database";
import type { GridColumnInputKind } from "@/types/resultContext";
import type { CellInput, TypedValue } from "@/types/typedValue";
import type { GridDraft } from "@/stores/gridDraftStore";
import { isTypedValue } from "@/lib/typedValue";
import { buildChangeSetRequest, type ChangeSetDraftOperation } from "@/lib/changeset";

export type GridCellIntent = "value" | "null" | "default";
const inputKinds = new Set(["integer", "unsignedInteger", "text", "boolean", "binary", "float"]);

/** 只认后端发布的字段输入类型，绝不从显示文本或显示值推断。 */
export function gridColumnInputKind(result: QueryResult | undefined, index: number): GridColumnInputKind | undefined {
  const context = result?.resultContext;
  const column = context?.columns[index];
  const kind = column && context?.editPolicy.columnInputKinds?.[column.columnId];
  return column?.writable && !column.generated && kind && inputKinds.has(kind) ? kind : undefined;
}

export function typedValueInputText(value: TypedValue | null | undefined): string {
  if (!value || value.kind === "null") return "";
  if (value.kind === "binary") return Array.from(atob(value.value), (byte) => byte.charCodeAt(0).toString(16).padStart(2, "0")).join("");
  if (value.kind === "time" || value.kind === "timestamp") return value.value.value;
  return String(value.value);
}

/** 空文本、SQL NULL 与 DEFAULT 是三个互不混淆的意图。 */
export function parseGridCellInput(kind: GridColumnInputKind, text: string, intent: GridCellIntent = "value", nullable = true): CellInput {
  if (intent === "default") return { mode: "default" };
  if (intent === "null") {
    if (!nullable) throw new Error("该字段不允许 SQL NULL。");
    return { mode: "set", value: { kind: "null" } };
  }
  let value: TypedValue;
  switch (kind) {
    case "integer": value = { kind: "int64", value: text }; break;
    case "unsignedInteger": value = { kind: "uint64", value: text }; break;
    case "text": value = { kind: "text", value: text }; break;
    case "boolean":
      if (text !== "true" && text !== "false") throw new Error("布尔值请输入 true 或 false。");
      value = { kind: "boolean", value: text === "true" }; break;
    case "binary": {
      if (!/^(?:[0-9a-fA-F]{2})*$/.test(text)) throw new Error("二进制请输入完整的十六进制字节，例如 00ff（不带 0x）。");
      const bytes = text.match(/.{2}/g) ?? [];
      value = { kind: "binary", value: btoa(bytes.map((byte) => String.fromCharCode(parseInt(byte, 16))).join("")) }; break;
    }
    case "float":
      if (!/^[+-]?(?:[0-9]+(?:\.[0-9]*)?|\.[0-9]+)(?:[eE][+-]?[0-9]+)?$/.test(text)) throw new Error("浮点值必须是有限的十进制数。");
      value = { kind: "float", value: Number(text) }; break;
  }
  if (!isTypedValue(value)) throw new Error("输入不符合字段类型或超过允许范围；整数请使用严格十进制，且不得经过浮点转换。");
  return { mode: "set", value };
}

export function buildGridDraftChangeSet(draft: GridDraft) {
  const result = draft.sourceResult;
  const context = result?.resultContext;
  if (!result || !context) throw new Error("缺少可信的原始结果上下文。");
  const operations: ChangeSetDraftOperation[] = [];
  const requireOperation = (kind: string) => {
    if (!context.editPolicy.allowedOperations.includes(kind)) throw new Error("后端尚未授权此类网格操作。");
  };
  const change = (columnIndex: number, text: string, key: string) => {
    const kind = gridColumnInputKind(result, columnIndex);
    if (!kind) throw new Error("该列为只读，或后端尚未发布输入类型。");
    return { columnIndex, input: parseGridCellInput(kind, text, draft.cellIntents?.[key], context.columns[columnIndex].nullable) };
  };
  const updates = new Map<number, { columnIndex: number; input: CellInput }[]>();
  for (const [key, text] of Object.entries(draft.edits)) {
    if (!/^[0-9]+:[0-9]+$/.test(key)) throw new Error("编辑单元格索引无效。");
    const [rowIndex, columnIndex] = key.split(":").map(Number);
    if (draft.deletedRows.has(rowIndex)) continue;
    const changes = updates.get(rowIndex) ?? [];
    changes.push(change(columnIndex, text, key));
    updates.set(rowIndex, changes);
  }
  for (const [rowIndex, changes] of updates) { requireOperation("update"); operations.push({ kind: "update", rowIndex, changes }); }
  for (const rowIndex of draft.deletedRows) { requireOperation("delete"); operations.push({ kind: "delete", rowIndex }); }
  draft.newRows.forEach((row, rowIndex) => {
    requireOperation("insert");
    operations.push({ kind: "insert", changes: Object.entries(row).map(([column, text]) => change(Number(column), text, `new:${rowIndex}:${column}`)) });
  });
  if (!operations.length) throw new Error("草稿中没有可提交的变更。");
  return buildChangeSetRequest(result, operations);
}
