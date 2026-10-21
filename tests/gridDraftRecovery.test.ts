import assert from "node:assert/strict";
import { afterEach, beforeEach, test } from "node:test";
import { rejectedBeforeExecution } from "../src/lib/commitReceipt";
import { gridDraftRecoveryText, prepareGridDraftRecovery } from "../src/lib/gridDraftRecovery";
import { buildGridDraftChangeSet } from "../src/lib/gridCellInput";
import { guardGridDraft, isGridDraftDirty, releaseGridDraft, useGridDraftStore } from "../src/stores/gridDraftStore";
import { DatasetController } from "../src/lib/datasetController";
import { retireResultContext } from "../src/lib/resultContext";
import { tauriService } from "../src/services/tauriService";
import type { QueryResult } from "../src/types/database";
import type { ChangeSetRequest } from "../src/types/typedValue";
import type { CommitReceipt } from "../src/types/commitReceipt";

const id = (n: number) => `00000000-0000-4000-8000-${String(n).padStart(12, "0")}`;
const state = () => useGridDraftStore.getState();
function source(result = 1, reordered = false): QueryResult {
  const offset = result * 100;
  const values = reordered ? [["2", "第二行"], ["9007199254740993", "原值"]] : [["9007199254740993", "原值"], ["2", "第二行"]];
  return { columns: ["id", "payload"].map((name) => ({ name, dataType: "text", unifiedType: "string", nullable: true })),
    rows: values.map((row) => ({ values: row })), executionTimeMs: 1,
    typedRows: values.map(([key, value]) => [{ kind: "int64", value: key }, { kind: "text", value }]),
    rowHandles: values.map((_, index) => id(offset + 10 + index)),
    resultContext: { protocolVersion: 1, resultId: id(result), revision: 1, connectionId: "原连接", connectionGeneration: "原代次", recoveryIdentity: "a".repeat(64),
      source: { database: "原库", schema: "原模式", table: "原表", kind: "table" },
      columns: ["id", "payload"].map((name, index) => ({ columnId: id(offset + index + 1), displayName: name, databaseType: "text",
        nullable: true, writable: true, generated: false, origin: { database: "原库", schema: "原模式", table: "原表", column: name } })),
      editPolicy: { allowedOperations: ["insert", "update", "delete"], reasonCodes: [], columnInputKinds: {
        [id(offset + 1)]: "integer", [id(offset + 2)]: "text",
      } }, createdAt: Date.now(), expiresAt: Date.now() + 60000 } };
}
function rejected(request: ChangeSetRequest) {
  return { code: "RESULT_CONTEXT_EXPIRED", message: "结果上下文已过期，请重新查询", commitEvidence: {
    protocolVersion: 1, changeSetId: request.changeSetId, resultId: request.resultId, revision: request.revision, commitState: "notStarted",
  } };
}
function committed(request: ChangeSetRequest): CommitReceipt {
  return { protocolVersion: 1, changeSetId: request.changeSetId, resultId: request.resultId, revision: request.revision,
    connectionId: "原连接", connectionGeneration: "原代次", taskId: null, status: "committed", commitState: "committed",
    rollbackState: "notStarted", cleanupState: "succeeded", refreshState: "required", errorCode: null, reasonCodes: [], completedAt: 1,
    operationResults: request.operations.map((operation) => ({ opId: operation.opId, outcome: "applied", matchedRows: 1, affectedRows: 1, errorCode: null })) };
}
const originals = { commit: tauriService.commitChangeSet, receipt: tauriService.getChangeSetReceipt, release: tauriService.releaseResultContext };
beforeEach(() => { useGridDraftStore.setState({ drafts: {}, navigation: null }); tauriService.releaseResultContext = async () => {}; });
afterEach(() => { tauriService.commitChangeSet = originals.commit; tauriService.getChangeSetReceipt = originals.receipt; tauriService.releaseResultContext = originals.release; });
function dirty() {
  state().ensureDraft("表", source());
  state().updateDraft("表", () => ({ edits: { "0:1": "草稿" }, deletedRows: new Set([1]), newRows: [{ 0: "3", 1: "" }],
    cellIntents: { "new:0:1": "null" } }));
  return buildGridDraftChangeSet(state().drafts["表"]);
}

