import assert from "node:assert/strict";
import { afterEach, beforeEach, test } from "node:test";
import { act, create, type ReactTestInstance, type ReactTestRenderer } from "react-test-renderer";
import { DatabaseOverview } from "../src/components/DatabaseOverview";
import { ViewView } from "../src/components/ViewView";
import { CreateTableView } from "../src/components/CreateTableView";
import { databaseNodeId } from "../src/lib/objectIdentity";
import { tauriService } from "../src/services/tauriService";
import { useConnectionStore } from "../src/stores/connectionStore";
import { useWorkspaceStore } from "../src/stores/workspaceStore";
import { useGridDraftStore } from "../src/stores/gridDraftStore";
import { useI18nStore } from "../src/i18n";
import type { ResolvedTable, TableInfo } from "../src/types/database";
import type { ControlledTableReadRequest } from "../src/types/resultContext";

const original = { tables: tauriService.getTables, definition: tauriService.getViewDefinition, read: tauriService.readTablePage,
  execute: tauriService.executeBatch, resolve: tauriService.resolveTable };
const originalConnections = useConnectionStore.getState();
const originalWorkspace = useWorkspaceStore.getState();
const originalLocale = useI18nStore.getState().locale;
let view: ReactTestRenderer | undefined;
const tables: TableInfo[] = [{ schema: "one", name: "a.b" }, { schema: "two", name: "a.b" }];
function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((done) => { resolve = done; });
  return { promise, resolve };
}
function text(node?: ReactTestInstance): string {
  return node ? node.children.map((child) => typeof child === "string" ? child : text(child)).join("") : JSON.stringify(view!.toJSON());
}
function button(label: string) {
  const found = view!.root.findAllByType("button").find((node) => text(node) === label);
  assert.ok(found, `缺少按钮：${label}`); return found;
}
async function click(label: string) { await act(async () => { await button(label).props.onClick(); }); }

beforeEach(() => {
  useI18nStore.setState({ locale: "zh-CN" });
  useConnectionStore.setState({ saved: [{ id: "A", name: "连接A", driverType: "postgres", host: "localhost", port: 5432, user: "test", password: "" }],
    liveIds: new Set(["A", "B"]), connectionEpochs: { A: 1, B: 1 }, expandedNodes: new Set(), treeData: [] });
  useWorkspaceStore.setState({ tabs: [], activeId: null });
  useGridDraftStore.setState({ drafts: {}, navigation: null });
  tauriService.getTables = async () => tables;
  tauriService.readTablePage = async () => { throw new Error("只验证身份传递"); };
  tauriService.getViewDefinition = async (_connectionId, _database, name, schema) => `CREATE VIEW ${schema}.${name}`;
});
afterEach(() => {
  act(() => view?.unmount()); view = undefined;
  tauriService.getTables = original.tables; tauriService.getViewDefinition = original.definition; tauriService.readTablePage = original.read;
  tauriService.executeBatch = original.execute; tauriService.resolveTable = original.resolve;
  useConnectionStore.setState(originalConnections); useWorkspaceStore.setState(originalWorkspace); useI18nStore.setState({ locale: originalLocale });
  useGridDraftStore.setState({ drafts: {}, navigation: null });
});

test("数据库概览按 schema 显示同名表并将完整身份交给工作区", async () => {
  await act(async () => { view = create(<DatabaseOverview connectionId="A" database="db" />); });
  const rows = view!.root.findAllByType("tr").filter((node) => node.props.onDoubleClick);
  assert.equal(rows.length, 2); assert.match(text(rows[0]), /one.a.b/); assert.match(text(rows[1]), /two.a.b/);
  act(() => { rows[0].props.onDoubleClick(); rows[1].props.onDoubleClick(); });
  assert.deepEqual(useWorkspaceStore.getState().tabs.map((tab) => [tab.connectionId, tab.database, tab.schema, tab.table]),
    [["A", "db", "one", "a.b"], ["A", "db", "two", "a.b"]]);
});

