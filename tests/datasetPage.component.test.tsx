import assert from "node:assert/strict";
import { afterEach, beforeEach, test } from "node:test";
import { act, create, type ReactTestInstance, type ReactTestRenderer } from "react-test-renderer";
import { TableData } from "../src/components/TableView";
import { DraftNavigationDialog } from "../src/components/result/DraftNavigationDialog";
import { DataGrid } from "../src/components/result/DataGrid";
import { useI18nStore } from "../src/i18n";
import { tauriService } from "../src/services/tauriService";
import { useConnectionStore } from "../src/stores/connectionStore";
import { tableDraftKey, useGridDraftStore } from "../src/stores/gridDraftStore";
import type { DatasetPage, DatasetReadOptions, TableCountRequest, TableCountResult } from "../src/types/dataset";
import type { ControlledTableReadRequest } from "../src/types/resultContext";

const originals = {
  readTablePage: tauriService.readTablePage, countTable: tauriService.countTable,
  cancelTableTask: tauriService.cancelTableTask, releaseResultContext: tauriService.releaseResultContext,
  getSessionCapabilities: tauriService.getSessionCapabilities, executeQuery: tauriService.executeQuery,
};
const originalConnections = useConnectionStore.getState();
const originalLocale = useI18nStore.getState().locale;
const originalDocument = Object.getOwnPropertyDescriptor(globalThis, "document");
const originalHTMLElement = Object.getOwnPropertyDescriptor(globalThis, "HTMLElement");
let view: ReactTestRenderer | undefined;
let reads: { request: ControlledTableReadRequest; options?: DatasetReadOptions }[];
let counts: TableCountRequest[];
let cancellations: [string, string][];
let releases: [string, number][];
let readImpl: (request: ControlledTableReadRequest, options?: DatasetReadOptions) => Promise<DatasetPage>;
let countImpl: (request: TableCountRequest) => Promise<TableCountResult>;

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason: unknown) => void;
  const promise = new Promise<T>((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}
function page(request: ControlledTableReadRequest, resultId = `result-${request.page}`, hasMore = request.page === 1, marker?: string): DatasetPage {
  const table = request.tableParts.at(-1)!;
  const schema = request.tableParts.length === 3 ? request.tableParts[1]
    : request.tableParts.length === 2 && request.tableParts[0] !== request.database ? request.tableParts[0] : undefined;
  const columns = [{ name: "id", dataType: "bigint", unifiedType: "integer", nullable: false }, { name: "payload", dataType: "text", unifiedType: "string", nullable: false }];
  const rows = [{ values: [(request.page - 1) * request.pageSize + 1, marker ?? `第${request.page}页数据`] }];
  return {
    result: { columns, rows, executionTimeMs: 1 },
    context: {
      protocolVersion: 1, resultId, revision: 1, connectionId: request.connectionId,
      connectionGeneration: `generation-${request.connectionId}`,
      source: { database: request.database, schema, table, kind: "table" },
      columns: columns.map((column, i) => ({ columnId: `c${i}`, displayName: column.name, databaseType: column.dataType, nullable: false, writable: false, generated: false })),
      editPolicy: { allowedOperations: [], reasonCodes: ["READ_ONLY"] }, createdAt: 1, expiresAt: 100,
    },
    rowHandles: [`handle-${resultId}`],
    table: { database: request.database, schema, table, columns: columns.map((column, i) => ({ ...column, isPrimaryKey: i === 0 })) },
    totalRows: null, hasMore, nextCursor: hasMore ? `cursor-${resultId}` : null,
    paginationMode: request.page > 1 ? "keyset" : "offset",
    observedOrder: [{ column: "id", direction: "asc" }], approxBytes: 1000, truncatedReason: null,
  };
}
function countResult(request: TableCountRequest, totalRows = "999"): TableCountResult {
  return { resultId: request.resultId, revision: request.revision, connectionId: "A", connectionGeneration: "generation-A", totalRows };
}
const drafts = () => useGridDraftStore.getState();
const draftKey = tableDraftKey("A", "db", "items");
function text(node: ReactTestInstance): string {
  return node.children.map((child) => typeof child === "string" ? child : text(child)).join("");
}
function visible() { return JSON.stringify(view!.toJSON()); }
function button(label: string): ReactTestInstance {
  const found = view!.root.findAllByType("button").find((node) => text(node) === label);
  assert.ok(found, `应存在按钮：${label}`); return found;
}
async function click(label: string) { await act(async () => { await button(label).props.onClick(); }); }
function launch(label: string) {
  let result: unknown;
  act(() => { result = button(label).props.onClick(); });
  return result;
}
async function mount(props = { connectionId: "A", database: "db", table: "items" }) {
  await act(async () => { view = create(<><TableData {...props} /><DraftNavigationDialog /></>); });
}

beforeEach(() => {
  reads = []; counts = []; cancellations = []; releases = [];
  useGridDraftStore.setState({ drafts: {}, navigation: null });
  useConnectionStore.setState({
    saved: ["A", "B"].map((id) => ({ id, name: id, driverType: id === "B" ? "postgres" : "mysql", host: "127.0.0.1", port: id === "B" ? 5432 : 3306, user: "test", password: "", database: "db" })),
    liveIds: new Set(), connectionEpochs: {},
  });
  useI18nStore.setState({ locale: "zh-CN" });
  Object.defineProperty(globalThis, "HTMLElement", { configurable: true, value: class {} });
  Object.defineProperty(globalThis, "document", { configurable: true, value: { activeElement: null } });
  readImpl = async (request) => page(request);
  countImpl = async (request) => countResult(request);
  tauriService.readTablePage = (request, options) => { reads.push({ request, options }); return readImpl(request, options); };
  tauriService.countTable = (request) => { counts.push(request); return countImpl(request); };
  tauriService.cancelTableTask = async (connectionId, taskId) => { cancellations.push([connectionId, taskId]); };
  tauriService.releaseResultContext = async (resultId, revision) => { releases.push([resultId, revision]); };
  tauriService.getSessionCapabilities = async () => ({ cancellation: true, cancellationReason: null, exclusiveSession: true, resetPolicy: "full" });
  tauriService.executeQuery = async () => assert.fail("受控取页和独立统计不能回退原始SQL");
});
afterEach(async () => {
  if (drafts().navigation && !drafts().navigation!.saving) await act(async () => { await drafts().chooseNavigation("cancel"); });
  act(() => { view?.unmount(); }); view = undefined;
  Object.assign(tauriService, originals);
  useGridDraftStore.setState({ drafts: {}, navigation: null });
  useConnectionStore.setState(originalConnections);
  useI18nStore.setState({ locale: originalLocale });
  if (originalDocument) Object.defineProperty(globalThis, "document", originalDocument); else Reflect.deleteProperty(globalThis, "document");
  if (originalHTMLElement) Object.defineProperty(globalThis, "HTMLElement", originalHTMLElement); else Reflect.deleteProperty(globalThis, "HTMLElement");
});

test("首屏直接显示可信页且不自动COUNT，未知总数仍按hasMore续页并在末页禁用下一页", async () => {
  await mount();
  assert.match(visible(), /第1页数据/);
  assert.match(visible(), /总行数未统计/);
  assert.equal(counts.length, 0);
  assert.equal(button("下一页 ›").props.disabled, false);
  assert.ok(reads[0].options?.taskId);
  await click("下一页 ›");
  assert.equal(reads[1].request.page, 2);
  assert.equal(reads[1].options?.cursor, "cursor-result-1");
  assert.notEqual(reads[1].options?.taskId, reads[0].options?.taskId);
  assert.match(visible(), /第2页数据/);
  assert.match(visible(), /游标续页/);
  assert.equal(button("下一页 ›").props.disabled, true);
  assert.equal(counts.length, 0);
  assert.equal(drafts().drafts[draftKey].sourceResult?.resultContext?.resultId, "result-2");
});

test("独立COUNT挂起不挡当前页和翻页，旧统计迟到不能回写新页", async () => {
  const pending = deferred<TableCountResult>(); countImpl = () => pending.promise;
  await mount(); launch("统计总行数");
  assert.equal(counts.length, 1);
  assert.match(visible(), /第1页数据/);
  assert.equal(button("下一页 ›").props.disabled, false);
  await click("下一页 ›");
  assert.match(visible(), /第2页数据/);
  assert.deepEqual(cancellations, [["A", counts[0].taskId]]);
  await act(async () => { pending.resolve(countResult(counts[0], "987654321")); await pending.promise; });
  assert.doesNotMatch(visible(), /987,654,321|987654321/);
  assert.match(visible(), /总行数未统计/);
  assert.equal(drafts().drafts[draftKey].sourceResult?.resultContext?.resultId, "result-2");
});

test("COUNT失败保留可信结果、行身份及分页能力", async () => {
  countImpl = async () => { throw { code: "COUNT_FAILED", message: "统计暂不可用" }; };
  await mount(); const previous = drafts().drafts[draftKey].sourceResult;
  await click("统计总行数");
  assert.match(visible(), /统计暂不可用/);
  assert.equal(drafts().drafts[draftKey].sourceResult, previous);
  assert.equal(previous?.resultContext?.resultId, "result-1");
  assert.match(visible(), /第1页数据/);
  assert.equal(button("下一页 ›").props.disabled, false);
  assert.deepEqual(releases, []);
});

test("强制刷新失败保留旧页展示但不再使用已释放身份编辑或统计，重试成功恢复", async () => {
  await mount();
  readImpl = async () => { throw new Error("刷新连接失败"); };
  await click("刷新");
  assert.match(visible(), /第1页数据/);
  assert.match(visible(), /刷新连接失败/);
  assert.deepEqual(releases, [["result-1", 1]]);
  assert.match(view!.root.findByType(DataGrid).props.readOnlyReason, /身份已失效/);
  assert.equal(button("统计总行数").props.disabled, true);
  readImpl = async (request) => page(request, "refreshed");
  await click("刷新");
  assert.equal(view!.root.findByType(DataGrid).props.readOnlyReason, undefined);
  assert.equal(button("统计总行数").props.disabled, false);
  assert.equal(drafts().drafts[draftKey].sourceResult?.resultContext?.resultId, "refreshed");
});

test("统计取消固定原taskId，只标已请求直到收到取消终态", async () => {
  const pending = deferred<TableCountResult>(); countImpl = () => pending.promise;
  await mount(); launch("统计总行数");
  await click("取消统计");
  assert.deepEqual(cancellations, [["A", counts[0].taskId]]);
  assert.match(visible(), /已请求取消/);
  assert.match(visible(), /统计中/);
  assert.match(visible(), /第1页数据/);
  assert.doesNotMatch(visible(), /统计已取消/);
  await act(async () => { pending.reject({ code: "TABLE_TASK_CANCELLED", message: "统计已取消" }); await pending.promise.catch(() => undefined); });
  assert.match(visible(), /统计已取消/);
  assert.doesNotMatch(visible(), /已请求取消/);
  assert.equal(drafts().drafts[draftKey].sourceResult?.resultContext?.resultId, "result-1");
});

test("读取取消使用原页taskId，取消回执到达前不宣称已取消", async () => {
  const pending = deferred<DatasetPage>(); readImpl = () => pending.promise;
  await mount();
  await click("取消读取");
  assert.deepEqual(cancellations, [["A", reads[0].options!.taskId]]);
  assert.match(visible(), /已请求取消/);
  assert.doesNotMatch(visible(), /读取已取消/);
  await act(async () => { pending.reject({ code: "TABLE_TASK_CANCELLED", message: "读取已取消" }); await pending.promise.catch(() => undefined); });
  assert.match(visible(), /读取已取消/);
  assert.doesNotMatch(visible(), /已请求取消/);
});

test("完整来源切换后旧页迟到仅释放，连接和schema表身份保持新来源", async () => {
  const pending = deferred<DatasetPage>();
  readImpl = async (request) => reads.length === 1 ? pending.promise : page(request, "new-source", false, "新来源数据");
  await mount();
  await act(async () => { view!.update(<><TableData connectionId="B" database="other_db" schema="second" table="items" /><DraftNavigationDialog /></>); });
  assert.deepEqual(reads[1].request.tableParts, ["second", "items"]);
  assert.equal(reads[1].request.database, "other_db");
  assert.equal(reads[1].request.connectionId, "B");
  await act(async () => { pending.resolve(page(reads[0].request, "old-source", true, "旧来源迟到")); await pending.promise; });
  assert.match(visible(), /新来源数据/); assert.doesNotMatch(visible(), /旧来源迟到/);
  assert.ok(releases.some(([id]) => id === "old-source"));
  assert.equal(drafts().drafts[tableDraftKey("B", "other_db", "items", "second")].sourceResult?.resultContext?.resultId, "new-source");
});

test("相同连接ID的epoch重连隔离旧页与旧COUNT，不恢复旧总数", async () => {
  const pendingCount = deferred<TableCountResult>(); countImpl = () => pendingCount.promise;
  await mount(); launch("统计总行数");
  const pendingRead = deferred<DatasetPage>(); readImpl = () => pendingRead.promise;
  await click("下一页 ›");
  await act(async () => { useConnectionStore.getState().markLive("A", false); });
  readImpl = async (request) => page(request, "new-epoch", false, "新会话数据");
  await act(async () => { useConnectionStore.getState().markLive("A", true); });
  await act(async () => {
    pendingRead.resolve(page(reads[1].request, "old-epoch", true, "旧会话迟到"));
    pendingCount.resolve(countResult(counts[0], "888888"));
    await Promise.all([pendingRead.promise, pendingCount.promise]);
  });
  assert.match(visible(), /新会话数据/);
  assert.doesNotMatch(visible(), /旧会话迟到|888,888|888888/);
  assert.ok(releases.some(([id]) => id === "old-epoch"));
  assert.equal(drafts().drafts[draftKey].sourceResult?.resultContext?.resultId, "new-epoch");
});

test("脏草稿阻止翻页，取消保留原结果和游标，放弃后才发续页", async () => {
  await mount();
  act(() => { drafts().updateDraft(draftKey, () => ({ edits: { "0:1": "未保存内容" } })); });
  const navigation = launch("下一页 ›");
  assert.equal(reads.length, 1);
  assert.deepEqual(drafts().navigation?.keys, [draftKey]);
  await click("取消"); await navigation;
  assert.equal(reads.length, 1);
  assert.equal(drafts().drafts[draftKey].edits["0:1"], "未保存内容");
  assert.equal(drafts().drafts[draftKey].sourceResult?.resultContext?.resultId, "result-1");
  assert.deepEqual(releases, []);
  const accepted = launch("下一页 ›");
  await click("放弃改动"); await accepted;
  assert.equal(reads.length, 2);
  assert.equal(reads[1].options?.cursor, "cursor-result-1");
  assert.match(visible(), /第2页数据/);
});
