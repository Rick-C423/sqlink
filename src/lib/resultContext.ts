import type { QueryResult } from "@/types/database";
import type { ControlledTablePage, ControlledTableReadRequest, ResultContext } from "@/types/resultContext";
import { isGridDraftDirty, type GridDraft } from "@/stores/gridDraftStore";
import { isTypedValue } from "@/lib/typedValue";

export type ReleaseResultContext = (resultId: string, revision: number) => Promise<void>;

/** Tauri 结构化错误保留可读消息与诊断码，避免显示为 [object Object]。 */
export function formatResultContextError(error: unknown): string {
  if (error && typeof error === "object" && "message" in error && typeof error.message === "string") {
    const code = "code" in error && typeof error.code === "string" ? error.code : "";
    return code ? `${error.message}（${code}）` : error.message;
  }
  return String(error);
}

/** 只把与当前请求、结果列和行数一致的 v1 后端上下文附到结果上。 */
export function attachResultContext(page: ControlledTablePage, request: ControlledTableReadRequest): QueryResult {
  const { result, context, rowHandles, table, typedRows } = page;
  const invalid = (reason: string): never => { throw new Error(`受控结果上下文无效：${reason}`); };
  if (!context || context.protocolVersion !== 1) invalid("不兼容的协议版本");
  if (!context.resultId || !Number.isSafeInteger(context.revision) || context.revision < 1 || !context.connectionGeneration) invalid("缺少结果身份");
  if (context.connectionId !== request.connectionId) invalid("连接与请求不一致");
  if (result.error) invalid("查询结果包含错误");
  if (!Array.isArray(context.columns) || context.columns.length !== result.columns.length ||
    context.columns.some((column, index) => !column.columnId || column.displayName !== result.columns[index].name || typeof column.writable !== "boolean") ||
    new Set(context.columns.map((column) => column.columnId)).size !== context.columns.length) invalid("列映射与结果不一致");
  if (!Array.isArray(rowHandles) || rowHandles.length !== result.rows.length ||
    rowHandles.some((handle) => handle !== null && (typeof handle !== "string" || !handle))) invalid("行句柄与结果不一致");
  if (!table || context.source.database !== table.database || context.source.table !== table.table ||
    (context.source.schema ?? null) !== (table.schema ?? null)) invalid("来源与解析表不一致");
  if (!context.editPolicy || !Array.isArray(context.editPolicy.allowedOperations) ||
    context.editPolicy.allowedOperations.some((operation) => !["insert", "update", "delete"].includes(operation)) ||
    new Set(context.editPolicy.allowedOperations).size !== context.editPolicy.allowedOperations.length ||
    !Array.isArray(context.editPolicy.reasonCodes)) invalid("编辑能力无效");
  const kinds = context.editPolicy.columnInputKinds ?? {};
  if (!kinds || typeof kinds !== "object" || Array.isArray(kinds) || Object.entries(kinds).some(([id, kind]) =>
    !["integer", "unsignedInteger", "text", "boolean", "binary", "float"].includes(kind) ||
    !context.columns.some((column) => column.columnId === id && column.writable && !column.generated)) ||
    context.columns.some((column) => column.writable && (!kinds[column.columnId] || column.generated))) invalid("类型化编辑映射无效");
  if (typedRows !== undefined && (!Array.isArray(typedRows) || typedRows.length !== result.rows.length ||
    Array.from(typedRows).some((row) => !Array.isArray(row) || row.length !== result.columns.length ||
      Array.from(row).some((value) => value !== null && !isTypedValue(value))))) invalid("无损值与结果列行不一致或类型无效");
  const writable = context.editPolicy.allowedOperations.length > 0;
  if (writable && (context.source.kind !== "table" || context.editPolicy.reasonCodes.length > 0 ||
    !typedRows || rowHandles.some((handle) => handle === null) || typedRows.some((row) => row.some((value) => value === null)))) invalid("可写结果缺少完整可信快照");
  if (!writable && (Object.keys(kinds).length > 0 || context.columns.some((column) => column.writable))) invalid("只读结果包含写入能力");
  // 仅接受受控响应的独立无损通道，不能保留 result 内未经检查的 typedRows。
  const attached = { ...result, resultContext: context, rowHandles };
  delete attached.typedRows;
  if (typedRows !== undefined) attached.typedRows = typedRows;
  return attached;
}

/** 脏草稿和正在提交的草稿固定保留最初结果的后端身份。 */
export function isResultContextPinned(context: ResultContext, drafts: Iterable<GridDraft>): boolean {
  for (const draft of drafts) {
    const source = draft.sourceResult?.resultContext;
    if (source?.resultId === context.resultId && source.revision === context.revision &&
      (draft.submitting || draft.saveState === "unknown" || draft.saveState === "committedRefreshFailed" || isGridDraftDirty(draft))) return true;
  }
  return false;
}

/** 释放失败交由后端 TTL 兜底，不能使旧请求干扰当前页面。 */
export async function retireResultContext(context: ResultContext | undefined, drafts: Iterable<GridDraft>, release: ReleaseResultContext): Promise<boolean> {
  if (!context?.resultId || !Number.isInteger(context.revision) || isResultContextPinned(context, drafts)) return false;
  try {
    await release(context.resultId, context.revision);
    return true;
  } catch {
    return false;
  }
}