test("仅接受后端绑定原请求的未执行证据，错误码和错误文本不能替代证据", () => {
  const request = dirty(); const error = rejected(request);
  assert.equal(rejectedBeforeExecution(error, request)?.commitEvidence.commitState, "notStarted");
  for (const invalid of ["RESULT_CONTEXT_EXPIRED", { code: error.code, message: error.message },
    { ...error, commitEvidence: { ...error.commitEvidence, protocolVersion: 2 } },
    { ...error, commitEvidence: { ...error.commitEvidence, changeSetId: id(99) } },
    { ...error, commitEvidence: { ...error.commitEvidence, resultId: id(99) } },
    { ...error, commitEvidence: { ...error.commitEvidence, revision: 2 } },
    { ...error, commitEvidence: { ...error.commitEvidence, commitState: "unknown" } }]) {
    assert.equal(rejectedBeforeExecution(invalid, request), undefined);
  }
});

test("过期拒绝附有证明时保留草稿并可明确放弃，不再锁死导航，也不自动重发", async () => {
  const request = dirty(); let commits = 0;
  tauriService.commitChangeSet = async () => { commits++; throw rejected(request); };
  assert.equal(await state().saveChangeSetDraft("表", request, async () => source(2)), false);
  assert.equal(state().drafts["表"].saveState, "failedBeforeCommit");
  assert.equal(isGridDraftDirty(state().drafts["表"]), true);
  assert.equal(await state().saveDraft("表"), false);
  const navigation = guardGridDraft("表"); await state().chooseNavigation("discard");
  assert.equal(await navigation, true); assert.equal(isGridDraftDirty(state().drafts["表"]), false); assert.equal(commits, 1);
});

test("回执不存在或过期错误没有证明时仍未知，禁止恢复、放弃或自动重发", async () => {
  const request = dirty(); let commits = 0; let reads = 0;
  tauriService.commitChangeSet = async () => { commits++; throw { code: "RESULT_CONTEXT_EXPIRED", message: "无执行证明" }; };
  tauriService.getChangeSetReceipt = async () => { throw { code: "RECEIPT_NOT_FOUND", message: "无回执" }; };
  await state().saveChangeSetDraft("表", request, async () => { reads++; return source(2); });
  assert.equal(await state().checkChangeSetReceipt("表"), false);
  assert.equal(state().drafts["表"].saveState, "unknown");
  assert.equal(await state().reloadRejectedDraft("表"), false); assert.equal(state().confirmRecoveredDraft("表"), false);
  const navigation = guardGridDraft("表"); await state().chooseNavigation("discard"); assert.equal(await navigation, false);
  assert.equal(isGridDraftDirty(state().drafts["表"]), true); assert.equal(commits, 1); assert.equal(reads, 0);
});

test("原响应丢失后查回同ID未执行墓碑可退出未知，核查不变成重提", async () => {
  const request = dirty(); let commits = 0;
  tauriService.commitChangeSet = async () => { commits++; throw new Error("响应丢失"); };
  tauriService.getChangeSetReceipt = async () => { throw { ...rejected(request), code: "COMMIT_NOT_EXECUTED" }; };
  await state().saveChangeSetDraft("表", request, async () => source(2));
  assert.equal(state().drafts["表"].saveState, "unknown");
  assert.equal(await state().checkChangeSetReceipt("表"), false);
  assert.equal(state().drafts["表"].saveState, "failedBeforeCommit"); assert.equal(commits, 1);
});

