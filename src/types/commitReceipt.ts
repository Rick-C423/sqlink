import type { TypedValue } from "@/types/typedValue";

export type CommitStatus = "committed" | "conflict" | "rolledBack" | "failedBeforeCommit" | "unknown";
export type CommitState = "notStarted" | "notApplicable" | "committed" | "rolledBack" | "unknown";
export type CompletionState = "notStarted" | "succeeded" | "failed" | "unknown";

/** 后端已登记的未执行证明；普通错误码或回执缺失不能替代它。 */
export interface CommitExecutionEvidence {
  protocolVersion: 1;
  changeSetId: string;
  resultId: string;
  revision: number;
  commitState: "notStarted";
}

export interface RejectedBeforeExecution {
  code: string;
  message: string;
  commitEvidence: CommitExecutionEvidence;
}

/** 回执中的事务结果与清理结果分别记录，清理失败不能推翻已经提交的事实。 */
export interface CommitReceipt {
  protocolVersion: 1;
  changeSetId: string;
  resultId: string;
  revision: number;
  connectionId: string;
  connectionGeneration: string;
  taskId: string | null;
  status: CommitStatus;
  commitState: CommitState;
  rollbackState: CompletionState;
  cleanupState: CompletionState;
  refreshState: "notRequested" | "required";
  operationResults: {
    opId: string;
    outcome: "applied" | "unchanged" | "conflict" | "rolledBack" | "notExecuted" | "unknown";
    matchedRows: number | null;
    affectedRows: number | null;
    errorCode: string | null;
    currentValues?: (TypedValue | null)[];
  }[];
  errorCode: string | null;
  reasonCodes: string[];
  completedAt: number;
}
