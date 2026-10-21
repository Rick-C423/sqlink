import assert from "node:assert/strict";
import { afterEach, beforeEach, test } from "node:test";
import { act, create, type ReactTestInstance, type ReactTestRenderer } from "react-test-renderer";
import { TableData } from "../src/components/TableView";
import { DraftNavigationDialog } from "../src/components/result/DraftNavigationDialog";
import { useI18nStore } from "../src/i18n";
import { tauriService } from "../src/services/tauriService";
import { useConnectionStore } from "../src/stores/connectionStore";
import { useEditorStore } from "../src/stores/editorStore";
import { isGridDraftDirty, queryDraftKey, tableDraftKey, useGridDraftStore } from "../src/stores/gridDraftStore";
import { useWorkspaceStore } from "../src/stores/workspaceStore";
import type { QueryResult, ResolvedTable } from "../src/types/database";
import type { ControlledTablePage, ControlledTableReadRequest } from "../src/types/resultContext";

const tableKey = tableDraftKey("A", "db", "items");
const queryKey = queryDraftKey("editor-A");
const source: QueryResult = {
  columns: [
    { name: "tenant_id", dataType: "int", unifiedType: "integer", nullable: false },
    { name: "id", dataType: "int", unifiedType: "integer", nullable: false },
    { name: "name", dataType: "text", unifiedType: "string", nullable: false },
  ],
  rows: [{ values: [1, 2, "原始名称"] }], executionTimeMs: 1,
};
const metadata: ResolvedTable = {
  database: "db", table: "items",
  columns: source.columns.map((column) => ({
    name: column.name, dataType: column.dataType, nullable: false,
    isPrimaryKey: column.name !== "name",
  })),
};
const originalReadTablePage = tauriService.readTablePage;
const originalReleaseResultContext = tauriService.releaseResultContext;
const originalResolveTable = tauriService.resolveTable;
const originalExecuteQuery = tauriService.executeQuery;
const originalDocument = Object.getOwnPropertyDescriptor(globalThis, "document");
const originalHTMLElement = Object.getOwnPropertyDescriptor(globalThis, "HTMLElement");
const originalLocale = useI18nStore.getState().locale;
const originalConnections = useConnectionStore.getState();
let view: ReactTestRenderer | undefined;
let requests: ControlledTableReadRequest[];
let releases: [string, number][];
let resolves: number;
function controlledPage(resultId: string): ControlledTablePage {
  return {
    result: { ...source, rows: source.rows.map((row) => ({ values: [...row.values] })) },
    context: {
      protocolVersion: 1, resultId, revision: 1, connectionId: "A", connectionGeneration: "generation-A",
      source: { database: "db", table: "items", kind: "table" },
      columns: source.columns.map((column, index) => ({ columnId: `c${index}`, displayName: column.name, databaseType: column.dataType, nullable: column.nullable, writable: false, generated: false })),
      editPolicy: { allowedOperations: [], reasonCodes: ["READ_ONLY"] }, createdAt: 1, expiresAt: 100,
    },
    rowHandles: ["opaque-row-1"], table: metadata, totalRows: 80,
  };
}
const drafts = () => useGridDraftStore.getState();
const workspace = () => useWorkspaceStore.getState();
const editor = () => useEditorStore.getState();

beforeEach(() => {
  requests = []; releases = []; resolves = 0;
  useGridDraftStore.setState({ drafts: {}, navigation: null });
  useWorkspaceStore.setState({ tabs: [], activeId: null });
  useEditorStore.setState({ tabs: [], activeTab: null });
  useConnectionStore.setState({ saved: [{ id: "A", name: "测试连接", driverType: "mysql", host: "localhost", port: 3306, user: "test", password: "" }], liveIds: new Set(), connectionEpochs: {} });
  useI18nStore.setState({ locale: "zh-CN" });
  // renderer 不创建 DOM 节点，只补齐真实对话框读取的焦点接口。
  Object.defineProperty(globalThis, "HTMLElement", { configurable: true, value: class {} });
  Object.defineProperty(globalThis, "document", { configurable: true, value: { activeElement: null } });
  tauriService.resolveTable = async (connectionId, database, parts) => {
    assert.equal(connectionId, "A"); assert.equal(database, "db"); assert.deepEqual(parts, ["db", "items"]);
    resolves++;
    return metadata;
  };
  tauriService.readTablePage = async (request) => {
    requests.push(request);
    return controlledPage(`result-${requests.length}`);
  };
  tauriService.releaseResultContext = async (resultId, revision) => { releases.push([resultId, revision]); };
  tauriService.executeQuery = async () => { assert.fail("无筛选读取不能使用原始 SQL 查询"); };
});