test("重新读取按完整无损原值匹配重排后的行，确认只恢复本地输入，下一次保存使用新身份", async () => {
  const original = dirty(); const next = source(2, true); let commits = 0;
  tauriService.commitChangeSet = async (request) => { commits++; if (commits === 1) throw rejected(request); return committed(request); };
  await state().saveChangeSetDraft("表", original, async () => next);
  assert.equal(await state().reloadRejectedDraft("表"), true);
  assert.equal(state().drafts["表"].sourceResult?.resultContext?.resultId, original.resultId);
  assert.equal(commits, 1); assert.ok(state().drafts["表"].recoveryPreview);
  assert.equal(state().confirmRecoveredDraft("表"), true); assert.equal(commits, 1);
  const draft = state().drafts["表"];
  assert.equal(draft.pendingRequest, undefined); assert.equal(draft.preflightRejection, undefined);
  assert.deepEqual(draft.edits, { "1:1": "草稿" }); assert.deepEqual([...draft.deletedRows], [0]);
  assert.equal(draft.cellIntents?.["new:0:1"], "null");
  const request = buildGridDraftChangeSet(draft);
  assert.equal(request.resultId, id(2)); assert.notEqual(request.changeSetId, original.changeSetId);
  const update = request.operations.find((operation) => operation.kind === "update")!;
  if (update.kind === "update") { assert.equal(update.rowHandle, next.rowHandles![1]); assert.deepEqual(Object.keys(update.changes), [id(202)]); }
  assert.equal(await state().saveChangeSetDraft("表", request, async () => {}), true);
  assert.equal(commits, 2); assert.equal(isGridDraftDirty(state().drafts["表"]), false);
});

test("原值、来源、结构、授权、页身份任一不可核对都拒绝恢复且保留原草稿", () => {
  dirty();
  const mutations: ((result: QueryResult) => void)[] = [
    (result) => { result.typedRows![1][1] = { kind: "text", value: "其他人修改" }; },
    (result) => { result.typedRows![1][0] = null; },
    (result) => { result.rows.pop(); result.typedRows!.pop(); result.rowHandles!.pop(); },
    (result) => { result.rows.push(result.rows[1]); result.typedRows!.push(result.typedRows![1]); result.rowHandles!.push(id(88)); },
    (result) => { result.resultContext!.connectionGeneration = "新代次"; },
    (result) => { result.resultContext!.recoveryIdentity = "b".repeat(64); },
    (result) => { delete result.resultContext!.recoveryIdentity; },
    (result) => { result.resultContext!.source.schema = "其他模式"; },
    (result) => { result.resultContext!.columns[1].databaseType = "integer"; },
    (result) => { result.resultContext!.editPolicy.allowedOperations = []; },
    (result) => { result.resultContext!.expiresAt = 1; },
    (result) => { result.truncated = true; },
  ];
  for (const mutate of mutations) {
    const next = source(2, true); mutate(next);
    const before = state().drafts["表"].sourceResult;
    assert.throws(() => prepareGridDraftRecovery(state().drafts["表"], next));
    assert.equal(state().drafts["表"].sourceResult, before);
    assert.equal(isGridDraftDirty(state().drafts["表"]), true);
  }
});

test("重读失败和取消核对均保留原草稿，可以重新核对而不重提写入", async () => {
  const request = dirty(); const next = source(2, true); let reads = 0; let commits = 0;
  tauriService.commitChangeSet = async () => { commits++; throw rejected(request); };
  await state().saveChangeSetDraft("表", request, async () => {
    reads++; if (reads === 1) throw new Error("读取故障"); return next;
  });
  const original = state().drafts["表"].sourceResult;
  assert.equal(await state().reloadRejectedDraft("表"), false);
  assert.equal(state().drafts["表"].submitting, false);
  assert.equal(state().drafts["表"].sourceResult, original);
  assert.match(state().drafts["表"].error!, /读取故障/);
  assert.equal(await state().reloadRejectedDraft("表"), true);
  state().cancelDraftRecovery("表");
  assert.equal(state().drafts["表"].recoveryPreview, undefined);
  assert.equal(state().confirmRecoveredDraft("表"), false);
  assert.equal(state().drafts["表"].sourceResult, original);
  assert.equal(isGridDraftDirty(state().drafts["表"]), true);
  assert.equal(await state().reloadRejectedDraft("表"), true);
  // 确认时再次校验，预览后过期也不能将旧输入迁移到失效结果。
  next.resultContext!.expiresAt = 1;
  assert.equal(state().confirmRecoveredDraft("表"), false);
  assert.equal(state().drafts["表"].sourceResult, original);
  assert.equal(state().drafts["表"].recoveryPreview, undefined);
  assert.equal(commits, 1); assert.equal(reads, 3);
});

