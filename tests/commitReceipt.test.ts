import assert from "node:assert/strict";
import { beforeEach, afterEach, test } from "node:test";
import { validateCommitReceipt } from "../src/lib/commitReceipt";
import { tauriService } from "../src/services/tauriService";
import { guardGridDraft, hasGridDraftChanges, isGridDraftDirty, releaseGridDraft, useGridDraftStore } from "../src/stores/gridDraftStore";
import type { ChangeSetRequest } from "../src/types/typedValue";
import type { CommitReceipt } from "../src/types/commitReceipt";
import type { QueryResult } from "../src/types/database";

const id = (n: number) => `00000000-0000-4000-8000-${String(n).padStart(12, "0")}`;
const request = (): ChangeSetRequest => ({ protocolVersion: 1, changeSetId: id(1), resultId: id(2), revision: 1,
  operations: [{ kind: "delete", opId: id(3), rowHandle: id(4) }] });
const source = (): QueryResult => ({ columns: [], rows: [], executionTimeMs: 0, resultContext: {
  protocolVersion: 1, resultId: id(2), revision: 1, connectionId: "连接", connectionGeneration: "代次",
  source: { database: "库", table: "表", kind: "table" }, columns: [], editPolicy: { allowedOperations: [], reasonCodes: [] }, createdAt: 0, expiresAt: 100,
} });
const receipt = (patch: Partial<CommitReceipt> = {}): CommitReceipt => ({ protocolVersion: 1, changeSetId: id(1), resultId: id(2), revision: 1,
  connectionId: "连接", connectionGeneration: "代次", taskId: null, status: "committed", commitState: "committed",
  rollbackState: "notStarted", cleanupState: "succeeded", refreshState: "required", operationResults: [
    { opId: id(3), outcome: "applied", matchedRows: 1, affectedRows: 1, errorCode: null },
  ], errorCode: null, reasonCodes: [], completedAt: 1, ...patch });
const state = () => useGridDraftStore.getState();
const originalCommit = tauriService.commitChangeSet;
const originalReceipt = tauriService.getChangeSetReceipt;
const originalRelease = tauriService.releaseResultContext;
beforeEach(() => {
  useGridDraftStore.setState({ drafts: {}, navigation: null });
  tauriService.releaseResultContext = async () => {};
});
afterEach(() => { tauriService.commitChangeSet = originalCommit; tauriService.getChangeSetReceipt = originalReceipt; tauriService.releaseResultContext = originalRelease; });
function dirty() {
  state().ensureDraft("表", source());
  state().updateDraft("表", () => ({ deletedRows: new Set([0]) }));
}

test("严格回执拒绝错误身份、重复/缺失操作及矛盾状态，已提交清理失败仍为已提交", () => {
  assert.equal(validateCommitReceipt(receipt({ cleanupState: "failed" }), request()).status, "committed");
  for (const broken of [receipt({ resultId: id(8) }), receipt({ changeSetId: id(8) }), receipt({ revision: 2 }),
    receipt({ operationResults: [] }), receipt({ operationResults: [...receipt().operationResults, ...receipt().operationResults] }),
    receipt({ operationResults: [{ ...receipt().operationResults[0], opId: id(8) }] }),
    receipt({ status: "unknown" }), receipt({ commitState: "notStarted" }),
    receipt({ status: "failedBeforeCommit", commitState: "notStarted", cleanupState: "failed", refreshState: "notRequested",
      operationResults: [{ ...receipt().operationResults[0], outcome: "notExecuted" }] }),
    receipt({ operationResults: [{ ...receipt().operationResults[0], currentValues: [{ kind: "int64", value: "1.5" }] }] })]) {
    assert.throws(() => validateCommitReceipt(broken, request()), /回执无效/);
  }
  assert.throws(() => validateCommitReceipt(receipt(), request(), { connectionId: "其他", connectionGeneration: "代次" }));
  const pair = request(); pair.operations.push({ ...pair.operations[0], opId: id(5) });
  assert.throws(() => validateCommitReceipt(receipt({ operationResults: [...receipt().operationResults, ...receipt().operationResults] }), pair));
});

test("类型化服务使用冻结 IPC 参数，核查只读且不改成重提", async () => {
  const target = globalThis as typeof globalThis & { window?: unknown };
  const previous = target.window; const calls: unknown[] = [];
  target.window = { __TAURI_INTERNALS__: { invoke: async (command: string, args: unknown) => { calls.push([command, args]); return receipt(); } } };
  try {
    await originalCommit(request(), "任务"); await originalReceipt(id(1), id(2), 1);
    assert.deepEqual(calls, [["changeset_commit", { request: request(), taskId: "任务" }],
      ["changeset_receipt", { changeSetId: id(1), resultId: id(2), revision: 1 }]]);
  } finally { if (previous === undefined) delete target.window; else target.window = previous; }
});

