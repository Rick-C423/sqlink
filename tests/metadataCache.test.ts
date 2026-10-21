import assert from "node:assert/strict";
import { test } from "node:test";
import { MetadataCache, metadataCache, type MetadataCacheIdentity } from "../src/lib/metadataCache";
import { tauriService } from "../src/services/tauriService";
import { useConnectionStore } from "../src/stores/connectionStore";

const identity: MetadataCacheIdentity = { connectionId: "A", connectionEpoch: 1, database: "db", schema: "one", table: "a.b", category: "structure" };
const value = (connectionGeneration = "g1", payload = "结构") => ({ connectionGeneration, payload });
function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((done) => { resolve = done; });
  return { promise, resolve };
}

test("缓存身份隔离 schema、点号名称、连接、epoch、generation 和类别", async () => {
  const cache = new MetadataCache();
  let calls = 0;
  const load = async () => { calls++; return value(); };
  await cache.get(identity, load);
  await cache.get({ ...identity, connectionGeneration: "g1" }, load);
  assert.equal(calls, 1);
  for (const changed of [{ schema: "two" }, { table: "a", schema: "b" }, { connectionId: "B" },
    { connectionEpoch: 2 }, { connectionGeneration: "g2" }, { category: "other" }]) {
    await cache.get({ ...identity, ...changed }, async () => { calls++; return value(changed.connectionGeneration ?? "g1"); });
  }
  assert.equal(calls, 7);
  await assert.rejects(cache.get({ ...identity, connectionGeneration: "g3" }, async () => value("g4")), /代次已变化/);
});

test("在途复用，连接失效和显式刷新后迟到旧请求不能复活缓存", async () => {
  const cache = new MetadataCache();
  const old = deferred<ReturnType<typeof value>>();
  let calls = 0;
  const first = cache.get(identity, () => { calls++; return old.promise; });
  const duplicate = cache.get(identity, () => { assert.fail("同身份在途请求应复用"); });
  await Promise.resolve();
  assert.equal(calls, 1);
  cache.invalidateConnection("A");
  await cache.get(identity, async () => value("g2", "新连接"));
  old.resolve(value("g1", "旧连接"));
  await Promise.all([first, duplicate]);
  assert.equal((await cache.get(identity, async () => { assert.fail("应命中新连接缓存"); })).payload, "新连接");
  const late = deferred<ReturnType<typeof value>>();
  const refreshing = cache.get(identity, () => late.promise, true);
  await Promise.resolve();
  await cache.get(identity, async () => value("g3", "显式刷新"), true);
  late.resolve(value("g2", "迟到刷新")); await refreshing;
  assert.equal((await cache.get(identity, async () => value())).payload, "显式刷新");
  cache.clear();
  assert.equal((await cache.get(identity, async () => value("g4"))).connectionGeneration, "g4");
});

test("缓存遵守 LRU 条目上限、字节上限和 TTL，失败不缓存", async () => {
  const cache = new MetadataCache({ maxEntries: 1, maxBytes: 100, ttlMs: 5 });
  let calls = 0;
  const load = async () => { calls++; return value(); };
  await cache.get(identity, load);
  await cache.get({ ...identity, schema: "two" }, load);
  await cache.get(identity, load);
  assert.equal(calls, 3);
  await cache.get(identity, async () => value("g1", "超".repeat(100)), true);
  await cache.get(identity, load);
  assert.equal(calls, 4);
  const originalNow = Date.now;
  try {
    const now = Date.now(); Date.now = () => now + 10;
    await cache.get(identity, load);
  } finally { Date.now = originalNow; }
  assert.equal(calls, 5);
  await assert.rejects(cache.get(identity, async () => { throw new Error("失败"); }, true), /失败/);
  await cache.get(identity, load);
  assert.equal(calls, 6);
});

test("活动连接切换使离开与进入连接的缓存失效，相同连接选择不丢缓存", async () => {
  const previous = useConnectionStore.getState();
  try {
    useConnectionStore.setState({ activeConnection: "A" });
    const b = { ...identity, connectionId: "B" };
    let calls = 0;
    const load = async () => { calls++; return value(); };
    await metadataCache.get(identity, load); await metadataCache.get(b, load);
    useConnectionStore.getState().setActiveConnection("A");
    await metadataCache.get(identity, load);
    assert.equal(calls, 2);
    useConnectionStore.getState().setActiveConnection("B");
    await metadataCache.get(identity, load); await metadataCache.get(b, load);
    assert.equal(calls, 4);
  } finally { useConnectionStore.setState(previous); metadataCache.clear(); }
});

test("显式 query/batch/DDL 的成功或失败都使该连接缓存失效，IPC 保留完整结构身份", async () => {
  const originalWindow = Object.getOwnPropertyDescriptor(globalThis, "window");
  let command = ""; let args: unknown; let fail = false;
  Object.defineProperty(globalThis, "window", { configurable: true, value: { __TAURI_INTERNALS__: {
    invoke: async (cmd: string, input: unknown) => { command = cmd; args = input; if (fail) throw new Error("部分生效"); return 1; },
  } } });
  const request = { connectionId: "A", connectionGeneration: "g1", database: "db", tableParts: ["one", "a.b"], revision: "revision-9", statements: ["ALTER TABLE ..."] };
  try {
    for (const action of [() => tauriService.executeQuery("A", "SELECT 1", "db"),
      () => tauriService.executeBatch("A", "db", ["SELECT 1"]), () => tauriService.executeDdl(request),
      () => tauriService.importSqlFile("A", "db", "snapshot.sql", "error")]) {
      for (fail of [false, true]) {
        await metadataCache.get(identity, async () => value());
        if (fail) await assert.rejects(action(), /部分生效/); else await action();
        let reloaded = false;
        await metadataCache.get(identity, async () => { reloaded = true; return value(); });
        assert.equal(reloaded, true);
      }
    }
    fail = false; await tauriService.executeDdl(request);
    assert.equal(command, "schema_execute_ddl"); assert.deepEqual(args, { request });
    await tauriService.getTableMetadata("A", "db", ["one", "a.b"]);
    assert.equal(command, "schema_get_table_metadata");
    assert.deepEqual(args, { connectionId: "A", database: "db", tableParts: ["one", "a.b"] });
    await tauriService.exportTableCsv("A", "db", "a.b", "out.csv", "task", "one");
    assert.deepEqual(args, { connectionId: "A", database: "db", table: "a.b", path: "out.csv", taskId: "task", schema: "one" });
    const tableSources = [{ database: "db", schema: "one", table: "a.b" }];
    await tauriService.dumpSqlFile("A", "db", undefined, "out.sql", true, false, "task", tableSources);
    assert.deepEqual(args, { connectionId: "A", database: "db", tables: undefined, path: "out.sql", includeDrop: true, includeData: false, taskId: "task", tableSources });
    await tauriService.getViewDefinition("A", "db", "v.x", "one");
    assert.deepEqual(args, { connectionId: "A", database: "db", view: "v.x", schema: "one" });
  } finally {
    metadataCache.clear();
    if (originalWindow) Object.defineProperty(globalThis, "window", originalWindow); else Reflect.deleteProperty(globalThis, "window");
  }
});
