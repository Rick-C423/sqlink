import assert from "node:assert/strict";
import { afterEach, beforeEach, test } from "node:test";
import { act, create, type ReactTestInstance, type ReactTestRenderer } from "react-test-renderer";
import { TableData, TableStructure } from "../src/components/TableView";
import { metadataCache } from "../src/lib/metadataCache";
import { tauriService } from "../src/services/tauriService";
import { useConnectionStore } from "../src/stores/connectionStore";
import { useGridDraftStore } from "../src/stores/gridDraftStore";
import { useI18nStore } from "../src/i18n";
import type { ExecuteDdlRequest, MetadataSnapshot } from "../src/types/database";

const original = { get: tauriService.getTableMetadata, execute: tauriService.executeDdl, read: tauriService.readTablePage };
const originalConnections = useConnectionStore.getState();
const originalLocale = useI18nStore.getState().locale;
let view: ReactTestRenderer | undefined;
let requests: { connectionId: string; database: string; parts: string[] }[];
let submissions: ExecuteDdlRequest[];

function snapshot(schema = "one", column = `列_${schema}`): MetadataSnapshot {
  return { connectionId: "A", connectionGeneration: "generation-A", revision: `revision-${schema}`,
    target: { database: "db", schema, table: "a.b", columns: [{ name: column, dataType: "text", nullable: true, isPrimaryKey: false }] },
    indexes: [{ name: "表达式索引", columns: [], isUnique: false, isPrimary: false,
      keyParts: [{ expression: 'lower("名称")', descending: true }], includedColumns: ["附加列"], predicate: '"启用" = 1',
      definition: 'CREATE INDEX "表达式索引" ON "one"."a.b" (lower("名称")) WHERE "启用" = 1' }],
    foreignKeys: [{ name: "fk", columns: ["a,b"], refDatabase: "db", refSchema: "foreign", refTable: "target.table", refColumns: ["id,part"] }] };
}
function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((done) => { resolve = done; });
  return { promise, resolve };
}
function text(node: ReactTestInstance): string {
  return node.children.map((child) => typeof child === "string" ? child : text(child)).join("");
}
function button(label: string): ReactTestInstance {
  const found = view!.root.findAllByType("button").find((node) => text(node) === label);
  assert.ok(found, `应存在按钮：${label}`); return found;
}
function columnInput() { return view!.root.findAllByType("input").find((node) => node.props.placeholder === "列名")!; }
async function mount(schema = "one") {
  await act(async () => { view = create(<TableStructure connectionId="A" database="db" schema={schema} table="a.b" />); });
}
async function switchSchema(schema: string) {
  await act(async () => { view!.update(<TableStructure connectionId="A" database="db" schema={schema} table="a.b" />); });
}

beforeEach(() => {
  metadataCache.clear(); requests = []; submissions = [];
  useI18nStore.setState({ locale: "zh-CN" });
  useGridDraftStore.setState({ drafts: {}, navigation: null });
  useConnectionStore.setState({ saved: [{ id: "A", name: "连接A", driverType: "postgres", host: "localhost", port: 5432, user: "test", password: "" }],
    liveIds: new Set(["A"]), connectionEpochs: { A: 1 } });
  tauriService.getTableMetadata = async (connectionId, database, parts) => {
    requests.push({ connectionId, database, parts }); return snapshot(parts[0]);
  };
  tauriService.executeDdl = async (request) => { submissions.push(request); metadataCache.invalidateConnection(request.connectionId); return request.statements.length; };
});

afterEach(() => {
  act(() => view?.unmount()); view = undefined;
  tauriService.getTableMetadata = original.get; tauriService.executeDdl = original.execute; tauriService.readTablePage = original.read;
  metadataCache.clear(); useConnectionStore.setState(originalConnections); useI18nStore.setState({ locale: originalLocale });
  useGridDraftStore.setState({ drafts: {}, navigation: null });
});

