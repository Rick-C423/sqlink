import assert from "node:assert/strict";
import { test } from "node:test";
import { buildChangeSetRequest, type ChangeSetDraftOperation } from "../src/lib/changeset";
import { getGridEditPolicy } from "../src/lib/gridEdits";
import { tauriService } from "../src/services/tauriService";
import type { QueryResult } from "../src/types/database";
import type { ChangeSetValidationResult } from "../src/types/typedValue";

const ids = [
  "00000000-0000-4000-8000-000000000001", "00000000-0000-4000-8000-000000000002",
  "00000000-0000-4000-8000-000000000003", "00000000-0000-4000-8000-000000000004",
  "00000000-0000-4000-8000-000000000005", "00000000-0000-4000-8000-000000000006",
];
const columnId = "00000000-0000-4000-8000-000000000007";
function result(): QueryResult {
  return {
    columns: [{ name: "金额", dataType: "decimal", unifiedType: "decimal", nullable: true }],
    // 显示值刻意含 SQL 字样与不安全数字，builder 不能读取或发送这些显示值。
    rows: [{ values: ["'; DROP TABLE secret; --"] }, { values: [9007199254740992] }], executionTimeMs: 1,
    rowHandles: [ids[4], ids[5]],
    resultContext: {
      protocolVersion: 1, resultId: ids[0], revision: 2, connectionId: "秘密连接", connectionGeneration: "generation",
      source: { database: "秘密库", schema: "财务", table: "秘密表", kind: "table" },
      columns: [{ columnId, displayName: "金额", databaseType: "decimal", nullable: true,
        origin: { database: "秘密库", schema: "财务", table: "秘密表", column: "金额" }, writable: false, generated: false }],
      editPolicy: { allowedOperations: [], reasonCodes: ["SESSION_LEASE_REQUIRED", "COMMIT_RECEIPT_REQUIRED"] }, createdAt: 0, expiresAt: 100,
    },
  };
}
const change = { columnIndex: 0, input: { mode: "set", value: { kind: "decimal", value: "9007199254740993.4500" } } } as const;
const options = { changeSetId: ids[1] };

test("builder 仅从列身份和行句柄构造结构化 insert/update/delete，保留类型化精度", () => {
  const source = result();
  const request = buildChangeSetRequest(source, [
    { kind: "insert", opId: ids[2], changes: [change] },
    { kind: "update", opId: ids[3], rowIndex: 0, changes: [{ columnIndex: 0, input: { mode: "default" } }] },
    { kind: "delete", opId: ids[4], rowIndex: 1 },
  ], options);
  assert.equal(request.protocolVersion, 1);
  assert.equal(request.resultId, ids[0]);
  assert.equal(request.revision, 2);
  assert.deepEqual(JSON.parse(JSON.stringify(request.operations)), [
    { kind: "insert", opId: ids[2], changes: { [columnId]: change.input } },
    { kind: "update", opId: ids[3], rowHandle: ids[4], changes: { [columnId]: { mode: "default" } } },
    { kind: "delete", opId: ids[4], rowHandle: ids[5] },
  ]);
  const payload = JSON.stringify(request);
  for (const secret of ["connectionId", "database", "primaryKeys", "秘密", "DROP TABLE", "sql"]) assert.equal(payload.includes(secret), false, secret);
  assert.equal(getGridEditPolicy(source).editable, false);
  assert.deepEqual(source.resultContext!.editPolicy.allowedOperations, []);
  assert.equal(source.resultContext!.columns[0].writable, false);
});

test("调用方可提供全部 UUID 或注入生成器，构造不依赖浏览器环境", () => {
  let generated = 0;
  const request = buildChangeSetRequest(result(), [{ kind: "insert", changes: [change] }], { generateId: () => ids[++generated] });
  assert.equal(generated, 2);
  assert.equal(request.changeSetId, ids[1]);
  assert.equal(request.operations[0].opId, ids[2]);
  assert.throws(() => buildChangeSetRequest(result(), [], { changeSetId: "not-uuid" }), /UUID/);
  const unsafe = result(); unsafe.resultContext!.revision = Number.MAX_SAFE_INTEGER + 1;
  assert.throws(() => buildChangeSetRequest(unsafe, [], options), /上下文/);
});

