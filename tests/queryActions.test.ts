import assert from "node:assert/strict";
import { beforeEach, test } from "node:test";
import { useEditorStore } from "../src/stores/editorStore";
import { useConnectionStore } from "../src/stores/connectionStore";
import { createQueryActions } from "../src/lib/queryActions";
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
    async executeQuery(...args: [string, string, string?, number?, boolean?]): Promise<QueryResult> {
      calls.queries.push(args);
      return result();
    },
    async executeBatch(...args: [string, string | undefined, string[]]): Promise<number> {
      calls.batches.push(args);
      return 1;
    },
    async cancelQuery(...args: [string, number]) { calls.cancels.push(args); },
    async resolveTable(...args: [string, string, string[]]): Promise<ResolvedTable> {
      calls.metadata.push(args);
      return { database: args[1], schema: "other_schema", table: "users", columns };
    },
  };
  return { service, calls, actions: createQueryActions(useEditorStore, service) };
}

const tab = () => useEditorStore.getState().tabs.find((entry) => entry.id === "tab")!;
beforeEach(() => {
  useEditorStore.setState({ tabs: [{ id: "tab", title: "查询", content: "", isExecuting: false }], activeTab: "tab" });
  useConnectionStore.setState({ activeConnection: "A", selectedDatabase: "db_a" });
});

test("A 查询后切换到 B，旧结果保存和刷新始终使用 A 及完整 schema", async () => {
  const { actions, calls } = harness();
  await actions.execute("tab", "SELECT * FROM other_schema.users", A);
  const taskId = tab().resultSource!.taskId;
  useConnectionStore.setState({ activeConnection: "B", selectedDatabase: "db_b" });
  await actions.applyEdits("tab", taskId, ['UPDATE "other_schema"."users" SET id=2 WHERE id=1']);
  assert.deepEqual(calls.batches, [["A", "db_a", ['UPDATE "other_schema"."users" SET id=2 WHERE id=1']]]);
  assert.deepEqual(calls.metadata[0], ["A", "db_a", ["other_schema", "users"]]);
  assert.equal(calls.queries[1][0], "A");
  assert.equal(calls.queries[1][1], 'SELECT * FROM "other_schema"."users"');
  assert.equal(calls.queries[1][2], "db_a");
  assert.equal(calls.queries[1][4], false);
  assert.equal(tab().editContext?.connectionId, "A");
  assert.equal(tab().editContext?.schema, "other_schema");
});

test("切换同连接的数据库或清空全局选择，也不改变旧结果的保存来源", async () => {
  const { actions, calls } = harness();
  await actions.execute("tab", "SELECT * FROM users", A);
  useConnectionStore.setState({ activeConnection: "A", selectedDatabase: "other_db" });
  await actions.applyEdits("tab", tab().resultSource!.taskId, ["UPDATE users SET id=2 WHERE id=1"]);
  useConnectionStore.setState({ activeConnection: null, selectedDatabase: null });
  await actions.applyEdits("tab", tab().resultSource!.taskId, ["UPDATE users SET id=3 WHERE id=2"]);
  assert.equal(calls.batches.length, 2);
  assert.ok(calls.batches.every((call) => call[0] === "A" && call[1] === "db_a"));
});

test("显式跨库查询保存到目标库，刷新保留执行库和完整表名", async () => {
  const { service, calls, actions } = harness();
  service.resolveTable = async (...args) => {
    calls.metadata.push(args);
    return { database: "target.db", schema: null, table: "user.name", columns };
  };
  await actions.execute("tab", "SELECT * FROM `target.db`.`user.name` WHERE id=1", { ...A, driverType: "mysql" });
  await actions.applyEdits("tab", tab().resultSource!.taskId, ["UPDATE `target.db`.`user.name` SET id=2 WHERE id=1"]);
  assert.equal(calls.batches[0][1], "target.db");
  assert.equal(calls.queries[1][1], "SELECT * FROM `target.db`.`user.name` WHERE id=1");
  assert.equal(calls.queries[1][2], "db_a");
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
  useConnectionStore.setState({ activeConnection: "B", selectedDatabase: "db_b" });
  await actions.cancel("tab");
  assert.deepEqual(calls.cancels, [["A", queryId]]);
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
  assert.equal(tab().editContext?.connectionId, "B");
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
  assert.equal(tab().editContext?.schema, "new_schema");
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

test("保存进行中启动了新查询时，保存完成不会重新执行旧查询", async () => {
  const { service, calls, actions } = harness();
  const saving = deferred<number>();
  service.executeBatch = async (...args) => { calls.batches.push(args); return saving.promise; };
  await actions.execute("tab", "SELECT * FROM users", A);
  const save = actions.applyEdits("tab", tab().resultSource!.taskId, ["UPDATE users SET id=2 WHERE id=1"]);
  await actions.execute("tab", "SELECT * FROM users", B);
  saving.resolve(1);
  await save;
  assert.equal(calls.batches[0][0], "A");
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
  useEditorStore.getState().closeTab("tab");
  waiting.resolve(result());
  await running;
  assert.deepEqual(useEditorStore.getState().tabs, []);
});

test("批量执行绑定开始时的来源，不继承旧查询的取消 ID", async () => {
  const { service, calls, actions } = harness();
  await actions.execute("tab", "SELECT * FROM users", A);
  const waiting = deferred<number>();
  service.executeBatch = async (...args) => { calls.batches.push(args); return waiting.promise; };
  const running = actions.executeAll("tab", ["UPDATE users SET id=2"], A);
  useConnectionStore.setState({ activeConnection: "B", selectedDatabase: "db_b" });
  assert.equal(tab().execution?.connectionId, "A");
  assert.equal(tab().execution?.queryId, undefined);
  await actions.cancel("tab");
  assert.equal(calls.cancels.length, 0);
  waiting.resolve(1);
  await running;
  assert.equal(tab().resultSource?.connectionId, "A");
  assert.equal(tab().editContext, null);
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