test("同名跨 schema 结构分别缓存；显式刷新绕过缓存；表达式/包含列和外键身份保持原貌", async () => {
  await mount(); assert.equal(columnInput().props.value, "列_one");
  await switchSchema("two"); assert.equal(columnInput().props.value, "列_two");
  await switchSchema("one"); assert.equal(columnInput().props.value, "列_one");
  assert.deepEqual(requests.map((entry) => entry.parts), [["one", "a.b"], ["two", "a.b"]]);
  await act(async () => { await button("刷新").props.onClick(); });
  assert.equal(requests.length, 3);
  await act(async () => { button("索引").props.onClick(); });
  const output = JSON.stringify(view!.toJSON());
  assert.match(output, /lower/); assert.match(output, /INCLUDE/); assert.match(output, /WHERE/); assert.match(output, /附加列/);
  assert.equal(view!.root.findAllByType("input").filter((node) => node.props.type !== "checkbox").length, 0);
  await act(async () => { button("外键").props.onClick(); });
  assert.match(JSON.stringify(view!.toJSON()), /foreign/); assert.match(JSON.stringify(view!.toJSON()), /target.table/);
  assert.match(JSON.stringify(view!.toJSON()), /a,b/);
});

test("结构读取迟到不能覆盖另一 schema，epoch 变化使旧缓存不可复用", async () => {
  const old = deferred<MetadataSnapshot>();
  tauriService.getTableMetadata = async (connectionId, database, parts) => {
    requests.push({ connectionId, database, parts }); return parts[0] === "one" ? old.promise : snapshot("two");
  };
  await mount();
  await switchSchema("two"); assert.equal(columnInput().props.value, "列_two");
  await act(async () => { old.resolve(snapshot("one", "旧列")); await old.promise; });
  assert.equal(columnInput().props.value, "列_two");
  await act(async () => { useConnectionStore.setState({ connectionEpochs: { A: 2 } }); });
  assert.equal(requests.length, 3); assert.equal(columnInput().props.value, "列_two");
});

test("DDL 冻结后端代次、revision和分段目标，提交后刷新原表，不使用连接树选择", async () => {
  await mount();
  act(() => { columnInput().props.onChange({ target: { value: "新列" } }); });
  await act(async () => { await button("提交").props.onClick(); });
  assert.deepEqual(submissions, [{ connectionId: "A", connectionGeneration: "generation-A", database: "db",
    tableParts: ["one", "a.b"], revision: "revision-one",
    statements: ['ALTER TABLE "one"."a.b" RENAME COLUMN "列_one" TO "新列";'] }]);
  assert.equal(requests.length, 2);
});

test("提交中切来源再切回：迟到完成不能重载或清除新草稿", async () => {
  const done = deferred<number>();
  tauriService.executeDdl = async (request) => { submissions.push(request); return done.promise; };
  await mount();
  act(() => { columnInput().props.onChange({ target: { value: "第一次修改" } }); });
  let submitting: Promise<void>;
  act(() => { submitting = button("提交").props.onClick(); });
  await switchSchema("two"); await switchSchema("one");
  act(() => { columnInput().props.onChange({ target: { value: "返回后的草稿" } }); });
  const count = requests.length;
  await act(async () => { done.resolve(1); await submitting!; });
  assert.equal(requests.length, count); assert.equal(columnInput().props.value, "返回后的草稿");
});

test("连接断开后原结构请求迟到不能恢复编辑，数据页也逐段传递 schema 和含点表名", async () => {
  const old = deferred<MetadataSnapshot>(); tauriService.getTableMetadata = async () => old.promise;
  await mount();
  await act(async () => { useConnectionStore.setState({ connectionEpochs: { A: 2 }, liveIds: new Set() }); });
  await act(async () => { old.resolve(snapshot()); await old.promise; });
  assert.equal(view!.root.findAllByType("input").length, 0);
  const reads: unknown[] = [];
  tauriService.readTablePage = async (request) => { reads.push(request); throw new Error("只验证来源"); };
  await act(async () => { useConnectionStore.setState({ liveIds: new Set(["A"]) });
    view!.update(<TableData connectionId="A" database="db" schema="two" table="a.b" />); });
  assert.deepEqual(reads, [{ protocolVersion: 1, connectionId: "A", database: "db", tableParts: ["two", "a.b"], page: 1, pageSize: 20, sort: null }]);
});
