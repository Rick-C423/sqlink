import assert from "node:assert/strict";
import { afterEach, beforeEach, test } from "node:test";
import { act, create, type ReactTestInstance, type ReactTestRenderer } from "react-test-renderer";
import { ExportSqlDialog } from "../src/components/connection/ExportSqlDialog";
import { tauriService } from "../src/services/tauriService";
import { useI18nStore } from "../src/i18n";
import type { DumpSummary, TableInfo } from "../src/types/database";
import type { SessionCapabilities, TaskSnapshot } from "../src/types/task";

const original = {
  getTables: tauriService.getTables,
  getSessionCapabilities: tauriService.getSessionCapabilities,
  getTaskStatus: tauriService.getTaskStatus,
  dumpSqlFile: tauriService.dumpSqlFile,
  exportTableCsv: tauriService.exportTableCsv,
  cancelExport: tauriService.cancelExport,
};
const originalWindow = Object.getOwnPropertyDescriptor(globalThis, "window");
const originalLocale = useI18nStore.getState().locale;
const supported: SessionCapabilities = {
  cancellation: true, cancellationReason: null, exclusiveSession: true, resetPolicy: "full",
};
let view: ReactTestRenderer | undefined;
let invoke: (command: string, args: Record<string, unknown>) => Promise<unknown>;
let calls: Parameters<typeof tauriService.dumpSqlFile>[];
let cancellations: [string, string][];
let tableRequests: [string, string | undefined][];
let closed: number;
let result: ReturnType<typeof deferred<DumpSummary>>;

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason: Error) => void;
  const promise = new Promise<T>((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}

function snapshot(taskId: string, phase: TaskSnapshot["phase"] = "running"): TaskSnapshot {
  return {
    protocolVersion: 1, taskId, queryId: null, connectionId: "A", connectionGeneration: "generation-A",
    operation: "sqlDump", phase, sequence: 1, cancelRequested: false, capabilities: null,
    sessionId: null, startedAt: 1, completedAt: null, commitState: "notStarted",
    rollbackState: "notStarted", cleanupState: "notStarted", errorCode: null,
  };
}

beforeEach(() => {
  calls = []; cancellations = []; tableRequests = []; closed = 0;
  result = deferred<DumpSummary>();
  useI18nStore.setState({ locale: "zh-CN" });
  invoke = async (command) => {
    assert.equal(command, "plugin:dialog|save");
    return "C:/exports/original.sql";
  };
  Object.defineProperty(globalThis, "window", { configurable: true, value: {
    __TAURI_INTERNALS__: { invoke: (command: string, args: Record<string, unknown>) => invoke(command, args) },
  } });
  tauriService.getTables = async (connectionId, database) => {
    tableRequests.push([connectionId, database]);
    return [{ name: "原表" }];
  };
  tauriService.getSessionCapabilities = async () => supported;
  tauriService.getTaskStatus = async (taskId) => snapshot(taskId);
  tauriService.dumpSqlFile = (...args) => { calls.push(args); return result.promise; };
  tauriService.cancelExport = async (connectionId, taskId) => { cancellations.push([connectionId, taskId]); };
});

afterEach(() => {
  act(() => view?.unmount()); view = undefined;
  Object.assign(tauriService, original);
  useI18nStore.setState({ locale: originalLocale });
  if (originalWindow) Object.defineProperty(globalThis, "window", originalWindow);
  else Reflect.deleteProperty(globalThis, "window");
});

function text(node?: ReactTestInstance): string {
  if (!node) return JSON.stringify(view!.toJSON());
  return node.children.map((child) => typeof child === "string" ? child : text(child)).join("");
}

function button(label: string) {
  const found = view!.root.findAllByType("button").find((node) => text(node) === label);
  assert.ok(found, `缺少按钮：${label}`);
  return found;
}

async function click(label: string) {
  await act(async () => { await button(label).props.onClick(); });
}

function dialog(connectionId = "A", database = "dbA") {
  return <ExportSqlDialog open connectionId={connectionId} database={database} onClose={() => { closed++; }} />;
}

async function mountAndPickPath() {
  await act(async () => { view = create(dialog()); });
  await click("选择…");
}

async function start() {
  let pending!: Promise<void>;
  await act(async () => { pending = button("开始导出").props.onClick(); });
  return { pending };
}

test("导出IPC透传可选taskId，取消按固定连接与任务调用专用命令", async () => {
  Object.assign(tauriService, original);
  const requests: [string, Record<string, unknown>][] = [];
  invoke = async (command, args) => {
    requests.push([command, args]);
    return command === "sql_dump_file" ? { tables: 1, rows: 2 } : 2;
  };
  await tauriService.dumpSqlFile("A", "dbA", ["t"], "a.sql", true, false, "sql-job");
  await tauriService.exportTableCsv("B", "dbB", "t", "b.csv", "csv-job");
  await tauriService.cancelExport("A", "sql-job");
  assert.equal(requests[0][0], "sql_dump_file");
  assert.equal(requests[0][1].taskId, "sql-job");
  assert.equal(requests[1][0], "table_export_csv");
  assert.equal(requests[1][1].taskId, "csv-job");
  assert.deepEqual(requests[2], ["export_cancel", { connectionId: "A", taskId: "sql-job" }]);
  await tauriService.exportTableCsv("A", undefined, "t", "legacy.csv");
  assert.equal(requests[3][1].taskId, undefined);
});

test("运行任务固定来源和UUID，切换外部连接不改变标题、选择、取消对象或完成路径", async () => {
  await mountAndPickPath();
  const { pending } = await start();
  assert.equal(calls.length, 1);
  const taskId = calls[0][6]!;
  assert.match(taskId, /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i);
  assert.deepEqual(calls[0].slice(0, 6), ["A", "dbA", undefined, "C:/exports/original.sql", true, true]);
  assert.deepEqual(calls[0][7], [{ database: "dbA", schema: undefined, table: "原表" }]);
  await act(async () => { view!.update(dialog("B", "dbB")); });
  assert.match(text(), /转储 SQL 文件 ← dbA/);
  assert.doesNotMatch(text(), /dbB/);
  assert.deepEqual(tableRequests, [["A", "dbA"]]);
  for (const label of ["全选", "全不选", "取消", "选择…"]) assert.equal(button(label).props.disabled, true);
  act(() => { button("全不选").props.onClick(); button("✕").props.onClick(); button("取消").props.onClick(); });
  assert.equal(closed, 0);
  assert.match(text(), /选择表（1\/1）/);
  await click("取消导出");
  assert.deepEqual(cancellations, [["A", taskId]]);
  await act(async () => { result.resolve({ tables: 1, rows: 3 }); await pending; });
  assert.match(text(), /导出完成/);
  assert.match(text(), /original.sql/);
  assert.match(text(), /转储 SQL 文件 ← dbA/);
});

test("取消请求成功以及取消终态快照都不能提前显示完成，必须等待原导出结果", async () => {
  tauriService.getTaskStatus = async (taskId) => snapshot(taskId, "cancelled");
  await mountAndPickPath();
  const { pending } = await start();
  assert.match(text(), /正在等待导出结果确认/);
  assert.doesNotMatch(text(), /已取消|导出完成/);
  await click("取消导出");
  assert.match(text(), /已请求取消，等待导出结果确认/);
  assert.doesNotMatch(text(), /已取消|导出完成/);
  assert.equal(button("取消导出").props.disabled, true);
  assert.equal(button("取消").props.disabled, true);
  await act(async () => { result.reject(new Error("查询已取消")); await pending; });
  assert.match(text(), /查询已取消/);
  assert.doesNotMatch(text(), /导出完成/);
  assert.equal(button("取消").props.disabled, false);
});

test("后端拒绝取消时显示实际错误，导出继续等待且不能关闭", async () => {
  tauriService.cancelExport = async () => { throw new Error("任务正在提交文件，拒绝取消"); };
  tauriService.getTaskStatus = async (taskId) => snapshot(taskId, "committing");
  await mountAndPickPath();
  const { pending } = await start();
  assert.match(text(), /正在提交导出文件/);
  await click("取消导出");
  assert.match(text(), /请求取消失败.*任务正在提交文件，拒绝取消/);
  assert.doesNotMatch(text(), /已请求取消|已取消|导出完成/);
  assert.equal(button("取消导出").props.disabled, false);
  assert.equal(button("取消").props.disabled, true);
  await act(async () => { result.resolve({ tables: 1, rows: 3 }); await pending; });
  assert.match(text(), /导出完成/);
});

test("不支持运行中取消的驱动禁用取消，不能通过按钮回调绕过能力限制", async () => {
  tauriService.getSessionCapabilities = async () => ({ ...supported, cancellation: false, cancellationReason: "SQL Server 暂不支持运行中取消" });
  await mountAndPickPath();
  await start();
  assert.equal(button("取消导出").props.disabled, true);
  assert.match(text(), /SQL Server 暂不支持运行中取消/);
  await click("取消导出");
  assert.equal(cancellations.length, 0);
  assert.doesNotMatch(text(), /已取消|导出完成/);
});

test("旧表清单迟到不覆盖新连接数据库，旧加载期间不能导出新来源", async () => {
  const old = deferred<TableInfo[]>();
  const next = deferred<TableInfo[]>();
  tauriService.getTables = (connectionId, database) => {
    tableRequests.push([connectionId, database]);
    return connectionId === "A" ? old.promise : next.promise;
  };
  await mountAndPickPath();
  await act(async () => { view!.update(dialog("B", "dbB")); });
  assert.equal(button("开始导出").props.disabled, true);
  await act(async () => { next.resolve([{ name: "新来源表" }]); });
  assert.equal(button("开始导出").props.disabled, false);
  await act(async () => { old.resolve([{ name: "旧来源迟到表" }]); });
  assert.match(text(), /新来源表/);
  assert.doesNotMatch(text(), /旧来源迟到表/);
  await start();
  assert.equal(calls[0][0], "B");
  assert.equal(calls[0][1], "dbB");
});

test("旧连接取消能力迟到不覆盖新来源能力，重试创建新的任务ID", async () => {
  const oldCapabilities = deferred<SessionCapabilities>();
  tauriService.getSessionCapabilities = (connectionId) => connectionId === "A" ? oldCapabilities.promise : Promise.resolve(supported);
  await mountAndPickPath();
  await act(async () => { view!.update(dialog("B", "dbB")); });
  await act(async () => { oldCapabilities.resolve({ ...supported, cancellation: false }); });
  const { pending } = await start();
  assert.equal(button("取消导出").props.disabled, false);
  const firstTask = calls[0][6];
  await act(async () => { result.reject(new Error("模拟导出失败")); await pending; });
  result = deferred<DumpSummary>();
  await start();
  assert.notEqual(calls[1][6], firstTask);
  assert.equal(calls[1][0], "B");
});

test("任务已完成后迟到的取消错误不能覆盖成功结果", async () => {
  const cancel = deferred<void>();
  tauriService.cancelExport = () => cancel.promise;
  await mountAndPickPath();
  const { pending } = await start();
  let cancelling!: Promise<void>;
  await act(async () => { cancelling = button("取消导出").props.onClick(); });
  await act(async () => { result.resolve({ tables: 1, rows: 3 }); await pending; });
  await act(async () => { cancel.reject(new Error("迟到取消错误")); await cancelling; });
  assert.match(text(), /导出完成/);
  assert.doesNotMatch(text(), /迟到取消错误/);
});

test("同一帧重复请求取消只发送一次，等待中的取消不允许退出", async () => {
  const cancel = deferred<void>();
  tauriService.cancelExport = (connectionId, taskId) => {
    cancellations.push([connectionId, taskId]);
    return cancel.promise;
  };
  await mountAndPickPath();
  await start();
  const handler = button("取消导出").props.onClick;
  let first!: Promise<void>;
  let second!: Promise<void>;
  act(() => { first = handler(); second = handler(); });
  assert.equal(cancellations.length, 1);
  assert.equal(button("取消").props.disabled, true);
  await act(async () => { cancel.resolve(); await first; await second; });
  assert.match(text(), /已请求取消，等待导出结果确认/);
  assert.doesNotMatch(text(), /已取消|导出完成/);
});

test("阶段读取暂时失败不伪造进度，未知取消能力保持禁用", async () => {
  tauriService.getSessionCapabilities = async () => { throw new Error("能力读取失败"); };
  tauriService.getTaskStatus = async () => { throw new Error("任务暂未登记"); };
  await mountAndPickPath();
  await start();
  assert.equal(button("取消导出").props.disabled, true);
  assert.match(text(), /暂时无法读取任务阶段/);
  assert.doesNotMatch(text(), /%|已取消|导出完成/);
  await click("取消导出");
  assert.equal(cancellations.length, 0);
});

test("同名跨 schema 表独立选择，全选也向后端冻结精确清单", async () => {
  tauriService.getTables = async () => [{ name: "a.b", schema: "one" }, { name: "a.b", schema: "two" }];
  await mountAndPickPath();
  assert.match(text(), /选择表（2\/2）/);
  assert.match(text(), /one.a.b/); assert.match(text(), /two.a.b/);
  const first = view!.root.findAllByType("label").find((node) => text(node) === "one.a.b")!.findByType("input");
  act(() => { first.props.onChange(); });
  assert.match(text(), /选择表（1\/2）/);
  let execution = await start();
  assert.equal(calls[0][2], undefined);
  assert.deepEqual(calls[0][7], [{ database: "dbA", schema: "two", table: "a.b" }]);
  await act(async () => { result.reject(new Error("重试全选")); await execution.pending; });
  await click("全选"); result = deferred<DumpSummary>(); execution = await start();
  assert.equal(calls[1][2], undefined);
  assert.deepEqual(calls[1][7], [{ database: "dbA", schema: "one", table: "a.b" }, { database: "dbA", schema: "two", table: "a.b" }]);
  await act(async () => { view!.update(dialog("B", "other")); });
  assert.deepEqual(calls[1][7], [{ database: "dbA", schema: "one", table: "a.b" }, { database: "dbA", schema: "two", table: "a.b" }]);
  await act(async () => { result.resolve({ tables: 2, rows: 0 }); await execution.pending; });
});