test("迟到重读不替换新草稿，人工备份不带旧句柄", async () => {
  const request = dirty(); let finish!: (result: QueryResult) => void; const releases: string[] = [];
  tauriService.releaseResultContext = async (resultId) => { releases.push(resultId); };
  tauriService.commitChangeSet = async () => { throw rejected(request); };
  await state().saveChangeSetDraft("表", request, () => new Promise((resolve) => { finish = resolve; }));
  const reading = state().reloadRejectedDraft("表");
  assert.equal(state().drafts["表"].submitting, true);
  useGridDraftStore.setState({ drafts: {} }); state().ensureDraft("表", source(3));
  state().updateDraft("表", () => ({ edits: { "0:1": "新的草稿" } }));
  finish(source(2, true)); assert.equal(await reading, false);
  assert.equal(state().drafts["表"].edits["0:1"], "新的草稿");
  const backup = gridDraftRecoveryText(state().drafts["表"]);
  assert.equal(backup.includes("rowHandle"), false); assert.match(backup, /不能自动重放/);
  assert.deepEqual(releases, [id(2)]);
});

test("恢复预览跨页面卸载仍保留上下文，确认后转交草稿，显式保存才写入", async () => {
  const request = dirty(); const next = source(2, true); const releases: string[] = []; let commits = 0;
  tauriService.releaseResultContext = async (resultId) => { releases.push(resultId); };
  const dataset = new DatasetController({ cancelPage: async () => {}, cancelCount: async () => {}, retire: (result) => {
    void retireResultContext(result.resultContext, Object.values(state().drafts), tauriService.releaseResultContext);
  } });
  tauriService.commitChangeSet = async (value) => { commits++; if (commits === 1) throw rejected(value); return committed(value); };
  await state().saveChangeSetDraft("表", request, async () => {
    dataset.accept(dataset.beginPage("原来源", 1), next); return next;
  });
  await state().reloadRejectedDraft("表");
  dataset.dispose();
  assert.equal(releases.includes(id(2)), false);
  // 卸载重挂只重绑回调，不能把等待确认的预览误认为无人引用。
  state().ensureDraft("表", source(3), null, undefined, async () => false, async () => source(4));
  assert.ok(state().drafts["表"].recoveryPreview);
  assert.equal(state().confirmRecoveredDraft("表"), true);
  assert.equal(commits, 1); assert.equal(releases.includes(id(2)), false);
  const recovered = buildGridDraftChangeSet(state().drafts["表"]);
  assert.equal(await state().saveChangeSetDraft("表", recovered, async () => {}), true);
  releaseGridDraft("表");
  assert.equal(commits, 2); assert.equal(releases.filter((value) => value === id(2)).length, 1);
});

test("取消恢复不释放页面仍持有的结果，最后一个页面释放后才回收；孤立预览取消即回收", async () => {
  const request = dirty(); const next = source(2, true); const releases: string[] = [];
  tauriService.releaseResultContext = async (resultId) => { releases.push(resultId); };
  const controller = () => new DatasetController({ cancelPage: async () => {}, cancelCount: async () => {}, retire: (result) => {
    void retireResultContext(result.resultContext, Object.values(state().drafts), tauriService.releaseResultContext);
  } });
  const first = controller(); const second = controller(); let candidate = next;
  tauriService.commitChangeSet = async () => { throw rejected(request); };
  await state().saveChangeSetDraft("表", request, async () => candidate);
  first.accept(first.beginPage("原来源", 1), next);
  second.accept(second.beginPage("原来源", 1), next);
  await state().reloadRejectedDraft("表"); state().cancelDraftRecovery("表");
  assert.deepEqual(releases, []);
  first.dispose(); assert.deepEqual(releases, []);
  second.dispose(); assert.deepEqual(releases, [id(2)]);
  candidate = source(3, true);
  await state().reloadRejectedDraft("表"); state().cancelDraftRecovery("表");
  assert.deepEqual(releases, [id(2), id(3)]);
  assert.equal(state().drafts["表"].sourceResult?.resultContext?.resultId, id(1));
});