afterEach(async () => {
  // 失败的断言也不能把未决导航留给后面的测试。
  if (drafts().navigation && !drafts().navigation!.saving) await drafts().chooseNavigation("cancel");
  act(() => view?.unmount()); view = undefined;
  tauriService.readTablePage = originalReadTablePage;
  tauriService.releaseResultContext = originalReleaseResultContext;
  tauriService.resolveTable = originalResolveTable;
  tauriService.executeQuery = originalExecuteQuery;
  useGridDraftStore.setState({ drafts: {}, navigation: null });
  useWorkspaceStore.setState({ tabs: [], activeId: null });
  useEditorStore.setState({ tabs: [], activeTab: null });
  useConnectionStore.setState(originalConnections);
  useI18nStore.setState({ locale: originalLocale });
  if (originalDocument) Object.defineProperty(globalThis, "document", originalDocument); else Reflect.deleteProperty(globalThis, "document");
  if (originalHTMLElement) Object.defineProperty(globalThis, "HTMLElement", originalHTMLElement); else Reflect.deleteProperty(globalThis, "HTMLElement");
});

function text(node: ReactTestInstance): string {
  return node.children.map((child) => typeof child === "string" ? child : text(child)).join("");
}
function button(label: string): ReactTestInstance {
  const found = view!.root.findAllByType("button").find((node) => text(node) === label);
  assert.ok(found, `应存在按钮：${label}`);
  return found;
}
function launch(action: () => unknown): unknown {
  let result: unknown;
  act(() => { result = action(); });
  return result;
}
async function choose(label: string) {
  await act(async () => { await button(label).props.onClick(); });
}
async function mountTable() {
  await act(async () => { view = create(<><TableData connectionId="A" database="db" table="items" /><DraftNavigationDialog /></>); });
  assert.equal(requests.length, 1);
  assert.equal(resolves, 0);
  assert.match(JSON.stringify(view!.toJSON()), /第 1 \/ 4 页/);
}
function injectDirty(key = tableKey, save?: () => Promise<void>) {
  if (!drafts().drafts[key]) drafts().ensureDraft(key, source, null, save);
  act(() => drafts().updateDraft(key, () => ({ edits: { "0:2": "未保存名称" }, ...(save ? { save } : {}) })));
  assert.equal(isGridDraftDirty(drafts().drafts[key]), true);
}
function assertNoReload() {
  assert.equal(requests.length, 1);
  assert.equal(resolves, 0);
  assert.equal(drafts().drafts[tableKey].edits["0:2"], "未保存名称");
}
function assertReleased(expected: [string, number][]) {
  // 页面与草稿 store 都可主动回收同一结果，后端释放接口保证幂等。
  assert.deepEqual([...new Set(releases.map((entry) => JSON.stringify(entry)))].sort(),
    [...new Set(expected.map((entry) => JSON.stringify(entry)))].sort());
}
function nameHeader() {
  const header = view!.root.findAllByType("th").find((node) => text(node).includes("name"));
  assert.ok(header);
  return header;
}
function openTabs() {
  useWorkspaceStore.setState({ tabs: [
    { id: "table-A", kind: "table", title: "items", connectionId: "A", database: "db", table: "items" },
    { id: "query-A", kind: "query", title: "查询", connectionId: "A", editorTabId: "editor-A" },
  ], activeId: "query-A" });
  useEditorStore.setState({ tabs: [{ id: "editor-A", title: "查询", content: "SELECT 1", isExecuting: false, result: source }], activeTab: "editor-A" });
  act(() => { view = create(<DraftNavigationDialog />); });
}

test("表数据真实列排序：取消保留草稿，放弃后才发送结构化排序请求", async () => {
  await mountTable(); injectDirty();
  launch(() => nameHeader().props.onClick());
  assert.deepEqual(drafts().navigation?.keys, [tableKey]);
  assertNoReload();
  await choose("取消");
  assertNoReload();
  assert.doesNotMatch(JSON.stringify(view!.toJSON()), /▲/);
  launch(() => nameHeader().props.onClick());
  assertNoReload();
  await choose("放弃改动");
  assert.equal(requests.length, 2);
  assert.equal(resolves, 0);
  assert.deepEqual(requests[1], { protocolVersion: 1, connectionId: "A", database: "db", tableParts: ["db", "items"], page: 1, pageSize: 20, sort: { column: "name", direction: "asc" } });
  assert.equal(isGridDraftDirty(drafts().drafts[tableKey]), false);
});

