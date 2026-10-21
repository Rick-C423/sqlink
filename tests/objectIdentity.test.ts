import assert from "node:assert/strict";
import { test } from "node:test";
import { databaseNodeId, relationNode, relationTabId } from "../src/lib/objectIdentity";
import { tableDraftKey } from "../src/stores/gridDraftStore";
import { useWorkspaceStore } from "../src/stores/workspaceStore";

const names = ["普通名称", "a::b", "a.b", 'a"b', "a'b"];

test("对象 JSON 键完整隔离连接、数据库、schema、表名中的分隔符和引号", () => {
  const databases = new Set<string>();
  const relations = new Set<string>();
  const nodes = new Set<string>();
  for (const connectionId of names) for (const database of names) {
    const parent = databaseNodeId(connectionId, database);
    assert.equal(databases.has(parent), false); databases.add(parent);
    for (const schema of names) for (const table of names) for (const kind of ["table", "view"] as const) {
      const key = relationTabId(kind, connectionId, database, table, schema);
      assert.equal(relations.has(key), false); relations.add(key);
      const node = relationNode(parent, kind, connectionId, database, { schema, name: table });
      assert.equal(nodes.has(node.id), false); nodes.add(node.id);
    }
  }
  assert.equal(databases.size, names.length ** 2);
  assert.equal(relations.size, names.length ** 4 * 2);
  assert.equal(nodes.size, relations.size);
  assert.notEqual(databaseNodeId("A::db::B", "C"), databaseNodeId("A", "B::db::C"));
  assert.notEqual(relationTabId("table", "A", "db", "a.b", "c"), relationTabId("table", "A", "db", "b", "c.a"));
});

test("工作区同名跨 schema 表和视图保持独立，仅精确同一身份复用页签", () => {
  const previous = useWorkspaceStore.getState();
  try {
    useWorkspaceStore.setState({ tabs: [], activeId: null });
    const store = useWorkspaceStore.getState();
    for (const schema of ["one::schema", 'two."schema']) {
      store.openTable("A::connection", "db.with::separator", "a.b", "data", schema);
      store.openView("A::connection", "db.with::separator", "a.b", schema);
    }
    assert.equal(useWorkspaceStore.getState().tabs.length, 4);
    assert.equal(new Set(useWorkspaceStore.getState().tabs.map((tab) => tab.id)).size, 4);
    store.openTable("A::connection", "db.with::separator", "a.b", "structure", "one::schema");
    store.openView("A::connection", "db.with::separator", "a.b", 'two."schema');
    const tabs = useWorkspaceStore.getState().tabs;
    assert.equal(tabs.length, 4);
    assert.deepEqual(tabs.map((tab) => [tab.kind, tab.schema, tab.table]), [
      ["table", "one::schema", "a.b"], ["view", "one::schema", "a.b"],
      ["table", 'two."schema', "a.b"], ["view", 'two."schema', "a.b"],
    ]);
    assert.equal(tabs.find((tab) => tab.kind === "table" && tab.schema === "one::schema")!.tableSubTab, "structure");
    assert.equal(useWorkspaceStore.getState().activeId, relationTabId("view", "A::connection", "db.with::separator", "a.b", 'two."schema'));
  } finally { useWorkspaceStore.setState(previous); }
});

test("网格草稿键兼容三参数调用，并对 schema 和任意名称分段隔离", () => {
  assert.equal(tableDraftKey("A", "db", "table"), JSON.stringify(["table", "A", "db", "table"]));
  assert.equal(tableDraftKey("A", "db", "table", undefined), tableDraftKey("A", "db", "table"));
  assert.notEqual(tableDraftKey("A", "db", "table", ""), tableDraftKey("A", "db", "table"));
  assert.notEqual(tableDraftKey("A", "db", "table", "one"), tableDraftKey("A", "db", "table", "two"));
  const keys = new Set<string>();
  for (const connectionId of names) for (const database of names) for (const schema of names) for (const table of names) {
    const key = tableDraftKey(connectionId, database, table, schema);
    assert.equal(keys.has(key), false); keys.add(key);
  }
  assert.equal(keys.size, names.length ** 4);
});

test("数据库、建表和例程工作区键不混淆包含分隔符的不同来源", () => {
  const previous = useWorkspaceStore.getState();
  try {
    useWorkspaceStore.setState({ tabs: [], activeId: null });
    const store = useWorkspaceStore.getState();
    for (const [connection, database] of [["A::B", "C"], ["A", "B::C"]]) {
      store.openDatabase(connection, database);
      store.openCreateTable(connection, database);
      store.openRoutine(connection, database, "function", "literal::name");
    }
    assert.equal(useWorkspaceStore.getState().tabs.length, 6);
    assert.equal(new Set(useWorkspaceStore.getState().tabs.map((tab) => tab.id)).size, 6);
  } finally { useWorkspaceStore.setState(previous); }
});

test("relationNode 的显示名即使相同，source 仍原样保留各段且不反解名称", () => {
  const connectionId = 'connection::"x';
  const database = "database::a.b";
  const parent = databaseNodeId(connectionId, database);
  const first = relationNode(parent, "table", connectionId, database, { schema: "a", name: "b.c", rowCount: 9, comment: "注释" });
  const second = relationNode(parent, "table", connectionId, database, { schema: "a.b", name: "c" });
  assert.equal(first.name, second.name); assert.notEqual(first.id, second.id);
  assert.deepEqual(first.source, { connectionId, database, schema: "a", table: "b.c" });
  assert.deepEqual(second.source, { connectionId, database, schema: "a.b", table: "c" });
  assert.deepEqual(first.metadata, { rowCount: 9, comment: "注释" });
  const unusual = relationNode(parent, "view", connectionId, database, { schema: 's::"x.y', name: "t::'z.w" });
  assert.deepEqual(unusual.source, { connectionId, database, schema: 's::"x.y', table: "t::'z.w" });
});