test("缺失上下文、行句柄或来源不能用客户端显示值与主键自造", () => {
  for (const mutate of [
    (source: QueryResult) => { delete source.resultContext; },
    (source: QueryResult) => { source.resultContext!.resultId = "fake-result"; },
    (source: QueryResult) => { source.resultContext!.source.kind = "view"; },
    (source: QueryResult) => { delete source.rowHandles; },
    (source: QueryResult) => { source.rowHandles = [null, ids[5]]; },
    (source: QueryResult) => { source.rowHandles = ["fake-row", ids[5]]; },
    (source: QueryResult) => { source.rowHandles = []; },
  ]) {
    const source = result(); mutate(source);
    assert.throws(() => buildChangeSetRequest(source, [{ kind: "delete", opId: ids[2], rowIndex: 0 }], options));
  }
  for (const rowIndex of [-1, 2, 0.5, NaN]) {
    assert.throws(() => buildChangeSetRequest(result(), [{ kind: "delete", opId: ids[2], rowIndex }], options), /句柄映射/);
  }
});

test("未知/重复列、生成列及缺失或错位来源均拒绝，不将 writable:false 变为授权", () => {
  for (const changes of [[{ ...change, columnIndex: -1 }], [{ ...change, columnIndex: 1 }],
    [{ ...change, columnIndex: 0.5 }], [change, change]]) {
    assert.throws(() => buildChangeSetRequest(result(), [{ kind: "insert", opId: ids[2], changes }], options), /列索引/);
  }
  for (const mutate of [
    (source: QueryResult) => { source.resultContext!.columns[0].generated = true; },
    (source: QueryResult) => { source.resultContext!.columns[0].origin = null; },
    (source: QueryResult) => { source.resultContext!.columns[0].origin!.table = "other"; },
    (source: QueryResult) => { source.resultContext!.columns[0].origin!.database = "other"; },
    (source: QueryResult) => { source.resultContext!.columns[0].origin!.schema = "other"; },
    (source: QueryResult) => { source.resultContext!.columns[0].origin!.column = "other"; },
    (source: QueryResult) => { source.resultContext!.columns[0].displayName = "other"; },
  ]) {
    const source = result(); mutate(source);
    assert.throws(() => buildChangeSetRequest(source, [{ kind: "insert", opId: ids[2], changes: [change] }], options));
  }
});

test("同一行或操作身份不能重复，未类型化的显示值不能混入请求", () => {
  assert.throws(() => buildChangeSetRequest(result(), [
    { kind: "update", opId: ids[2], rowIndex: 0, changes: [change] },
    { kind: "delete", opId: ids[3], rowIndex: 0 },
  ], options), /同一行句柄/);
  assert.throws(() => buildChangeSetRequest(result(), [
    { kind: "insert", opId: ids[2], changes: [change] }, { kind: "insert", opId: ids[2], changes: [] },
  ], options), /重复操作身份/);
  const untyped = [{ kind: "insert", opId: ids[2], changes: [{ columnIndex: 0, input: "NULL" }] }] as unknown as ChangeSetDraftOperation[];
  assert.throws(() => buildChangeSetRequest(result(), untyped, options), /显式类型化值/);
});

test("变更验证服务只调用 changeset_validate 且仅返回不可执行摘要", async () => {
  const request = buildChangeSetRequest(result(), [{ kind: "insert", opId: ids[2], changes: [change] }], options);
  const response: ChangeSetValidationResult = {
    changeSetId: request.changeSetId, resultId: request.resultId, revision: request.revision, operationCount: 1,
    executable: false, reasonCodes: ["SESSION_LEASE_REQUIRED", "COMMIT_RECEIPT_REQUIRED"],
  };
  const target = globalThis as typeof globalThis & { window?: unknown };
  const previous = target.window;
  const calls: unknown[] = [];
  target.window = { __TAURI_INTERNALS__: { invoke: async (command: string, args: unknown) => {
    calls.push([command, args]); return response;
  } } };
  try {
    assert.deepEqual(await tauriService.validateChangeSet(request), response);
    assert.deepEqual(calls, [["changeset_validate", { request }]]);
    assert.equal(getGridEditPolicy(result()).editable, false);
  } finally {
    if (previous === undefined) delete target.window; else target.window = previous;
  }
});
