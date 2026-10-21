import assert from "node:assert/strict";
import { test } from "node:test";
import { attachResultContext, formatResultContextError, retireResultContext } from "../src/lib/resultContext";
import type { ControlledTablePage, ControlledTableReadRequest } from "../src/types/resultContext";
import type { GridDraft } from "../src/stores/gridDraftStore";

const request: ControlledTableReadRequest = {
  protocolVersion: 1, connectionId: "A", database: "db", tableParts: ["items"], page: 1, pageSize: 20,
};

test("结构化上下文错误显示诊断码和重载提示，其他错误保留可读文本", () => {
  assert.equal(formatResultContextError({ code: "RESULT_EXPIRED", message: "结果已过期，请重新加载。" }), "结果已过期，请重新加载。（RESULT_EXPIRED）");
  assert.equal(formatResultContextError(new Error("连接不可用")), "连接不可用");
  assert.equal(formatResultContextError("请求失败"), "请求失败");
  assert.equal(formatResultContextError(null), "null");
});
function page(): ControlledTablePage {
  return {
    result: {
      columns: [{ name: "id", dataType: "int", unifiedType: "integer", nullable: false }],
      rows: [{ values: [1] }], executionTimeMs: 1,
    },
    context: {
      protocolVersion: 1, resultId: "result-A", revision: 1, connectionId: "A", connectionGeneration: "generation-A",
      source: { database: "db", table: "items", kind: "table" },
      columns: [{ columnId: "c1", displayName: "id", databaseType: "int", nullable: false, writable: false, generated: false }],
      editPolicy: { allowedOperations: [], reasonCodes: ["READ_ONLY"] }, createdAt: 1, expiresAt: 100,
    },
    rowHandles: ["opaque-row-1"],
    table: { database: "db", table: "items", columns: [{ name: "id", dataType: "int", nullable: false, isPrimaryKey: true }] },
    totalRows: 1,
  };
}

test("受控结果只接受 v1 后端来源并明确附上对应行句柄", () => {
  const response = page();
  const result = attachResultContext(response, request);
  assert.equal(result.resultContext, response.context);
  assert.equal(result.rowHandles, response.rowHandles);
  assert.equal(response.result.resultContext, undefined);
});

test("无损通道保留大整数、SQL NULL 与无法无损读取标志，绝不从显示值推断", () => {
  for (const value of [{ kind: "int64", value: "9007199254740993" } as const, { kind: "null" } as const, null]) {
    const response = page(); response.typedRows = [[value]];
    const result = attachResultContext(response, request);
    assert.equal(result.typedRows, response.typedRows);
    assert.deepEqual(result.typedRows, [[value]]);
  }
  const response = page();
  assert.equal(attachResultContext(response, request).typedRows, undefined);
  response.result.typedRows = [[{ kind: "text", value: "未验证的内部通道" }]];
  assert.equal(attachResultContext(response, request).typedRows, undefined);
});

test("无损通道行列错位、无效载荷或 DEFAULT 不能附到可信结果", () => {
  const invalidRows: unknown[] = [[], [[]], [[{ kind: "int64", value: "01" }]], [[{ kind: "default" }]],
    [[{ kind: "int64", value: 9007199254740992 }]], [[{ kind: "null" }, null]], [[undefined]], [new Array(1)], new Array(1)];
  for (const typedRows of invalidRows) {
    const response = page(); response.typedRows = typedRows as ControlledTablePage["typedRows"];
    assert.throws(() => attachResultContext(response, request), /无损值/);
    assert.equal(response.result.resultContext, undefined);
  }
});

test("不兼容协议、错误连接、错位列与错位行句柄均不能成为可信结果", () => {
  for (const change of [
    (response: ControlledTablePage) => { response.context.protocolVersion = 2; },
    (response: ControlledTablePage) => { response.context.connectionId = "B"; },
    (response: ControlledTablePage) => { response.context.columns = []; },
    (response: ControlledTablePage) => { response.context.columns[0].displayName = "other"; },
    (response: ControlledTablePage) => { response.rowHandles = []; },
    (response: ControlledTablePage) => { response.context.editPolicy.allowedOperations = ["update"]; },
  ]) {
    const response = page(); change(response);
    assert.throws(() => attachResultContext(response, request), /受控结果上下文无效/);
    assert.equal(response.result.resultContext, undefined);
  }
});

test("干净结果可释放；脏草稿与进行中的提交保留原始 context", async () => {
  const response = page();
  const sourceResult = attachResultContext(response, request);
  const draft: GridDraft = {
    sourceResult, edits: {}, newRows: [], deletedRows: new Set(), editingCell: null, submitting: false, error: null,
  };
  const released: unknown[] = [];
  const release = async (resultId: string, revision: number) => { released.push([resultId, revision]); };
  assert.equal(await retireResultContext(response.context, [{ ...draft, edits: { "0:0": "2" } }], release), false);
  assert.equal(await retireResultContext(response.context, [{ ...draft, submitting: true }], release), false);
  assert.deepEqual(released, []);
  assert.equal(await retireResultContext(response.context, [draft], release), true);
  assert.deepEqual(released, [["result-A", 1]]);
});