test("事务内校验失败且回滚确认成功仍为failedBeforeCommit，保留草稿而非误判未知", async () => {
  for (const rollbackState of ["notStarted", "succeeded"] as const) {
    useGridDraftStore.setState({ drafts: {}, navigation: null }); dirty();
    const failed = receipt({ status: "failedBeforeCommit", commitState: "notStarted", rollbackState,
      cleanupState: "succeeded", refreshState: "notRequested", errorCode: "METADATA_CHANGED",
      operationResults: [{ ...receipt().operationResults[0], outcome: "notExecuted", matchedRows: null, affectedRows: null }] });
    assert.equal(validateCommitReceipt(failed, request()).status, "failedBeforeCommit");
    assert.throws(() => validateCommitReceipt({ ...failed, operationResults: receipt().operationResults }, request()));
    for (const unsafe of ["failed", "unknown"] as const) assert.throws(() => validateCommitReceipt({ ...failed, rollbackState: unsafe }, request()));
    tauriService.commitChangeSet = async () => failed;
    let reads = 0;
    assert.equal(await state().saveChangeSetDraft("表", request(), async () => { reads++; }), false);
    assert.equal(state().drafts["表"].saveState, "failedBeforeCommit");
    assert.equal(state().drafts["表"].commitReceipt?.rollbackState, rollbackState);
    assert.equal(isGridDraftDirty(state().drafts["表"]), true); assert.equal(reads, 0);
    const decision = guardGridDraft("表"); await state().chooseNavigation("cancel"); assert.equal(await decision, false);
    assert.equal(state().drafts["表"].saveState, "failedBeforeCommit");
  }
});

test("committed清输入与刷新期间始终独占，已清草稿不能被同步订阅释放或替换", async () => {
  dirty(); tauriService.commitChangeSet = async () => receipt();
  let finish!: () => void; let clearedBeforeRead = false; let refreshComplete = false;
  const initial = state().drafts["表"].sourceResult;
  const newer = { ...source(), rows: [{ values: ["新结果"] }] };
  const unsubscribe = useGridDraftStore.subscribe((current) => {
    const draft = current.drafts["表"];
    if (!refreshComplete && draft?.commitReceipt?.status === "committed" && !isGridDraftDirty(draft)) {
      state().ensureDraft("表", newer); releaseGridDraft("表");
    }
  });
  try {
    const saving = state().saveChangeSetDraft("表", request(), async () => {
      clearedBeforeRead = !isGridDraftDirty(state().drafts["表"]);
      await new Promise<void>((resolve) => { finish = resolve; });
      refreshComplete = true;
    });
    await Promise.resolve();
    assert.equal(clearedBeforeRead, true); assert.equal(state().drafts["表"].submitting, true);
    assert.equal(state().drafts["表"].sourceResult, initial); assert.equal(await guardGridDraft("表"), false);
    finish(); assert.equal(await saving, true);
  } finally { unsubscribe(); }
});

test("未知结果取消导航不丢稳定请求，后续核查不会延续已取消导航", async () => {
  dirty(); let commits = 0;
  tauriService.commitChangeSet = async () => { commits++; throw new Error("响应丢失"); };
  assert.equal(await state().saveChangeSetDraft("表", request(), async () => {}), false);
  const pending = state().drafts["表"].pendingRequest;
  const decision = guardGridDraft("表"); await state().chooseNavigation("cancel"); assert.equal(await decision, false);
  assert.equal(state().navigation, null); assert.equal(state().drafts["表"].pendingRequest, pending);
  tauriService.getChangeSetReceipt = async () => receipt();
  assert.equal(await state().checkChangeSetReceipt("表"), true);
  assert.equal(state().navigation, null); assert.equal(commits, 1);
});

test("刷新失败先清已提交草稿并保留回执；导航保存只重试读取", async () => {
  dirty(); let commits = 0; let reads = 0;
  tauriService.commitChangeSet = async () => { commits++; return receipt(); };
  const refresh = async () => { reads++; if (reads < 3) throw new Error("断线"); };
  assert.equal(await state().saveChangeSetDraft("表", request(), refresh), false);
  assert.equal(state().drafts["表"].saveState, "committedRefreshFailed");
  assert.equal(isGridDraftDirty(state().drafts["表"]), false);
  assert.equal(state().drafts["表"].commitReceipt?.commitState, "committed");
  assert.equal(hasGridDraftChanges("表"), true);
  const decision = guardGridDraft("表"); await state().chooseNavigation("save"); assert.equal(await decision, false);
  assert.equal(await state().saveChangeSetDraft("表", request(), refresh), false);
  assert.equal(await state().refreshCommittedDraft("表"), true);
  assert.equal(commits, 1); assert.equal(reads, 3);
});

