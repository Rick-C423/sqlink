export interface SessionCapabilities {
  cancellation: boolean;
  cancellationReason: string | null;
  exclusiveSession: boolean;
  resetPolicy: "full" | "transactionOnly" | "unsupported";
}

export interface TaskSnapshot {
  protocolVersion: 1;
  taskId: string;
  queryId: number | null;
  connectionId: string;
  connectionGeneration: string | null;
  operation: string;
  phase: "queued" | "preparing" | "running" | "committing" | "succeeded" | "failed" | "cancelled" | "unknown";
  sequence: number;
  cancelRequested: boolean;
  capabilities: SessionCapabilities | null;
  sessionId: string | null;
  startedAt: number;
  completedAt: number | null;
  commitState: "notStarted" | "notApplicable" | "committed" | "rolledBack" | "unknown";
  rollbackState: "notStarted" | "succeeded" | "failed" | "unknown";
  cleanupState: "notStarted" | "succeeded" | "failed" | "unknown";
  errorCode: string | null;
  progress?: { stage: string; processedBytes: number; totalBytes: number | null; processedStatements: number } | null;
}
