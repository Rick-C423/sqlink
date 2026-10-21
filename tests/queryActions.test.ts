import assert from "node:assert/strict";
import { beforeEach, test } from "node:test";
import { useEditorStore } from "../src/stores/editorStore";
import { useConnectionStore } from "../src/stores/connectionStore";
import { createQueryActions } from "../src/lib/queryActions";
import { useGridDraftStore, queryDraftKey } from "../src/stores/gridDraftStore";
import type { QueryResult, QueryTarget, ResolvedTable } from "../src/types/database";

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (error: unknown) => void;
  const promise = new Promise<T>((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}

const A: QueryTarget = { connectionId: "A", database: "db_a", driverType: "postgres" };
const B: QueryTarget = { connectionId: "B", database: "db_b", driverType: "postgres" };
const columns = [{ name: "id", dataType: "integer", nullable: false, isPrimaryKey: true }];
const result = (id = 1): QueryResult => ({
  columns: [{ name: "id", dataType: "integer", unifiedType: "Integer", nullable: false }],
  rows: [{ values: [id] }], executionTimeMs: 1,
});

function harness() {
  const calls = { queries: [] as unknown[][], batches: [] as unknown[][], cancels: [] as unknown[][], metadata: [] as unknown[][] };
  const service = {
    async executeQuery(...args: [string, string, string?, number?, boolean?, string?]): Promise<QueryResult> {
      calls.queries.push(args);
      return result();
    },
    async executeBatch(...args: [string, string | undefined, string[], number?, string?]): Promise<number> {
      calls.batches.push(args);
      return 1;
    },
    async cancelQuery(...args: [string, number, string?]) { calls.cancels.push(args); },
    async applyGridEdits(): Promise<never> { throw new Error("GRID_EDIT_CONTEXT_REQUIRED"); },
    async resolveTable(...args: [string, string, string[]]): Promise<ResolvedTable> {
      calls.metadata.push(args);
      return { database: args[1], schema: "other_schema", table: "users", columns };
    },
  };
  return { service, calls, actions: createQueryActions(useEditorStore, service) };
}

const tab = () => useEditorStore.getState().tabs.find((entry) => entry.id === "tab")!;
beforeEach(() => {
  useGridDraftStore.setState({ drafts: {}, navigation: null });
  useEditorStore.setState({ tabs: [{ id: "tab", title: "查询", content: "", isExecuting: false }], activeTab: "tab" });
  useConnectionStore.setState({ activeConnection: "A", selectedDatabase: "db_a" });
});

test("A 查询后切换到 B，来源仍固定到 A，旧网格写入被后端拒绝", async () => {
  const { actions, calls } = harness();
  await actions.execute("tab", "SELECT * FROM other_schema.users", A);
  const taskId = tab().resultSource!.taskId;
  useConnectionStore.setState({ activeConnection: "B", selectedDatabase: "db_b" });
  await assert.rejects(actions.applyEdits("tab", taskId, ['UPDATE "other_schema"."users" SET id=2 WHERE id=1']), /GRID_EDIT_CONTEXT_REQUIRED/);
  assert.deepEqual(calls.batches, []);
  assert.deepEqual(calls.metadata[0], ["A", "db_a", ["other_schema", "users"]]);
  assert.equal(tab().resultSource?.connectionId, "A");
  assert.equal(tab().resultSource?.refreshSql, 'SELECT * FROM "other_schema"."users"');
  assert.equal(tab().editContext, null);
});

test("切换数据库或清空全局选择，均不能重新启用旧网格写入", async () => {
  const { actions, calls } = harness();
  await actions.execute("tab", "SELECT * FROM users", A);
  useConnectionStore.setState({ activeConnection: "A", selectedDatabase: "other_db" });
  await assert.rejects(actions.applyEdits("tab", tab().resultSource!.taskId, ["UPDATE users SET id=2 WHERE id=1"]), /GRID_EDIT_CONTEXT_REQUIRED/);
  useConnectionStore.setState({ activeConnection: null, selectedDatabase: null });
  await assert.rejects(actions.applyEdits("tab", tab().resultSource!.taskId, ["UPDATE users SET id=3 WHERE id=2"]), /GRID_EDIT_CONTEXT_REQUIRED/);
  assert.equal(calls.batches.length, 0);
  assert.equal(tab().resultSource?.database, "db_a");
});

test("显式跨库查询保留完整读取身份，但不授予网格编辑权限", async () => {
  const { service, calls, actions } = harness();
  service.resolveTable = async (...args) => {
    calls.metadata.push(args);
    return { database: "target.db", schema: null, table: "user.name", columns };
  };
  await actions.execute("tab", "SELECT * FROM `target.db`.`user.name` WHERE id=1", { ...A, driverType: "mysql" });
  await assert.rejects(actions.applyEdits("tab", tab().resultSource!.taskId, ["UPDATE `target.db`.`user.name` SET id=2 WHERE id=1"]), /GRID_EDIT_CONTEXT_REQUIRED/);
  assert.equal(calls.batches.length, 0);
  assert.equal(tab().resultSource?.refreshSql, "SELECT * FROM `target.db`.`user.name` WHERE id=1");
  assert.equal(tab().resultSource?.database, "db_a");
});

test("PG 未引用标识符按服务器规则折叠大小写，引用名称保留原样", async () => {
  const { actions, calls } = harness();
  await actions.execute("tab", "SELECT * FROM Other_Schema.USERS", A);
  assert.deepEqual(calls.metadata[0], ["A", "db_a", ["other_schema", "users"]]);
  await actions.execute("tab", 'SELECT * FROM "Other_Schema"."USERS"', A);
  assert.deepEqual(calls.metadata[1], ["A", "db_a", ["Other_Schema", "USERS"]]);
  await actions.execute("tab", "SELECT * FROM ÄUSERS", A);
  assert.deepEqual(calls.metadata[2], ["A", "db_a", ["Äusers"]]);
});

test("实际查询在读取数据前固定完整表身份，刷新也使用同一份限定名", async () => {
  const { actions, calls } = harness();
  await actions.execute("tab", "SELECT users.id FROM users WHERE id=1", A);
  assert.equal(calls.queries[0][1], 'SELECT users.id FROM "other_schema"."users" WHERE id=1');
  assert.equal(tab().resultSource?.sql, "SELECT users.id FROM users WHERE id=1");
  assert.equal(tab().resultSource?.refreshSql, calls.queries[0][1]);
});

test("无法确认方言引用方式时不重写用户 SQL，也不解析成其他表", async () => {
  const { actions, calls } = harness();
  await actions.execute("tab", "SELECT * FROM [users]", A);
  assert.equal(calls.queries[0][1], "SELECT * FROM [users]");
  assert.equal(calls.metadata.length, 0);
  assert.equal(tab().editContext, null);
});

test("查询期间切换连接，取消使用运行记录的连接与查询 ID", async () => {
  const { service, calls, actions } = harness();
  const waiting = deferred<QueryResult>();
  const entered = deferred<void>();
  service.executeQuery = async (...args) => { calls.queries.push(args); entered.resolve(); return waiting.promise; };
  const running = actions.execute("tab", "SELECT * FROM users", A);
  await entered.promise;
  const queryId = tab().execution!.queryId;
  const taskToken = tab().execution!.backendTaskId;
  useConnectionStore.setState({ activeConnection: "B", selectedDatabase: "db_b" });
  await actions.cancel("tab");
  assert.deepEqual(calls.cancels, [["A", queryId, taskToken]]);
  assert.equal(tab().isExecuting, true);
  assert.equal(tab().cancelRequested, true);
  waiting.resolve(result());
  await running;
  await actions.cancel("tab");
  assert.equal(calls.cancels.length, 1);
});

test("解析表身份期间取消，不会在解析完成后继续启动 SQL", async () => {
  const { service, calls, actions } = harness();
  const metadata = deferred<ResolvedTable>();
  service.resolveTable = async () => metadata.promise;
  const running = actions.execute("tab", "SELECT * FROM users", A);
  useConnectionStore.setState({ activeConnection: "B", selectedDatabase: "db_b" });
  await actions.cancel("tab");
  assert.equal(tab().isExecuting, false);
  assert.equal(tab().result?.error, "查询已取消");
  metadata.resolve({ database: "db_a", schema: "public", table: "users", columns });
  await running;
  assert.equal(calls.queries.length, 0);
  assert.equal(calls.cancels.length, 0);
});

test("后发查询先完成后，迟到的旧查询不能覆盖结果、来源或运行状态", async () => {
  const { service, calls, actions } = harness();
  const old = deferred<QueryResult>();
  const entered = deferred<void>();
  service.executeQuery = async (...args) => {
    calls.queries.push(args);
    if (args[0] === "A") { entered.resolve(); return old.promise; }
    return result(2);
  };
  const first = actions.execute("tab", "SELECT * FROM users", A);
  await entered.promise;
  await actions.execute("tab", "SELECT * FROM users", B);
  const newSource = tab().resultSource;
  old.resolve(result(1));
  await first;
  assert.deepEqual(tab().result?.rows, [{ values: [2] }]);
  assert.equal(tab().resultSource, newSource);
  assert.equal(tab().editContext, null);
  assert.equal(tab().isExecuting, false);
});

test("迟到的旧元数据不能与新结果混合", async () => {
  const { service, actions } = harness();
  const metadata = deferred<ResolvedTable>();
  const entered = deferred<void>();
  service.resolveTable = async (connectionId, database) => {
    if (connectionId === "A") { entered.resolve(); return metadata.promise; }
    return { database, schema: "new_schema", table: "users", columns };
  };
  const first = actions.execute("tab", "SELECT * FROM users", A);
  await entered.promise;
  await actions.execute("tab", "SELECT * FROM users", B);
  metadata.resolve({ database: "db_a", schema: "old_schema", table: "users", columns });
  await first;
  assert.equal(tab().resultSource?.connectionId, "B");
  assert.equal(tab().resultSource?.refreshSql, 'SELECT * FROM "new_schema"."users"');
  assert.equal(tab().editContext, null);
});

test("旧请求失败不能清除新查询的运行状态", async () => {
  const { service, actions } = harness();
  const old = deferred<QueryResult>();
  const current = deferred<QueryResult>();
  const entered = deferred<void>();
  service.executeQuery = async (connectionId) => {
    if (connectionId === "A") { entered.resolve(); return old.promise; }
    return current.promise;
  };
  const first = actions.execute("tab", "SELECT * FROM users", A);
  await entered.promise;
  const second = actions.execute("tab", "SELECT * FROM users", B);
  old.reject(new Error("旧请求失败"));
  await first;
  assert.equal(tab().isExecuting, true);
  assert.equal(tab().execution?.connectionId, "B");
  current.resolve(result(2));
  await second;
});

test("旧结果的待提交请求不能应用到后续查询", async () => {
  const { calls, actions } = harness();
  await actions.execute("tab", "SELECT * FROM users", A);
  const oldTaskId = tab().resultSource!.taskId;
  await actions.execute("tab", "SELECT * FROM users", B);
  await assert.rejects(actions.applyEdits("tab", oldTaskId, ["UPDATE users SET id=9"]), /结果已更新/);
  assert.equal(calls.batches.length, 0);
});

test("网格拒绝写入后不会触发自动刷新或覆盖后续查询", async () => {
  const { calls, actions } = harness();
  await actions.execute("tab", "SELECT * FROM users", A);
  await assert.rejects(actions.applyEdits("tab", tab().resultSource!.taskId, ["UPDATE users SET id=2 WHERE id=1"]), /GRID_EDIT_CONTEXT_REQUIRED/);
  await actions.execute("tab", "SELECT * FROM users", B);
  assert.equal(calls.batches.length, 0);
  assert.equal(calls.queries.length, 2);
  assert.equal(tab().resultSource?.connectionId, "B");
});

test("元数据解析失败只使结果只读；执行失败清除原来的编辑来源", async () => {
  const { service, actions } = harness();
  service.resolveTable = async () => { throw new Error("表身份不明确"); };
  await actions.execute("tab", "SELECT * FROM users", A);
  assert.deepEqual(tab().result?.rows, [{ values: [1] }]);
  assert.equal(tab().editContext, null);
  service.executeQuery = async () => { throw new Error("连接已断开"); };
  await assert.rejects(actions.execute("tab", "SELECT * FROM users", B), /连接已断开/);
  assert.equal(tab().editContext, null);
  assert.equal(tab().resultSource?.connectionId, "B");
  assert.equal(tab().isExecuting, false);
});

test("关闭页签后迟到结果不会重新创建页签", async () => {
  const { service, actions } = harness();
  const waiting = deferred<QueryResult>();
  const entered = deferred<void>();
  service.executeQuery = async () => { entered.resolve(); return waiting.promise; };
  const running = actions.execute("tab", "SELECT * FROM users", A);
  await entered.promise;
  await useEditorStore.getState().closeTab("tab");
  waiting.resolve(result());
  await running;
  assert.deepEqual(useEditorStore.getState().tabs, []);
});

test("批量执行绑定开始时的来源，并使用新的取消 ID 与任务 UUID", async () => {
  const { service, calls, actions } = harness();
  await actions.execute("tab", "SELECT * FROM users", A);
  const previous = tab().resultSource!;
  const waiting = deferred<number>();
  service.executeBatch = async (...args) => { calls.batches.push(args); return waiting.promise; };
  const running = actions.executeAll("tab", ["UPDATE users SET id=2"], A);
  useConnectionStore.setState({ activeConnection: "B", selectedDatabase: "db_b" });
  assert.equal(tab().execution?.connectionId, "A");
  const execution = tab().execution!;
  assert.notEqual(execution.queryId, previous.queryId);
  assert.notEqual(execution.backendTaskId, previous.backendTaskId);
  await actions.cancel("tab");
  assert.deepEqual(calls.cancels, [["A", execution.queryId, execution.backendTaskId]]);
  waiting.resolve(1);
  await running;
  assert.equal(tab().resultSource?.connectionId, "A");
  assert.equal(tab().editContext, null);
});

test("不支持取消的驱动不会下发请求或声称任务已停止", async () => {
  const { service, calls, actions } = harness();
  const waiting = deferred<QueryResult>();
  const entered = deferred<void>();
  service.executeQuery = async () => { entered.resolve(); return waiting.promise; };
  const running = actions.execute("tab", "SELECT 1", { ...A, driverType: "mssql" });
  await entered.promise;
  assert.equal(tab().execution?.canCancel, false);
  await assert.rejects(actions.cancel("tab"), /不支持取消/);
  assert.equal(calls.cancels.length, 0);
  assert.equal(tab().isExecuting, true);
  waiting.resolve(result());
  await running;
});

test("后端能力优先，能力不可用时禁止取消且仍允许查询返回", async () => {
  const { service, calls } = harness();
  const entered = deferred<void>(); const waiting = deferred<QueryResult>();
  service.executeQuery = async () => { entered.resolve(); return waiting.promise; };
  const actions = createQueryActions(useEditorStore, { ...service, getSessionCapabilities: async () => ({ cancellation: false, cancellationReason: "未验证", exclusiveSession: true, resetPolicy: "full" as const }) });
  const running = actions.execute("tab", "SELECT 1", A);
  await entered.promise;
  await assert.rejects(actions.cancel("tab"), /不支持取消/);
  assert.equal(tab().execution?.canCancel, false);
  assert.equal(calls.cancels.length, 0);
  waiting.resolve(result()); await running;
});

test("取消请求失败保留运行状态并允许再次请求", async () => {
  const { service, actions } = harness();
  const entered = deferred<void>(); const waiting = deferred<QueryResult>(); const cancellation = deferred<void>();
  service.executeQuery = async () => { entered.resolve(); return waiting.promise; };
  service.cancelQuery = async () => cancellation.promise;
  const running = actions.execute("tab", "SELECT 1", A);
  await entered.promise;
  const cancel = actions.cancel("tab");
  assert.equal(tab().cancelRequested, true);
  assert.equal(tab().isExecuting, true);
  cancellation.reject(new Error("取消连接超时"));
  await assert.rejects(cancel, /取消连接超时/);
  assert.equal(tab().cancelRequested, false);
  assert.equal(tab().isExecuting, true);
  waiting.resolve(result()); await running;
});

test("旧任务的取消失败不能清除新任务已请求取消的状态", async () => {
  const { service, actions } = harness();
  const first = deferred<QueryResult>(); const second = deferred<QueryResult>();
  const enteredFirst = deferred<void>(); const enteredSecond = deferred<void>(); const rejectedCancel = deferred<void>();
  let queries = 0;
  service.executeQuery = async () => { if (++queries === 1) { enteredFirst.resolve(); return first.promise; } enteredSecond.resolve(); return second.promise; };
  const firstRun = actions.execute("tab", "SELECT 1", A); await enteredFirst.promise;
  const oldToken = tab().execution!.backendTaskId;
  service.cancelQuery = async (_conn, _qid, taskId) => taskId === oldToken ? rejectedCancel.promise : undefined;
  const firstCancel = actions.cancel("tab");
  const secondRun = actions.execute("tab", "SELECT 2", B); await enteredSecond.promise;
  await actions.cancel("tab"); assert.equal(tab().cancelRequested, true);
  rejectedCancel.reject(new Error("旧任务取消请求迟到失败")); await assert.rejects(firstCancel, /迟到失败/);
  assert.equal(tab().cancelRequested, true); assert.equal(tab().execution?.connectionId, "B");
  second.resolve(result(2)); await secondRun; first.resolve(result(1)); await firstRun;
  assert.equal(tab().resultSource?.connectionId, "B"); assert.deepEqual(tab().result?.rows, [{ values: [2] }]);
});

test("后端任务回执错配时拒绝展示为当前执行成功", async () => {
  const { service, actions } = harness();
  service.executeQuery = async () => ({ ...result(), task: {
    protocolVersion: 1, taskId: crypto.randomUUID(), queryId: 1, connectionId: "B", connectionGeneration: "g", operation: "query",
    phase: "succeeded", sequence: 4, cancelRequested: false, capabilities: null, sessionId: null, startedAt: 1, completedAt: 2,
    commitState: "notApplicable", rollbackState: "notStarted", cleanupState: "succeeded", errorCode: null,
  } });
  await assert.rejects(actions.execute("tab", "SELECT 1", A), /任务回执.*不一致/);
  assert.match(tab().result?.error ?? "", /任务回执/); assert.equal(tab().isExecuting, false);
});

test("同一毫秒内的执行仍使用不同的任务与取消 ID", async () => {
  const { actions, calls } = harness();
  const original = Date.now;
  try {
    Date.now = () => 1000;
    await actions.execute("tab", "SELECT * FROM users", A);
    await actions.execute("tab", "SELECT * FROM users", A);
  } finally { Date.now = original; }
  assert.notEqual(calls.queries[0][3], calls.queries[1][3]);
});

for (const sql of [
  "SELECT id + 1 AS id, name FROM users", "SELECT id, id FROM users",
  "SELECT COUNT(*) AS id FROM users", "SELECT a.id FROM users a JOIN other b ON a.id=b.id",
]) {
  test(`任意 SQL 不授予网格写权限：${sql}`, async () => {
    const { actions, calls } = harness();
    await actions.execute("tab", sql, A);
    assert.equal(tab().editContext, null);
    await assert.rejects(actions.applyEdits("tab", tab().resultSource?.taskId, ["DELETE FROM users WHERE id=2"]), /GRID_EDIT_CONTEXT_REQUIRED/);
    assert.equal(calls.batches.length, 0);
  });
}

function dirtyDraft(save: () => Promise<void> = async () => {}) {
  const key = queryDraftKey("tab");
  useGridDraftStore.getState().ensureDraft(key, result(), null, save);
  useGridDraftStore.getState().updateDraft(key, () => ({ edits: { "0:0": "9" } }));
  return key;
}

test("草稿取消重跑，不发送新查询也不清空草稿", async () => {
  const { actions, calls } = harness();
  const key = dirtyDraft();
  const running = actions.execute("tab", "SELECT * FROM users", A);
  assert.equal(calls.queries.length, 0);
  await useGridDraftStore.getState().chooseNavigation("cancel");
  await running;
  assert.equal(calls.queries.length, 0);
  assert.equal(useGridDraftStore.getState().drafts[key].edits["0:0"], "9");
});

test("草稿保存失败停止整页执行，保留数据和错误", async () => {
  const { actions, calls } = harness();
  const key = dirtyDraft(async () => { throw new Error("保存失败"); });
  const running = actions.executeAll("tab", ["DELETE FROM users"], A);
  await useGridDraftStore.getState().chooseNavigation("save");
  await running;
  assert.equal(calls.batches.length, 0);
  assert.equal(useGridDraftStore.getState().drafts[key].edits["0:0"], "9");
  assert.match(useGridDraftStore.getState().drafts[key].error!, /保存失败/);
});

test("放弃草稿后才启动新任务", async () => {
  const { actions, calls } = harness();
  dirtyDraft();
  const running = actions.execute("tab", "SELECT 1", B);
  await useGridDraftStore.getState().chooseNavigation("discard");
  await running;
  assert.equal(calls.queries.length, 1);
  assert.equal(tab().resultSource?.connectionId, "B");
});
