import assert from "node:assert/strict";
import { test } from "node:test";
import { parseGridCellInput, typedValueInputText, gridColumnInputKind, buildGridDraftChangeSet } from "../src/lib/gridCellInput";
import { materializeGridDraft } from "../src/stores/gridDraftStore";
import type { GridDraft } from "../src/stores/gridDraftStore";
import type { QueryResult } from "../src/types/database";

const id = (n: number) => `00000000-0000-4000-8000-${String(n).padStart(12, "0")}`;
function source(): QueryResult {
  return { columns: [{ name: "文本", dataType: "text", unifiedType: "string", nullable: true }], rows: [{ values: ["显示值"] }],
    typedRows: [[{ kind: "text", value: "无损原值" }]], rowHandles: [id(3)], executionTimeMs: 1,
    resultContext: { protocolVersion: 1, resultId: id(1), revision: 1, connectionId: "A", connectionGeneration: "g",
      source: { database: "db", table: "t", kind: "table" }, columns: [{ columnId: id(2), displayName: "文本", databaseType: "text",
        nullable: true, writable: true, generated: false, origin: { database: "db", table: "t", column: "文本" } }],
      editPolicy: { allowedOperations: ["insert", "update", "delete"], reasonCodes: [], columnInputKinds: { [id(2)]: "text" } }, createdAt: 1, expiresAt: 100 } };
}
function draft(): GridDraft { return { sourceResult: source(), edits: {}, newRows: [], deletedRows: new Set(), editingCell: null, submitting: false, error: null }; }

test("网格文本空串、字面NULL、明确SQL NULL与DEFAULT保持独立意图", () => {
  assert.deepEqual(parseGridCellInput("text", ""), { mode: "set", value: { kind: "text", value: "" } });
  assert.deepEqual(parseGridCellInput("text", "NULL"), { mode: "set", value: { kind: "text", value: "NULL" } });
  assert.deepEqual(parseGridCellInput("text", "", "null"), { mode: "set", value: { kind: "null" } });
  assert.deepEqual(parseGridCellInput("text", "", "default"), { mode: "default" });
  assert.throws(() => parseGridCellInput("text", "", "null", false), /不允许/);
});

test("网格整数不经Number，拒绝非规范十进制与越界，保留uint64最大值", () => {
  assert.deepEqual(parseGridCellInput("integer", "9007199254740993"), { mode: "set", value: { kind: "int64", value: "9007199254740993" } });
  assert.deepEqual(parseGridCellInput("unsignedInteger", "18446744073709551615"), { mode: "set", value: { kind: "uint64", value: "18446744073709551615" } });
  for (const text of ["", "01", "-0", "+1", "1e2", "1.0", " 1", "9223372036854775808"]) assert.throws(() => parseGridCellInput("integer", text));
  for (const text of ["-1", "18446744073709551616"]) assert.throws(() => parseGridCellInput("unsignedInteger", text));
});

test("网格二进制显式hex往返，布尔和浮点严格校验", () => {
  assert.deepEqual(parseGridCellInput("binary", "00fF"), { mode: "set", value: { kind: "binary", value: "AP8=" } });
  assert.equal(typedValueInputText({ kind: "binary", value: "AP8=" }), "00ff");
  assert.throws(() => parseGridCellInput("binary", "0x00")); assert.throws(() => parseGridCellInput("binary", "f"));
  assert.deepEqual(parseGridCellInput("boolean", "false"), { mode: "set", value: { kind: "boolean", value: false } });
  for (const value of ["", "0", "False"]) assert.throws(() => parseGridCellInput("boolean", value));
  for (const value of ["", "NaN", "Infinity", "0x10", "1e999"]) assert.throws(() => parseGridCellInput("float", value));
});

test("草稿实体化使用无损原值，新增空文本保留，省略字段仍为DEFAULT", () => {
  let current = draft(); current.editingCell = { key: "0:0", value: "无损原值" };
  assert.deepEqual(materializeGridDraft(current).edits, {});
  current.newRows = [{}, {}]; current.editingCell = { key: "new:0:0", value: "", intent: "value" };
  current = materializeGridDraft(current);
  assert.deepEqual(current.newRows, [{ 0: "" }, {}]);
  const request = buildGridDraftChangeSet(current);
  assert.deepEqual(request.operations.map((op) => op.kind === "insert" ? { ...op.changes } : null), [{ [id(2)]: { mode: "set", value: { kind: "text", value: "" } } }, {}]);
  current.editingCell = { key: "new:0:0", value: "", intent: "default" };
  assert.deepEqual(materializeGridDraft(current).newRows, [{}, {}]);
});

test("字段权限、类型和操作均取后端策略，生成列与未发布字段保持只读", () => {
  const current = draft(); const result = current.sourceResult!;
  assert.equal(gridColumnInputKind(result, 0), "text");
  current.edits = { "0:0": "新值" }; result.resultContext!.editPolicy.allowedOperations = ["delete"];
  assert.throws(() => buildGridDraftChangeSet(current), /未授权/);
  result.resultContext!.editPolicy.allowedOperations = ["update"];
  result.resultContext!.columns[0].generated = true;
  assert.equal(gridColumnInputKind(result, 0), undefined); assert.throws(() => buildGridDraftChangeSet(current), /只读/);
  result.resultContext!.columns[0].generated = false; delete result.resultContext!.editPolicy.columnInputKinds;
  assert.equal(gridColumnInputKind(result, 0), undefined);
  result.resultContext!.editPolicy.columnInputKinds = { [id(2)]: "text" }; result.resultContext!.columns[0].writable = false;
  assert.equal(gridColumnInputKind(result, 0), undefined);
});
