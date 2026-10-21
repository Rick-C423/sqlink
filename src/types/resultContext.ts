import type { QueryResult, ResolvedTable } from "@/types/database";
import type { TypedValue } from "@/types/typedValue";

/** 只允许后端生成受控表读取的 SQL 和来源映射。 */
export interface ControlledTableReadRequest {
  protocolVersion: 1;
  connectionId: string;
  database: string;
  tableParts: string[];
  page: number;
  pageSize: number;
  sort?: { column: string; direction: "asc" | "desc" } | null;
}

export type GridColumnInputKind = "integer" | "unsignedInteger" | "text" | "boolean" | "binary" | "float";

export interface ResultContext {
  protocolVersion: number;
  resultId: string;
  revision: number;
  connectionId: string;
  connectionGeneration: string;
  source: { database: string; schema?: string | null; table: string; kind: "table" | "view" | "unknown" };
  columns: {
    columnId: string;
    displayName: string;
    databaseType: string;
    nullable: boolean;
    origin?: { database: string; schema?: string | null; table: string; column: string } | null;
    writable: boolean;
    generated: boolean;
  }[];
  editPolicy: { allowedOperations: string[]; reasonCodes: string[]; columnInputKinds?: Record<string, GridColumnInputKind> };
  createdAt: number;
  expiresAt: number;
}

export interface ControlledTablePage {
  result: QueryResult;
  context: ResultContext;
  rowHandles: (string | null)[];
  /** 后端无损值；可选仅为兼容历史前端 mock，不从显示值补齐。 */
  typedRows?: (TypedValue | null)[][];
  table: ResolvedTable;
  totalRows: number | null;
}

export interface ResultContextValidationRequest {
  protocolVersion: 1;
  resultId: string;
  revision: number;
  rowHandles: string[];
}
