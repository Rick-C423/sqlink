import type { ImportSummary } from "./database";
import type { TaskSnapshot } from "./task";

export type ImportConflict = "error" | "skip" | "overwrite";
export interface ImportPlanRequest {
  connectionId: string;
  database?: string;
  path: string;
  onConflict: ImportConflict;
  taskId: string;
}
export interface ImportExecuteRequest { planId: string; taskId: string }
export interface ImportPlan {
  planId: string;
  connectionId: string;
  database: string | null;
  path: string;
  onConflict: ImportConflict;
  connectionGeneration: string;
  fileBytes: number;
  statementCount: number;
  warnings: string[];
  expiresAt: number;
  task: TaskSnapshot;
}
export interface ImportReport {
  runId: string;
  planId: string;
  task: TaskSnapshot;
  summary: ImportSummary;
  failedStatement: number | null;
  /** 已发送或无法排除已发送的 SQL 数，包含目录查询及 overwrite 的 DROP。 */
  sentStatements: number;
  commitState: TaskSnapshot["commitState"];
  cleanupState: TaskSnapshot["cleanupState"];
  errorCode: string | null;
  message: string | null;
  durable: boolean;
  recovery: "manualVerification" | "safeToRetry";
  recovered: boolean;
}