test("其他结果的脏草稿不阻止回收，回收失败由 TTL 兜底", async () => {
  const response = page();
  const other = page(); other.context.resultId = "other-result";
  const otherDraft: GridDraft = {
    sourceResult: attachResultContext(other, request), edits: { "0:0": "2" }, newRows: [], deletedRows: new Set(),
    editingCell: null, submitting: false, error: null,
  };
  let released = false;
  assert.equal(await retireResultContext(response.context, [otherDraft], async () => { released = true; }), true);
  assert.equal(released, true);
  assert.equal(await retireResultContext(response.context, [], async () => { throw new Error("连接已关闭"); }), false);
  assert.equal(await retireResultContext(undefined, [], async () => { assert.fail("无上下文不应释放"); }), false);
});

function writablePage(): ControlledTablePage {
  const response = page();
  response.context.columns[0].writable = true;
  response.context.columns[0].origin = { database: "db", table: "items", column: "id" };
  response.context.editPolicy = { allowedOperations: ["insert", "update", "delete"], reasonCodes: [], columnInputKinds: { c1: "integer" } };
  response.typedRows = [[{ kind: "int64", value: "9007199254740993" }]];
  return response;
}

test("受控后端发布完整类型能力后保留精度与授权，前端不补造输入类型", () => {
  const response = writablePage();
  const result = attachResultContext(response, request);
  assert.equal(result.resultContext, response.context);
  assert.deepEqual(result.resultContext!.editPolicy.allowedOperations, ["insert", "update", "delete"]);
  assert.deepEqual(result.resultContext!.editPolicy.columnInputKinds, { c1: "integer" });
  assert.deepEqual(result.typedRows, [[{ kind: "int64", value: "9007199254740993" }]]);
});

test("受控授权拒绝未知或重复操作、未知类型、缺失映射、生成列写入与只读能力混入", () => {
  const changes: ((response: ControlledTablePage) => void)[] = [
    (response) => { response.context.editPolicy.allowedOperations = ["upsert"]; },
    (response) => { response.context.editPolicy.allowedOperations = ["update", "update"]; },
    (response) => { response.context.editPolicy.columnInputKinds = { c1: "decimal" as never }; },
    (response) => { response.context.editPolicy.columnInputKinds = { unknown: "text" }; },
    (response) => { delete response.context.editPolicy.columnInputKinds; },
    (response) => { response.context.columns[0].generated = true; },
    (response) => { response.context.columns[0].writable = false; },
    (response) => { response.context.editPolicy.allowedOperations = []; },
    (response) => { response.context.editPolicy.reasonCodes = ["UNVERIFIED_DRIVER"]; },
    (response) => { response.context.source.kind = "view"; },
  ];
  for (const change of changes) {
    const response = writablePage(); change(response);
    assert.throws(() => attachResultContext(response, request), /受控结果上下文无效/);
    assert.equal(response.result.resultContext, undefined);
  }
});

test("可写上下文必须有完整无损快照和可定位句柄，SQL NULL仍保留显式类型", () => {
  for (const change of [
    (response: ControlledTablePage) => { delete response.typedRows; },
    (response: ControlledTablePage) => { response.typedRows = [[null]]; },
    (response: ControlledTablePage) => { response.rowHandles = [null]; },
    (response: ControlledTablePage) => { response.typedRows = []; },
  ]) {
    const response = writablePage(); change(response);
    assert.throws(() => attachResultContext(response, request), /受控结果上下文无效/);
  }
  const response = writablePage(); response.context.columns[0].nullable = true; response.typedRows = [[{ kind: "null" }]];
  assert.deepEqual(attachResultContext(response, request).typedRows, [[{ kind: "null" }]]);
});

test("未知提交与已提交刷新失败保留干净草稿的原context，恢复后才允许回收", async () => {
  const response = page(); const result = attachResultContext(response, request);
  const draft: GridDraft = { sourceResult: result, edits: {}, newRows: [], deletedRows: new Set(), editingCell: null, submitting: false, error: null };
  let releases = 0;
  for (const saveState of ["unknown", "committedRefreshFailed"] as const) {
    assert.equal(await retireResultContext(response.context, [{ ...draft, saveState }], async () => { releases++; }), false);
  }
  assert.equal(releases, 0);
  assert.equal(await retireResultContext(response.context, [{ ...draft, saveState: "committed" }], async () => { releases++; }), true);
  assert.equal(releases, 1);
});