test("IPC异常保留请求稳定身份，核查失败与unknown均不自动重发，后续committed才清草稿", async () => {
  dirty(); let commits = 0; let lookups = 0; let reads = 0;
  tauriService.commitChangeSet = async () => { commits++; throw new Error("响应丢失"); };
  tauriService.getChangeSetReceipt = async (changeSetId, resultId, revision) => {
    lookups++; assert.deepEqual([changeSetId, resultId, revision], [id(1), id(2), 1]);
    if (lookups === 1) throw { code: "RECEIPT_NOT_FOUND" };
    if (lookups === 2) return receipt({ status: "unknown", commitState: "unknown", operationResults: [{ ...receipt().operationResults[0], outcome: "unknown" }] });
    return receipt();
  };
  const input = request();
  assert.equal(await state().saveChangeSetDraft("表", input, async () => { reads++; }), false);
  input.changeSetId = id(9);
  assert.equal(state().drafts["表"].pendingRequest?.changeSetId, id(1));
  assert.equal(await state().saveChangeSetDraft("表", request(), async () => {}), false);
  state().discardDraft("表"); assert.equal(isGridDraftDirty(state().drafts["表"]), true);
  assert.equal(await state().checkChangeSetReceipt("表"), false);
  assert.equal(await state().checkChangeSetReceipt("表"), false);
  assert.equal(await state().checkChangeSetReceipt("表"), true);
  assert.equal(commits, 1); assert.equal(lookups, 3); assert.equal(reads, 1);
  assert.equal(isGridDraftDirty(state().drafts["表"]), false);
});

test("错误回执按未知保存，不能用错误opId清除草稿", async () => {
  dirty(); tauriService.commitChangeSet = async () => receipt({ operationResults: [{ ...receipt().operationResults[0], opId: id(8) }] });
  let reads = 0;
  assert.equal(await state().saveChangeSetDraft("表", request(), async () => { reads++; }), false);
  assert.equal(state().drafts["表"].saveState, "unknown"); assert.equal(isGridDraftDirty(state().drafts["表"]), true); assert.equal(reads, 0);
});

test("已提交但清理失败不会恢复已提交草稿或误标提交前失败", async () => {
  dirty(); tauriService.commitChangeSet = async () => receipt({ cleanupState: "failed", errorCode: "CLEANUP_FAILED" });
  assert.equal(await state().saveChangeSetDraft("表", request(), async () => { throw new Error("连接已失效"); }), false);
  assert.equal(state().drafts["表"].commitReceipt?.status, "committed");
  assert.equal(state().drafts["表"].commitReceipt?.cleanupState, "failed");
  assert.equal(state().drafts["表"].saveState, "committedRefreshFailed");
  assert.equal(isGridDraftDirty(state().drafts["表"]), false);
});

test("冲突和确认回滚保留草稿，导航保存不回退旧SQL回调", async () => {
  for (const status of ["conflict", "rolledBack", "failedBeforeCommit"] as const) {
    useGridDraftStore.setState({ drafts: {}, navigation: null }); dirty();
    let oldSaves = 0;
    state().updateDraft("表", () => ({ save: async () => { oldSaves++; } }));
    tauriService.commitChangeSet = async () => receipt({ status, commitState: status === "failedBeforeCommit" ? "notStarted" : "rolledBack",
      rollbackState: status === "failedBeforeCommit" ? "notStarted" : "succeeded", refreshState: "notRequested",
      operationResults: [{ ...receipt().operationResults[0], outcome: status === "failedBeforeCommit" ? "notExecuted" : status }] });
    assert.equal(await state().saveChangeSetDraft("表", request(), async () => {}), false);
    assert.equal(state().drafts["表"].saveState, status); assert.equal(isGridDraftDirty(state().drafts["表"]), true);
    const decision = guardGridDraft("表"); await state().chooseNavigation("save"); assert.equal(await decision, false); assert.equal(oldSaves, 0);
  }
});

test("并发提交被拒绝，迟到回执不覆盖新结果或新草稿", async () => {
  dirty(); let finish!: (result: CommitReceipt) => void; let reads = 0; let calls = 0;
  tauriService.commitChangeSet = async () => { calls++; return new Promise<CommitReceipt>((resolve) => { finish = resolve; }); };
  const saving = state().saveChangeSetDraft("表", request(), async () => { reads++; });
  assert.equal(await state().saveChangeSetDraft("表", request(), async () => {}), false);
  const newer = { ...source(), rows: [{ values: ["新结果"] }] };
  // 模拟页面整体替换/关闭后以同一key建立新草稿；旧异步任务不能写回。
  useGridDraftStore.setState({ drafts: {} }); state().ensureDraft("表", newer);
  state().updateDraft("表", () => ({ edits: { "0:0": "新草稿" } }));
  finish(receipt()); assert.equal(await saving, false);
  assert.equal(state().drafts["表"].sourceResult, newer); assert.equal(state().drafts["表"].edits["0:0"], "新草稿");
  assert.equal(state().drafts["表"].commitReceipt, undefined); assert.equal(reads, 0); assert.equal(calls, 1);
});

test("来源不匹配与重复操作在IPC前拒绝，不读取后来ensureDraft的上下文", async () => {
  dirty(); let calls = 0; tauriService.commitChangeSet = async () => { calls++; return receipt(); };
  state().ensureDraft("表", { ...source(), resultContext: { ...source().resultContext!, resultId: id(8) } });
  assert.equal(await state().saveChangeSetDraft("表", { ...request(), resultId: id(8) }, async () => {}), false);
  const duplicate = request(); duplicate.operations.push({ ...duplicate.operations[0] });
  assert.equal(await state().saveChangeSetDraft("表", duplicate, async () => {}), false); assert.equal(calls, 0);
});
