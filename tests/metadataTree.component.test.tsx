import assert from "node:assert/strict";
import { afterEach, beforeEach, test } from "node:test";
import { act, create, type ReactTestInstance, type ReactTestRenderer } from "react-test-renderer";
import { ConnectionTree } from "../src/components/connection/ConnectionTree";
import { useI18nStore } from "../src/i18n";
import { metadataCache } from "../src/lib/metadataCache";
import { tauriService } from "../src/services/tauriService";
import { useConnectionStore } from "../src/stores/connectionStore";
import { useEditorStore } from "../src/stores/editorStore";
import { useWorkspaceStore } from "../src/stores/workspaceStore";
import type { ExecuteDdlRequest, MetadataSnapshot, TableInfo, TreeNode } from "../src/types/database";

const connectionId = '连接::A.一"二';
const database = '库::db.一"二';
const schema = '模式::s.一"二';
const otherSchema = '模式::另.一"二';
const table = '表::t.一"二';
const viewName = '视图::v.一"二';
const otherConnection = "另一个连接";
const originalServices = {
  getDatabaseInfos: tauriService.getDatabaseInfos, getTables: tauriService.getTables,
  getViews: tauriService.getViews, getTriggers: tauriService.getTriggers,
  getProcedures: tauriService.getProcedures, getFunctions: tauriService.getFunctions,
  getTableMetadata: tauriService.getTableMetadata, executeDdl: tauriService.executeDdl,
  executeQuery: tauriService.executeQuery, executeBatch: tauriService.executeBatch,
};
const originalConnections = useConnectionStore.getState();
const originalWorkspace = useWorkspaceStore.getState();
const originalEditor = useEditorStore.getState();
const originalLocale = useI18nStore.getState().locale;
const originalWindow = Object.getOwnPropertyDescriptor(globalThis, "window");
const originalStorage = Object.getOwnPropertyDescriptor(globalThis, "localStorage");
let renderer: ReactTestRenderer | undefined;
let metadataRequests: [string, string, string[]][];
let tableRequests: [string, string | undefined][];
let submissions: ExecuteDdlRequest[];
let promptResult: string | null;

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((done) => { resolve = done; });
  return { promise, resolve };
}
function text(node: ReactTestInstance): string {
  return node.children.map((child) => typeof child === "string" ? child : text(child)).join("");
}
function rows() { return renderer!.root.findAllByType("div").filter((node) => typeof node.props.onContextMenu === "function"); }
function row(name: string) {
  const found = rows().find((node) => node.children.some((child) => typeof child !== "string" && child.type === "span" && text(child) === name));
  assert.ok(found, `缺少树节点：${name}`); return found;
}
function nodeFor(name: string): TreeNode {
  function walk(nodes: TreeNode[]): TreeNode | undefined {
    for (const node of nodes) { if (node.name === name) return node; const nested = node.children && walk(node.children); if (nested) return nested; }
  }
  const found = walk(useConnectionStore.getState().treeData);
  assert.ok(found, `store 缺少节点：${name}`); return found;
}
async function clickRow(name: string) { await act(async () => { row(name).props.onClick(); }); }
async function expand(name: string) {
  const id = nodeFor(name).id;
  const arrow = () => row(name).children.find((child): child is ReactTestInstance => typeof child !== "string" && typeof child.props.onClick === "function")!;
  if (useConnectionStore.getState().expandedNodes.has(id)) act(() => { arrow().props.onClick({ stopPropagation() {} }); });
  await act(async () => { arrow().props.onClick({ stopPropagation() {} }); });
}
async function openMenu(name: string) {
  await act(async () => { row(name).props.onContextMenu({ preventDefault() {}, clientX: 10, clientY: 20 }); });
}
async function choose(label: string) {
  const button = renderer!.root.findAllByType("button").find((node) => text(node) === label);
  assert.ok(button, `缺少右键操作：${label}`);
  await act(async () => { button.props.onClick(); });
}
async function menu(name: string, label: string) { await openMenu(name); await choose(label); }
function snapshot(column = "当前列"): MetadataSnapshot {
  return { connectionId, connectionGeneration: "服务端代次::原连接", revision: "结构修订::原对象",
    target: { database, schema, table, columns: [{ name: column, dataType: "text", nullable: true, isPrimaryKey: false }] },
    indexes: [], foreignKeys: [] };
}
async function mount(loadDatabase = true) {
  await act(async () => { renderer = create(<ConnectionTree onNewConnection={() => {}} onEditConnection={() => {}} />); });
  await expand("原连接");
  if (loadDatabase) await expand(database);
}
function assertSource(node: TreeNode, expectedSchema = schema, name = table) {
  assert.deepEqual(node.source, { connectionId, database, schema: expectedSchema, table: name });
}