test("表数据下一页：真实只读保存失败后不发请求，页码及草稿保持原样", async () => {
  await mountTable(); injectDirty();
  const paging = launch(() => button("下一页 ›").props.onClick());
  assertNoReload();
  await choose("保存并继续"); await paging;
  assertNoReload();
  assert.match(JSON.stringify(view!.toJSON()), /第 1 \/ 4 页/);
  assert.equal(drafts().drafts[tableKey].submitting, false);
  assert.match(drafts().drafts[tableKey].error!, /只读|列来源|行身份/);
  assert.equal(drafts().navigation, null);
});

test("表数据刷新和每页条数：取消不重载或改条数，放弃后才按新条数查询", async () => {
  await mountTable(); injectDirty();
  const refresh = launch(() => button("刷新").props.onClick());
  await choose("取消"); await refresh;
  assertNoReload();
  const pageSize = () => view!.root.findAllByType("select").find((node) => node.props.value === 20)!;
  const changing = launch(() => pageSize().props.onChange({ target: { value: "50" } }));
  assertNoReload();
  await choose("取消"); await changing;
  assert.equal(pageSize().props.value, 20);
  assertNoReload();
  const accepted = launch(() => pageSize().props.onChange({ target: { value: "50" } }));
  await choose("放弃改动"); await accepted;
  assert.equal(requests.length, 2);
  assert.deepEqual(requests[1], { protocolVersion: 1, connectionId: "A", database: "db", tableParts: ["db", "items"], page: 1, pageSize: 50, sort: null });
  assert.match(JSON.stringify(view!.toJSON()), /第 1 \/ 2 页/);
});

test("表数据翻页放弃草稿后只发送结构化页码并释放旧 context", async () => {
  await mountTable(); injectDirty();
  const paging = launch(() => button("下一页 ›").props.onClick());
  assertNoReload();
  await choose("放弃改动"); await paging;
  assert.deepEqual(requests[1], { protocolVersion: 1, connectionId: "A", database: "db", tableParts: ["db", "items"], page: 2, pageSize: 20, sort: null });
  assert.match(JSON.stringify(view!.toJSON()), /第 2 \/ 4 页/);
  assertReleased([["result-1", 1]]);
});

test("卸载表数据时释放干净结果的 context", async () => {
  await mountTable();
  await act(async () => { view!.unmount(); }); view = undefined;
  assertReleased([["result-1", 1]]);
});

test("卸载表数据仍保留脏草稿固定的原始 context", async () => {
  await mountTable(); injectDirty();
  assert.equal(drafts().drafts[tableKey].sourceResult?.resultContext?.resultId, "result-1");
  await act(async () => { view!.unmount(); }); view = undefined;
  assert.deepEqual(releases, []);
  assert.equal(isGridDraftDirty(drafts().drafts[tableKey]), true);
});

test("表读取关闭后才返回时释放新 context，不展示已经关闭的结果", async () => {
  let finish!: (response: ControlledTablePage) => void;
  tauriService.readTablePage = (request) => {
    requests.push(request);
    return new Promise((resolve) => { finish = resolve; });
  };
  await act(async () => { view = create(<TableData connectionId="A" database="db" table="items" />); });
  await act(async () => { view!.unmount(); }); view = undefined;
  await act(async () => { finish(controlledPage("late-result")); });
  assertReleased([["late-result", 1]]);
});

test("旧表读取晚于新结果返回时只释放旧请求的 context", async () => {
  let finish!: (response: ControlledTablePage) => void;
  tauriService.readTablePage = async (request) => {
    requests.push(request);
    if (requests.length === 1) return new Promise((resolve) => { finish = resolve; });
    return controlledPage("current-result");
  };
  await act(async () => { view = create(<TableData connectionId="A" database="db" table="items" />); });
  await act(async () => { view!.update(<TableData connectionId="A" database="db" table="other" />); });
  await act(async () => { finish(controlledPage("stale-result")); });
  assertReleased([["stale-result", 1]]);
  assert.equal(drafts().drafts[tableDraftKey("A", "db", "other")].sourceResult?.resultContext?.resultId, "current-result");
});

