import type { CommitReceipt, RejectedBeforeExecution } from "@/types/commitReceipt";
import type { ChangeSetRequest } from "@/types/typedValue";
import type { ResultContext } from "@/types/resultContext";
import { isTypedValue } from "@/lib/typedValue";

const uuid = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;
const record = (value: unknown): value is Record<string, unknown> => !!value && typeof value === "object" && !Array.isArray(value);
const text = (value: unknown): value is string => typeof value === "string" && value.length > 0;
const nullableCode = (value: unknown) => value === null || text(value);
const count = (value: unknown) => value === null || (typeof value === "number" && Number.isSafeInteger(value) && value >= 0);
const completion = (value: unknown) => ["notStarted", "succeeded", "failed", "unknown"].includes(value as string);

/** 只接受绑定原请求的后端证明，不能根据错误码、异常文本或查无回执推断未执行。 */
export function rejectedBeforeExecution(value: unknown, request: ChangeSetRequest): RejectedBeforeExecution | undefined {
  if (!record(value) || !text(value.code) || !text(value.message) || !record(value.commitEvidence)) return undefined;
  const proof = value.commitEvidence;
  if (proof.protocolVersion !== 1 || proof.commitState !== "notStarted" ||
    !uuid.test(request.changeSetId) || !uuid.test(request.resultId) ||
    proof.changeSetId !== request.changeSetId || proof.resultId !== request.resultId ||
    !Number.isSafeInteger(request.revision) || request.revision < 1 || proof.revision !== request.revision) return undefined;
  return { code: value.code, message: value.message, commitEvidence: {
    protocolVersion: 1, changeSetId: request.changeSetId, resultId: request.resultId,
    revision: request.revision, commitState: "notStarted",
  } };
}

/** 校验身份、每个操作及事务事实；任何损坏都交给调用方按结果未知处理。 */
export function validateCommitReceipt(
  value: unknown,
  request: ChangeSetRequest,
  context?: Pick<ResultContext, "connectionId" | "connectionGeneration">,
): CommitReceipt {
  const invalid = (): never => { throw new Error("提交回执无效或与原始变更集不匹配，请核查结果，禁止自动重发。"); };
  if (!record(value) || value.protocolVersion !== 1 || request.protocolVersion !== 1 ||
    !uuid.test(request.changeSetId) || !uuid.test(request.resultId) ||
    value.changeSetId !== request.changeSetId || value.resultId !== request.resultId ||
    !Number.isSafeInteger(request.revision) || request.revision < 1 || value.revision !== request.revision ||
    !text(value.connectionId) || !text(value.connectionGeneration) ||
    (context && (value.connectionId !== context.connectionId || value.connectionGeneration !== context.connectionGeneration)) ||
    !(value.taskId === null || text(value.taskId)) || !nullableCode(value.errorCode) ||
    !completion(value.rollbackState) || !completion(value.cleanupState) ||
    !["notRequested", "required"].includes(value.refreshState as string) ||
    typeof value.completedAt !== "number" || !Number.isSafeInteger(value.completedAt) || value.completedAt < 0 ||
    !Array.isArray(value.reasonCodes) || !value.reasonCodes.every(text) || !Array.isArray(value.operationResults)) return invalid();

  const expected = new Set(request.operations.map((operation) => operation.opId));
  if (expected.size !== request.operations.length || new Set([...expected].map((id) => id.toLowerCase())).size !== expected.size ||
    [...expected].some((id) => !uuid.test(id)) ||
    value.operationResults.length !== expected.size) return invalid();
  const seen = new Set<string>();
  for (const operation of value.operationResults) {
    if (!record(operation) || !text(operation.opId) || !expected.has(operation.opId) || seen.has(operation.opId) ||
      !["applied", "unchanged", "conflict", "rolledBack", "notExecuted", "unknown"].includes(operation.outcome as string) ||
      !count(operation.matchedRows) || !count(operation.affectedRows) || !nullableCode(operation.errorCode) ||
      (operation.currentValues !== undefined && (!Array.isArray(operation.currentValues) ||
        !operation.currentValues.every((item: unknown) => item === null || isTypedValue(item))))) return invalid();
    seen.add(operation.opId);
  }
  const receipt = value as unknown as CommitReceipt;
  const outcomes = receipt.operationResults.map((operation) => operation.outcome);
  switch (receipt.status) {
    case "committed":
      if (receipt.commitState !== "committed" || receipt.rollbackState !== "notStarted" || receipt.refreshState !== "required" ||
        outcomes.some((outcome) => outcome !== "applied" && outcome !== "unchanged")) return invalid();
      break;
    case "rolledBack":
      if (receipt.commitState !== "rolledBack" || receipt.rollbackState !== "succeeded" ||
        outcomes.some((outcome) => !["rolledBack", "notExecuted", "conflict"].includes(outcome))) return invalid();
      break;
    case "conflict":
      if (!["notStarted", "notApplicable", "rolledBack"].includes(receipt.commitState) ||
        (receipt.commitState === "rolledBack" && receipt.rollbackState !== "succeeded") ||
        outcomes.some((outcome) => !["conflict", "rolledBack", "notExecuted"].includes(outcome))) return invalid();
      break;
    case "failedBeforeCommit":
      if (receipt.commitState !== "notStarted" || !["notStarted", "succeeded"].includes(receipt.rollbackState) || receipt.refreshState !== "notRequested" ||
        outcomes.some((outcome) => outcome !== "notExecuted")) return invalid();
      break;
    case "unknown":
      if (receipt.commitState !== "unknown") return invalid();
      break;
    default: return invalid();
  }
  if (receipt.status !== "committed" && receipt.status !== "unknown" &&
    (receipt.cleanupState === "failed" || receipt.cleanupState === "unknown" ||
      receipt.rollbackState === "failed" || receipt.rollbackState === "unknown" || receipt.refreshState !== "notRequested")) return invalid();
  return receipt;
}