beforeEach(() => {
  metadataCache.clear(); metadataRequests = []; tableRequests = []; submissions = []; promptResult = null;
  useI18nStore.setState({ locale: "zh-CN" });
  useConnectionStore.setState({
    saved: [{ id: connectionId, name: "原连接", driverType: "postgres", host: "localhost", port: 5432, user: "test", password: "", database },
      { id: otherConnection, name: "另连接", driverType: "mysql", host: "localhost", port: 3306, user: "test", password: "", database: "另库" }],
    treeData: [{ id: connectionId, name: "原连接", type: "connection" }, { id: otherConnection, name: "另连接", type: "connection" }],
    liveIds: new Set([connectionId, otherConnection]), connectionEpochs: { [connectionId]: 1, [otherConnection]: 1 },
    activeConnection: otherConnection, selectedDatabase: "另库", expandedNodes: new Set(), loadingNodes: new Set(),
    showSystemDatabases: false, credentialHealth: null, refreshCredentialHealth: async () => {},
  });
  useWorkspaceStore.setState({ tabs: [], activeId: null });
  useEditorStore.setState({ tabs: [], activeTab: null });
  // renderer 不创建原生 DOM；真实 ContextMenu 仍注册和移除窗口监听。
  Object.defineProperty(globalThis, "window", { configurable: true, value: {
    addEventListener() {}, removeEventListener() {}, innerWidth: 1200, innerHeight: 800,
    confirm: () => true, prompt: () => promptResult,
  } });
  const storage = new Map<string, string>();
  Object.defineProperty(globalThis, "localStorage", { configurable: true, value: {
    getItem: (key: string) => storage.get(key) ?? null,
    setItem: (key: string, value: string) => { storage.set(key, value); },
  } });
  tauriService.getDatabaseInfos = async (id) => { assert.equal(id, connectionId); return [{ name: database, accessible: true }]; };
  tauriService.getTables = async (id, db) => { tableRequests.push([id, db]); return [{ schema, name: table }, { schema: otherSchema, name: table }]; };
  tauriService.getViews = async () => [{ schema, name: viewName }];
  tauriService.getTriggers = async () => [];
  tauriService.getProcedures = async () => [];
  tauriService.getFunctions = async () => [];
  tauriService.getTableMetadata = async (id, db, parts) => { metadataRequests.push([id, db, parts]); return snapshot(); };
  tauriService.executeDdl = async (request) => { submissions.push(request); metadataCache.invalidateConnection(request.connectionId); return request.statements.length; };
  tauriService.executeQuery = async () => { assert.fail("表操作必须经过带 generation/revision 的 executeDdl"); };
  tauriService.executeBatch = async () => { assert.fail("表操作不能绕过结构元数据授权"); };
});
afterEach(() => {
  act(() => renderer?.unmount()); renderer = undefined;
  Object.assign(tauriService, originalServices);
  metadataCache.clear();
  useConnectionStore.setState(originalConnections); useWorkspaceStore.setState(originalWorkspace); useEditorStore.setState(originalEditor);
  useI18nStore.setState({ locale: originalLocale });
  if (originalWindow) Object.defineProperty(globalThis, "window", originalWindow); else Reflect.deleteProperty(globalThis, "window");
  if (originalStorage) Object.defineProperty(globalThis, "localStorage", originalStorage); else Reflect.deleteProperty(globalThis, "localStorage");
});