test("数据库概览的旧连接和旧 epoch 清单迟到不能覆盖当前列表", async () => {
  const old = deferred<TableInfo[]>();
  const later = deferred<TableInfo[]>();
  tauriService.getTables = async (connectionId) => connectionId === "A" ? old.promise : later.promise;
  await act(async () => { view = create(<DatabaseOverview connectionId="A" database="db" />); });
  await act(async () => { view!.update(<DatabaseOverview connectionId="B" database="db2" />); });
  await act(async () => { later.resolve([{ schema: "two", name: "当前表" }]); await later.promise; });
  await act(async () => { old.resolve([{ schema: "one", name: "迟到表" }]); await old.promise; });
  assert.match(text(), /当前表/); assert.doesNotMatch(text(), /迟到表/);
  const stale = deferred<TableInfo[]>(); tauriService.getTables = async () => stale.promise;
  let refresh!: Promise<void>; act(() => { refresh = button("刷新").props.onClick(); });
  await act(async () => { useConnectionStore.setState({ connectionEpochs: { A: 1, B: 2 }, liveIds: new Set(["A"]) }); });
  await act(async () => { stale.resolve([{ name: "断开前旧表" }]); await refresh; });
  assert.doesNotMatch(text(), /断开前旧表|当前表/);
});

test("视图数据和定义都传 schema，旧定义与断开前定义迟到不能恢复", async () => {
  const reads: ControlledTableReadRequest[] = [];
  const definitions: unknown[][] = [];
  const old = deferred<string>();
  tauriService.readTablePage = async (request) => { reads.push(request); throw new Error("仅验证来源"); };
  tauriService.getViewDefinition = async (...args) => { definitions.push(args); return args[3] === "one" ? old.promise : "two 的定义"; };
  await act(async () => { view = create(<ViewView connectionId="A" database="db" schema="one" view="v.x" />); });
  assert.deepEqual(reads[0].tableParts, ["one", "v.x"]);
  await click("定义");
  await act(async () => { view!.update(<ViewView connectionId="A" database="db" schema="two" view="v.x" />); });
  await act(async () => { old.resolve("迟到的 one 定义"); await old.promise; });
  assert.deepEqual(definitions, [["A", "db", "v.x", "one"], ["A", "db", "v.x", "two"]]);
  assert.match(text(), /two 的定义/); assert.doesNotMatch(text(), /迟到的 one/);
  const stale = deferred<string>(); tauriService.getViewDefinition = async () => stale.promise;
  let refresh!: Promise<void>; act(() => { refresh = button("刷新").props.onClick(); });
  await act(async () => { useConnectionStore.setState({ connectionEpochs: { A: 2, B: 1 }, liveIds: new Set(["B"]) }); });
  await act(async () => { stale.resolve("断开前定义"); await refresh; });
  assert.doesNotMatch(text(), /断开前定义|two 的定义/);
});

test("新建表的畸形列名输入显示错误，修正后用后端 schema 打开并生成无碰撞树节点", async () => {
  const dbId = databaseNodeId("A", "db::with.separator");
  useConnectionStore.setState({ treeData: [{ id: "A", name: "A", type: "connection", children: [{ id: dbId, name: "db", type: "database" }] }] });
  const executed: string[][] = [];
  tauriService.executeBatch = async (_connectionId, _database, statements) => { executed.push(statements); return statements.length; };
  tauriService.resolveTable = async (connectionId, database, parts) => {
    assert.equal(connectionId, "A"); assert.equal(database, "db::with.separator"); assert.deepEqual(parts, ["a.b"]);
    return { database, schema: "two", table: "a.b", columns: [] };
  };
  await act(async () => { view = create(<CreateTableView connectionId="A" database="db::with.separator" tabId="create" />); });
  act(() => { view!.root.findAllByType("input").find((node) => node.props.placeholder === "表名")!.props.onChange({ target: { value: "a.b" } }); });
  await click("索引 (0)"); await click("新增索引");
  const columns = () => view!.root.findAllByType("input").find((node) => node.props.placeholder === "col1, col2")!;
  act(() => { columns().props.onChange({ target: { value: '"坏列名' } }); });
  assert.match(text(), /列名引用未闭合/); assert.equal(button("创建表").props.disabled, true); assert.equal(executed.length, 0);
  act(() => { columns().props.onChange({ target: { value: '"a,b"' } }); });
  assert.doesNotMatch(text(), /列名引用未闭合/);
  await click("创建表");
  assert.equal(executed.length, 1); assert.match(executed[0].join("\n"), /"a,b"/);
  const nodes = useConnectionStore.getState().treeData[0].children![0].children!;
  assert.equal(nodes.length, 2); assert.notEqual(nodes[0].id, nodes[1].id);
  assert.deepEqual(nodes.map((node) => node.source), [{ connectionId: "A", database: "db::with.separator", schema: "one", table: "a.b" },
    { connectionId: "A", database: "db::with.separator", schema: "two", table: "a.b" }]);
  const opened = useWorkspaceStore.getState().tabs.find((tab) => tab.kind === "table")!;
  assert.equal(opened.schema, "two"); assert.equal(opened.table, "a.b");
});