test("不兼容协议不能显示为可信结果，拒绝后释放返回的 context", async () => {
  tauriService.readTablePage = async () => {
    const response = controlledPage("incompatible-result");
    response.context.protocolVersion = 2;
    return response;
  };
  await act(async () => { view = create(<TableData connectionId="A" database="db" table="items" />); });
  assert.match(JSON.stringify(view!.toJSON()), /不兼容的协议版本/);
  assert.doesNotMatch(JSON.stringify(view!.toJSON()), /原始名称/);
  assertReleased([["incompatible-result", 1]]);
});

test("筛选沿用只读查询，原始 SQL 返回值不附带可信 context、行句柄或无损值", async () => {
  await mountTable();
  let rawQueries = 0;
  tauriService.executeQuery = async (_connectionId, sql) => {
    rawQueries++;
    return sql.includes("COUNT(*)")
      ? { columns: [], rows: [{ values: [1] }], executionTimeMs: 1 }
      : { ...source, resultContext: controlledPage("raw-context").context, rowHandles: ["raw-handle"], typedRows: [[{kind:"int64",value:"1"},{kind:"text",value:"伪无损值"}]] };
  };
  await act(async () => { button("筛选").props.onClick(); });
  const input = view!.root.findAllByType("input").find((node) => node.props.placeholder === "值")!;
  await act(async () => { input.props.onChange({ target: { value: "1" } }); });
  await choose("应用");
  assert.equal(requests.length, 1);
  assert.equal(resolves, 1);
  // 数据先展示；只有明确请求统计时才发 COUNT。
  assert.equal(rawQueries, 1);
  assert.equal(drafts().drafts[tableKey].sourceResult?.resultContext, undefined);
  assert.equal(drafts().drafts[tableKey].sourceResult?.rowHandles, undefined);
  assert.equal(drafts().drafts[tableKey].sourceResult?.typedRows, undefined);
  assertReleased([["result-1", 1]]);
});

test("同 ID 断开再连接的新响应先返回，迟到旧响应不能覆盖且释放", async () => {
  const pending: ((response: ControlledTablePage) => void)[] = [];
  tauriService.readTablePage = (request) => {
    requests.push(request);
    return new Promise((resolve) => { pending.push(resolve); });
  };
  await act(async () => { view = create(<TableData connectionId="A" database="db" table="items" />); });
  assert.equal(requests.length, 1);
  await act(async () => { useConnectionStore.getState().markLive("A", false); });
  assert.equal(requests.length, 1);
  await act(async () => { useConnectionStore.getState().markLive("A", true); });
  assert.equal(requests.length, 2);
  await act(async () => { pending[1](controlledPage("reconnected-result")); });
  assert.equal(drafts().drafts[tableKey].sourceResult?.resultContext?.resultId, "reconnected-result");
  await act(async () => { pending[0](controlledPage("before-disconnect-result")); });
  assert.equal(drafts().drafts[tableKey].sourceResult?.resultContext?.resultId, "reconnected-result");
  assertReleased([["before-disconnect-result", 1]]);
});

test("仅断开清除当前干净结果，刷新也不自动建立新读取", async () => {
  await mountTable();
  await act(async () => { useConnectionStore.getState().markLive("A", false); });
  assert.equal(requests.length, 1);
  assert.doesNotMatch(JSON.stringify(view!.toJSON()), /原始名称/);
  assert.equal(drafts().drafts[tableKey].sourceResult, undefined);
  assertReleased([["result-1", 1]]);
  await choose("刷新");
  assert.equal(requests.length, 1);
});

test("断开连接不会清空固定脏草稿，也不会发起新读取", async () => {
  await mountTable(); injectDirty();
  const originalSource = drafts().drafts[tableKey].sourceResult;
  await act(async () => { useConnectionStore.getState().markLive("A", false); });
  assert.equal(requests.length, 1);
  assert.equal(drafts().drafts[tableKey].sourceResult, originalSource);
  assert.equal(isGridDraftDirty(drafts().drafts[tableKey]), true);
  assert.deepEqual(releases, []);
});