test("连接树保留名称中的双冒号、点和引号；同名跨 schema 节点点击及查询模板使用完整来源", async () => {
  await mount();
  assert.deepEqual(nodeFor(database).source, { connectionId, database });
  const first = nodeFor(`${schema}.${table}`);
  const second = nodeFor(`${otherSchema}.${table}`);
  assertSource(first); assertSource(second, otherSchema); assert.notEqual(first.id, second.id);
  assert.deepEqual(tableRequests, [[connectionId, database]]);
  await clickRow(first.name); await clickRow(second.name);
  assert.deepEqual(useWorkspaceStore.getState().tabs.filter((tab) => tab.kind === "table").map((tab) => [tab.connectionId, tab.database, tab.schema, tab.table]),
    [[connectionId, database, schema, table], [connectionId, database, otherSchema, table]]);
  await expand("视图 (1)");
  const view = nodeFor(`${schema}.${viewName}`); assertSource(view, schema, viewName);
  await clickRow(view.name);
  const opened = useWorkspaceStore.getState().tabs.find((tab) => tab.kind === "view")!;
  assert.deepEqual([opened.connectionId, opened.database, opened.schema, opened.table], [connectionId, database, schema, viewName]);
  for (const [name, expectedName] of [[first.name, table], [view.name, viewName]]) {
    act(() => useConnectionStore.getState().setActiveConnection(otherConnection));
    await menu(name, "新建查询（SELECT *）");
    const query = useWorkspaceStore.getState().tabs.filter((tab) => tab.kind === "query").at(-1)!;
    assert.equal(query.connectionId, connectionId);
    assert.equal(useConnectionStore.getState().selectedDatabase, database);
    assert.equal(useEditorStore.getState().tabs.find((tab) => tab.id === query.editorTabId)!.content,
      `SELECT * FROM "模式::s.一""二"."${expectedName.replace(/"/g, '""')}" LIMIT 100;`);
  }
  await expand(first.name);
  assert.deepEqual(metadataRequests, [[connectionId, database, [schema, table]]]);
});

test("右键删除等待真实元数据，用 canonical 目标及其 generation/revision，切换全局连接不改原节点操作", async () => {
  await mount();
  const response = deferred<MetadataSnapshot>();
  const events: string[] = [];
  tauriService.getTableMetadata = async (id, db, parts) => { metadataRequests.push([id, db, parts]); events.push("metadata"); return response.promise; };
  tauriService.executeDdl = async (request) => { events.push("ddl"); submissions.push(request); return 1; };
  await openMenu(`${schema}.${table}`);
  act(() => { useConnectionStore.getState().setActiveConnection(otherConnection); useConnectionStore.getState().setSelectedDatabase("切换后的库"); });
  await choose("删除表");
  assert.deepEqual(events, ["metadata"]); assert.deepEqual(submissions, []);
  assert.deepEqual(metadataRequests, [[connectionId, database, [schema, table]]]);
  const canonical = { ...snapshot(), target: { ...snapshot().target, database: '实际库::"', schema: '实际模式.::"', table: '实际表.::"' } };
  await act(async () => { response.resolve(canonical); await response.promise; });
  assert.deepEqual(events, ["metadata", "ddl"]);
  assert.deepEqual(submissions, [{ connectionId, database: '实际库::"', connectionGeneration: "服务端代次::原连接", revision: "结构修订::原对象",
    tableParts: ['实际模式.::"', '实际表.::"'], statements: ['DROP TABLE "实际模式.::"""."实际表.::""";'] }]);
  assert.deepEqual(tableRequests.at(-1), [connectionId, database]);
  assert.equal(useConnectionStore.getState().activeConnection, otherConnection);
});

test("右键重命名从后端 canonical 目标生成 SQL，名称中的点与引号保持单段", async () => {
  await mount();
  const canonical = { ...snapshot(), target: { ...snapshot().target, database: "canonical-db", schema: '真实::模式."', table: '真实::表."' } };
  tauriService.getTableMetadata = async (id, db, parts) => { metadataRequests.push([id, db, parts]); return canonical; };
  promptResult = '新::名字.一"二';
  await menu(`${schema}.${table}`, "重命名表");
  assert.deepEqual(metadataRequests, [[connectionId, database, [schema, table]]]);
  assert.deepEqual(submissions, [{ connectionId, database: "canonical-db", connectionGeneration: "服务端代次::原连接", revision: "结构修订::原对象",
    tableParts: ['真实::模式."', '真实::表."'], statements: ['ALTER TABLE "真实::模式."""."真实::表.""" RENAME TO "新::名字.一""二";'] }]);
  assert.deepEqual(tableRequests.at(-1), [connectionId, database]);
});