for (const stage of ["ddl", "tables", "resolve"] as const) {
  for (const change of ["epoch", "source", "unmount"] as const) {
    test(`新建表 ${stage} 阶段遇到 ${change} 后，旧任务完成不继续读取或修改树和页签`, async () => {
      const ddl = deferred<number>();
      const list = deferred<TableInfo[]>();
      const resolved = deferred<ResolvedTable>();
      const calls: string[] = [];
      const effects: string[] = [];
      const created = { database: "db", schema: "one", table: "created", columns: [] };
      useConnectionStore.setState({ updateNodeChildren: () => { effects.push("update-tree"); }, expandNode: () => { effects.push("expand-tree"); } });
      useWorkspaceStore.setState({ openTable: () => { effects.push("open-table"); }, close: async () => { effects.push("close-tab"); return true; } });
      tauriService.executeBatch = async (connectionId, database) => {
        assert.equal(connectionId, "A"); assert.equal(database, "db"); calls.push("ddl");
        return stage === "ddl" ? ddl.promise : 1;
      };
      tauriService.getTables = async (connectionId, database) => {
        assert.equal(connectionId, "A"); assert.equal(database, "db"); calls.push("tables");
        return stage === "tables" ? list.promise : tables;
      };
      tauriService.resolveTable = async (connectionId, database, parts) => {
        assert.equal(connectionId, "A"); assert.equal(database, "db"); assert.deepEqual(parts, ["created"]); calls.push("resolve");
        return stage === "resolve" ? resolved.promise : created;
      };
      await act(async () => { view = create(<CreateTableView connectionId="A" database="db" tabId="create-A" />); });
      act(() => { view!.root.findAllByType("input").find((node) => node.props.placeholder === "表名")!.props.onChange({ target: { value: "created" } }); });
      let pending!: Promise<void>;
      await act(async () => { pending = button("创建表").props.onClick(); });
      const before = [...calls];
      if (change === "epoch") {
        await act(async () => { useConnectionStore.setState({ connectionEpochs: { A: 3, B: 1 } }); });
      } else if (change === "source") {
        await act(async () => { view!.update(<CreateTableView connectionId="B" database="other" tabId="create-B" />); });
      } else {
        act(() => { view!.unmount(); view = undefined; });
      }
      await act(async () => {
        if (stage === "ddl") ddl.resolve(1);
        else if (stage === "tables") list.resolve(tables);
        else resolved.resolve(created);
        await pending;
      });
      assert.deepEqual(calls, before);
      assert.deepEqual(effects, []);
      if (view) assert.doesNotMatch(text(), /创建失败|结构刷新失败/);
    });
  }
}

test("建表 DDL 成功后的结构刷新错误明确说明语句已执行，不误报建表失败", async () => {
  tauriService.executeBatch = async () => 1;
  tauriService.getTables = async () => { throw new Error("结构目录暂不可读"); };
  tauriService.resolveTable = async () => { assert.fail("清单读取失败后不能继续解析来源"); };
  await act(async () => { view = create(<CreateTableView connectionId="A" database="db" tabId="create" />); });
  act(() => { view!.root.findAllByType("input").find((node) => node.props.placeholder === "表名")!.props.onChange({ target: { value: "created" } }); });
  await click("创建表");
  assert.match(text(), /建表语句已执行，但结构刷新失败.*结构目录暂不可读/);
  assert.doesNotMatch(text(), /创建失败/);
});
