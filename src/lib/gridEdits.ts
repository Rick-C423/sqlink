import type { EditContext, QueryResult } from "@/types/database";

export interface GridEditPolicy {
  editable: boolean;
  reason: string;
}

export const GRID_READ_ONLY_REASON = "当前结果缺少后端可验证的列来源和行身份，网格暂为只读。";
export const QUERY_READ_ONLY_REASON = "SQL 查询结果为只读，请使用 SQL 编辑器执行修改。";

/** 用于检查受控读取的候选映射；通过检查也不等于获得后端写入授权。 */
export interface ControlledColumnOrigin {
  sourceColumn: string;
  direct: boolean;
  supported: boolean;
}

export interface ControlledGridOrigin {
  kind: "controlled-table";
  columns: ControlledColumnOrigin[];
}

export interface GridIdentityCheck {
  valid: boolean;
  reason: string;
}

const denied = (reason: string): GridEditPolicy => ({ editable: false, reason });
const invalid = (reason: string): GridIdentityCheck => ({ valid: false, reason });

/** 显示列名、单表 SQL 和主键同名均不能证明真实行身份。 */
export function validateGridIdentity(
  result: QueryResult,
  context: EditContext,
  origin?: ControlledGridOrigin
): GridIdentityCheck {
  if (!origin || origin.kind !== "controlled-table") return invalid("没有可核验的受控表字段来源。");
  if (result.error || origin.columns.length !== result.columns.length) return invalid("字段来源映射不完整。");
  const names = result.columns.map((column) => column.name);
  const sources = origin.columns.map((column) => column.sourceColumn);
  if (new Set(names).size !== names.length || new Set(sources).size !== sources.length) {
    return invalid("结果存在重复列名或重复来源映射。");
  }
  if (origin.columns.some((column, index) => !column.direct || !column.supported || column.sourceColumn !== names[index])) {
    return invalid("结果包含表达式、别名或不支持的字段转换。");
  }
  if (!context.primaryKeys.length || new Set(context.primaryKeys).size !== context.primaryKeys.length) {
    return invalid("缺少完整且唯一的真实主键。");
  }
  const indexes = context.primaryKeys.map((key) => sources.indexOf(key));
  if (indexes.some((index) => index < 0)) return invalid("结果未包含完整主键。");
  if (indexes.some((index) => !["integer", "string", "boolean"].includes(result.columns[index].unifiedType.toLowerCase()))) {
    return invalid("主键类型尚未支持无损行定位。");
  }
  const seen = new Set<string>();
  for (const row of result.rows) {
    if (row.values.length !== names.length) return invalid("结果行与字段映射不一致。");
    const values = indexes.map((index) => row.values[index]);
    if (values.some((value) => value === null || value === undefined)) return invalid("主键包含空值。");
    if (values.some((value) => typeof value === "number" && !Number.isSafeInteger(value))) {
      return invalid("主键包含不安全数值，无法无损定位原行。");
    }
    if (indexes.some((index) => {
      const type = result.columns[index].unifiedType.toLowerCase();
      return typeof row.values[index] !== (type === "integer" ? "number" : type === "boolean" ? "boolean" : "string");
    })) {
      return invalid("主键显示值与字段类型不一致。");
    }
    const identity = JSON.stringify(values);
    if (seen.has(identity)) return invalid("结果中存在重复行身份。");
    seen.add(identity);
  }
  return { valid: true, reason: "候选行身份检查通过，仍需后端可信上下文授权。" };
}

/** 编辑权限只来自后端按驱动和字段类型发布的能力。 */
export function getGridEditPolicy(result: QueryResult | undefined, _context?: EditContext | null): GridEditPolicy {
  if (result?.truncated) return denied("结果读取已提前停止，请减少每页行数或查询字段后重新读取；截断结果不可编辑。");
  if (result?.resultContext) {
    const context = result.resultContext;
    const reasons = context.editPolicy.reasonCodes;
    if (!result.error && context.source.kind === "table" &&
      context.columns.length === result.columns.length &&
      context.editPolicy.allowedOperations.some((op) => ["insert", "update", "delete"].includes(op))) {
      return { editable: true, reason: "" };
    }
    if (reasons.includes("NO_PRIMARY_KEY")) return denied("结果来源已绑定，但该表缺少可信主键，网格继续只读。");
    if (reasons.includes("SOURCE_NOT_BASE_TABLE")) return denied("结果来源已绑定，但视图或未知来源不能建立可信行身份，网格继续只读。");
    if (reasons.includes("ROW_IDENTITY_UNAVAILABLE")) return denied("结果来源已绑定，部分主键类型尚不能无损定位，网格继续只读。");
    return denied("受控提交与冲突回执已接通；凭据恢复和各驱动实库验收完成前，网格继续只读。");
  }
  return denied(GRID_READ_ONLY_REASON);
}

/** 保留入口以明确拒绝旧调用，不能再从显示值拼接 UPDATE/DELETE。 */
export function buildEditStatements(
  _result: QueryResult,
  _context: EditContext,
  _edits: Record<string, string>,
  _newRows: Record<number, string>[],
  _deletedRows: ReadonlySet<number>
): string[] {
  throw new Error(GRID_READ_ONLY_REASON);
}