test("同 ID 断开重连后旧表目录迟到不能注入新树或清除新请求的 loading", async () => {
  const old = deferred<TableInfo[]>(); const fresh = deferred<TableInfo[]>();
  let count = 0;
  tauriService.getTables = async (id, db) => { tableRequests.push([id, db]); return ++count === 1 ? old.promise : fresh.promise; };
  await mount();
  const dbId = nodeFor(database).id;
  assert.equal(useConnectionStore.getState().loadingNodes.has(dbId), true);
  act(() => { useConnectionStore.getState().markLive(connectionId, false); useConnectionStore.getState().markLive(connectionId, true); });
  await menu("原连接", "刷新");
  await menu(database, "刷新");
  assert.equal(count, 2);
  await act(async () => { old.resolve([{ schema, name: "旧代次迟到表" }]); await old.promise; });
  assert.equal(useConnectionStore.getState().loadingNodes.has(dbId), true);
  assert.doesNotMatch(JSON.stringify(useConnectionStore.getState().treeData), /旧代次迟到表/);
  await act(async () => { fresh.resolve([{ schema, name: "新代次当前表" }]); await fresh.promise; });
  assert.match(JSON.stringify(useConnectionStore.getState().treeData), /新代次当前表/);
  assert.doesNotMatch(JSON.stringify(useConnectionStore.getState().treeData), /旧代次迟到表/);
  assert.equal(useConnectionStore.getState().loadingNodes.has(dbId), false);
});

test("同 ID 重连后旧结构响应不能覆盖新树的列或重新进入当前结构缓存", async () => {
  await mount();
  const old = deferred<MetadataSnapshot>(); let count = 0;
  tauriService.getTableMetadata = async (id, db, parts) => {
    metadataRequests.push([id, db, parts]);
    return ++count === 1 ? old.promise : { ...snapshot("重连后列"), connectionGeneration: "重连后服务端代次" };
  };
  const name = `${schema}.${table}`;
  await expand(name);
  assert.equal(count, 1);
  act(() => { useConnectionStore.getState().markLive(connectionId, false); useConnectionStore.getState().markLive(connectionId, true); });
  await menu("原连接", "刷新"); await menu(database, "刷新"); await expand(name);
  assert.equal(count, 2);
  await act(async () => { old.resolve({ ...snapshot("重连前旧列"), connectionGeneration: "重连前服务端代次" }); await old.promise; });
  const tree = JSON.stringify(useConnectionStore.getState().treeData);
  assert.match(tree, /重连后列/); assert.doesNotMatch(tree, /重连前旧列/);
  const cached = await metadataCache.get({ connectionId, database, schema, table, category: "structure",
    connectionEpoch: useConnectionStore.getState().connectionEpochs[connectionId] }, async () => { assert.fail("新代次已成功加载的结构应能复用"); });
  assert.equal(cached.connectionGeneration, "重连后服务端代次");
});

test("显式刷新重取表目录并淘汰结构缓存，旧结构响应不能恢复刷新前子树", async () => {
  await mount();
  const name = `${schema}.${table}`;
  await expand(name);
  assert.equal(metadataRequests.length, 1);
  await menu(database, "刷新");
  await expand(name);
  assert.equal(tableRequests.length, 2);
  assert.equal(metadataRequests.length, 2);
  const stale = deferred<MetadataSnapshot>();
  tauriService.getTableMetadata = async (id, db, parts) => { metadataRequests.push([id, db, parts]); return stale.promise; };
  await menu(database, "刷新"); await expand(name);
  assert.equal(metadataRequests.length, 3);
  await menu(database, "刷新");
  tauriService.getTableMetadata = async (id, db, parts) => { metadataRequests.push([id, db, parts]); return snapshot("刷新后真实列"); };
  await expand(name);
  assert.equal(metadataRequests.length, 4);
  await act(async () => { stale.resolve(snapshot("刷新前迟到列")); await stale.promise; });
  const tree = JSON.stringify(useConnectionStore.getState().treeData);
  assert.match(tree, /刷新后真实列/); assert.doesNotMatch(tree, /刷新前迟到列/);
});