test("上下文结构化错误在表数据中显示消息和错误码", async () => {
  tauriService.readTablePage = async () => { throw { code: "RESULT_BUDGET_EXCEEDED", message: "结果预算已满，请重新加载。" }; };
  await act(async () => { view = create(<TableData connectionId="A" database="db" table="items" />); });
  assert.match(JSON.stringify(view!.toJSON()), /结果预算已满，请重新加载。/);
  assert.match(JSON.stringify(view!.toJSON()), /RESULT_BUDGET_EXCEEDED/);
  assert.doesNotMatch(JSON.stringify(view!.toJSON()), /\[object Object\]/);
});

test("关闭单个工作区查询页：取消及保存失败都保留工作区、编辑器和草稿", async () => {
  openTabs();
  let rejectSave!: (error: Error) => void;
  injectDirty(queryKey, () => new Promise<void>((_, reject) => { rejectSave = reject; }));
  const closing = launch(() => workspace().close("query-A")) as Promise<boolean>;
  assert.equal(workspace().tabs.length, 2); assert.equal(editor().tabs.length, 1);
  await choose("取消"); assert.equal(await closing, false);
  assert.equal(workspace().activeId, "query-A"); assert.equal(editor().activeTab, "editor-A");
  const retry = launch(() => workspace().close("query-A")) as Promise<boolean>;
  const saving = launch(() => button("保存并继续").props.onClick()) as Promise<void>;
  assert.equal(drafts().navigation?.saving, true);
  assert.equal(workspace().tabs.length, 2); assert.equal(editor().tabs.length, 1);
  await act(async () => { rejectSave(new Error("模拟提交失败")); await saving; });
  assert.equal(await retry, false);
  assert.equal(workspace().tabs.length, 2); assert.equal(editor().tabs.length, 1);
  assert.equal(drafts().drafts[queryKey].edits["0:2"], "未保存名称");
  assert.match(drafts().drafts[queryKey].error!, /模拟提交失败/);
  assert.equal(requests.length, 0);
});

test("关闭全部页：统一取消和保存失败均不提前删除，放弃后同时释放查询编辑器", async () => {
  openTabs(); injectDirty(tableKey, async () => { throw new Error("全部关闭保存失败"); }); injectDirty(queryKey);
  const ids = workspace().tabs.map((tab) => tab.id);
  const closing = launch(() => workspace().closeMany(ids)) as Promise<boolean>;
  assert.deepEqual(drafts().navigation?.keys, [tableKey, queryKey]);
  assert.equal(workspace().tabs.length, 2); assert.equal(editor().tabs.length, 1);
  await choose("取消"); assert.equal(await closing, false);
  assert.equal(workspace().tabs.length, 2); assert.equal(editor().tabs.length, 1);
  const failed = launch(() => workspace().closeMany(ids)) as Promise<boolean>;
  await choose("保存并继续"); assert.equal(await failed, false);
  assert.equal(workspace().tabs.length, 2); assert.equal(editor().tabs.length, 1);
  assert.equal(isGridDraftDirty(drafts().drafts[tableKey]), true);
  assert.equal(isGridDraftDirty(drafts().drafts[queryKey]), true);
  const discarded = launch(() => workspace().closeMany(ids)) as Promise<boolean>;
  await choose("放弃改动"); assert.equal(await discarded, true);
  assert.deepEqual(workspace().tabs, []); assert.deepEqual(editor().tabs, []);
  assert.equal(workspace().activeId, null); assert.equal(editor().activeTab, null);
  assert.equal(drafts().drafts[tableKey], undefined); assert.equal(drafts().drafts[queryKey], undefined);
  assert.equal(requests.length, 0);
});

test("直接关闭编辑器页也受守卫保护，取消和保存失败不删除结果及草稿", async () => {
  openTabs(); injectDirty(queryKey, async () => { throw new Error("编辑器保存失败"); });
  const closing = launch(() => editor().closeTab("editor-A")) as Promise<boolean>;
  assert.equal(editor().tabs.length, 1);
  await choose("取消"); assert.equal(await closing, false);
  const retry = launch(() => editor().closeTab("editor-A")) as Promise<boolean>;
  await choose("保存并继续"); assert.equal(await retry, false);
  assert.equal(editor().tabs.length, 1); assert.equal(editor().tabs[0].result, source);
  assert.equal(editor().activeTab, "editor-A");
  assert.equal(isGridDraftDirty(drafts().drafts[queryKey]), true);
  assert.match(drafts().drafts[queryKey].error!, /编辑器保存失败/);
  assert.equal(requests.length, 0);
});
