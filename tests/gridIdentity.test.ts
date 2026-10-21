import assert from "node:assert/strict";
import { test } from "node:test";
import { getGridEditPolicy, validateGridIdentity } from "../src/lib/gridEdits";
import type { ControlledGridOrigin } from "../src/lib/gridEdits";
import type { EditContext, QueryResult } from "../src/types/database";

const context: EditContext = { connectionId: "A", database: "db", schema: "public", table: "users", driverType: "postgres", primaryKeys: ["id"] };
const data = (names = ["id", "name"], rows: QueryResult["rows"] = [{ values: [1, "甲"] }]): QueryResult => ({
  columns: names.map((name) => ({ name, dataType: name === "id" ? "integer" : "text", unifiedType: name === "id" ? "Integer" : "String", nullable: false })),
  rows, executionTimeMs: 1,
});
const origin = (names = ["id", "name"]): ControlledGridOrigin => ({ kind: "controlled-table", columns: names.map((sourceColumn) => ({ sourceColumn, direct: true, supported: true })) });

test("计算列 id+1 AS id 不能凭同名列定位原行", () => {
  const computed = data(["id", "name"], [{ values: [2, "甲"] }]);
  assert.equal(validateGridIdentity(computed, context).valid, false);
  const mapping = origin();
  mapping.columns[0].direct = false;
  assert.match(validateGridIdentity(computed, context, mapping).reason, /表达式/);
  assert.equal(getGridEditPolicy(computed, context).editable, false);
});

test("候选受控直接列与完整非空唯一键通过身份检查，但无后端上下文仍只读", () => {
  assert.equal(validateGridIdentity(data(), context, origin()).valid, true);
  assert.equal(getGridEditPolicy(data(), context).editable, false);
});

test("拒绝重复显示列、重复源字段、别名覆盖和不支持类型", () => {
  assert.match(validateGridIdentity(data(["id", "id"]), context, origin(["id", "id"])).reason, /重复/);
  const duplicate = origin(); duplicate.columns[1].sourceColumn = "id";
  assert.match(validateGridIdentity(data(), context, duplicate).reason, /重复/);
  const alias = origin(); alias.columns[0].sourceColumn = "other_key";
  assert.match(validateGridIdentity(data(), context, alias).reason, /别名/);
  const unsupported = origin(); unsupported.columns[1].supported = false;
  assert.equal(validateGridIdentity(data(), context, unsupported).valid, false);
});

test("缺键、复合键不完整、重复键映射一律拒绝", () => {
  assert.equal(validateGridIdentity(data(), { ...context, primaryKeys: [] }, origin()).valid, false);
  assert.match(validateGridIdentity(data(), { ...context, primaryKeys: ["id", "tenant"] }, origin()).reason, /完整主键/);
  assert.equal(validateGridIdentity(data(), { ...context, primaryKeys: ["id", "id"] }, origin()).valid, false);
  const composite = data(["id", "tenant"], [{ values: [1, "a"] }, { values: [1, "b"] }]);
  assert.equal(validateGridIdentity(composite, { ...context, primaryKeys: ["id", "tenant"] }, origin(["id", "tenant"])).valid, true);
});

test("空键、重复行身份、不完整行和超安全整数不得用于写入", () => {
  for (const values of [[null, "甲"], [Number.MAX_SAFE_INTEGER + 1, "甲"], [1.25, "甲"], [Infinity, "甲"]]) {
    assert.equal(validateGridIdentity(data(undefined, [{ values }]), context, origin()).valid, false);
  }
  assert.match(validateGridIdentity(data(undefined, [{ values: [1, "a"] }, { values: [1, "b"] }]), context, origin()).reason, /重复行身份/);
  assert.equal(validateGridIdentity(data(undefined, [{ values: [1] }]), context, origin()).valid, false);
});
