import assert from "node:assert/strict";
import { test } from "node:test";
import { extractSingleTable, quoteTable } from "../src/lib/sqlTable";
import { buildEditStatements } from "../src/lib/gridEdits";
import type { EditContext, QueryResult } from "../src/types/database";

test("单表提取保留所有限定段和每段内的点号、引号", () => {
  for (const [sql, parts] of [
    ["SELECT * FROM other_schema.users", ["other_schema", "users"]],
    ["SELECT * FROM `other.db`.`user``name`", ["other.db", "user`name"]],
    ['SELECT * FROM "库.一"."表""二"', ["库.一", '表"二']],
    ["SELECT * FROM [库.一].[架构]]名].[表.名] AS t WHERE t.id = 1", ["库.一", "架构]名", "表.名"]],
    ["SELECT * FROM users AS u ORDER BY u.id;", ["users"]],
    ["SELECT * FROM users u WHERE u.id > 1", ["users"]],
  ] as const) {
    const reference = extractSingleTable(sql);
    assert.deepEqual(reference?.parts, parts, sql);
    assert.ok(reference);
    assert.equal(sql.slice(reference.start, reference.end).includes("SELECT"), false);
  }
});

test("注释和字符串中的 FROM、JOIN、分号不会改变表身份", () => {
  const sql = "/* FROM wrong JOIN x */ SELECT 'FROM a; JOIN b' AS text FROM \"s\".\"t\" -- FROM ignored\nWHERE id = 1";
  const reference = extractSingleTable(sql);
  assert.deepEqual(reference?.parts, ["s", "t"]);
  assert.equal(sql.slice(reference!.start, reference!.end), '"s"."t"');
});

test("不能确定单表身份的 SQL 不开放结果编辑", () => {
  for (const sql of [
    "SELECT * FROM a, b", "SELECT * FROM a x, b y", "SELECT * FROM a JOIN b ON a.id=b.id",
    "SELECT * FROM (SELECT * FROM a) x", "WITH x AS (SELECT * FROM a) SELECT * FROM x",
    "SELECT * FROM a UNION SELECT * FROM b", "SELECT * FROM fn(1)",
    "SELECT * FROM a; DELETE FROM b", "SELECT * FROM a; SELECT 1", "SELECT 1",
    "SELECT * FROM [linked].[db].[dbo].[t]", "SELECT * FROM db..t", 'SELECT * FROM "broken',
    "SELECT * FROM a /* broken", "SELECT * INTO copy FROM a",
    "SELECT a.id, b.name FROM a /*! JOIN b ON a.id=b.id */;",
    "SELECT a.id, b.name FROM a /*M! JOIN b ON a.id=b.id */;",
    "SELECT * FROM ONLY a", "SELECT * FROM LATERAL a",
  ]) assert.equal(extractSingleTable(sql), null, sql);
});

test("四种方言按完整身份逐段引用表名", () => {
  const base = { database: "db.name", schema: "s.name", table: "t.name" };
  assert.equal(quoteTable({ ...base, driverType: "mysql" }), "`db.name`.`t.name`");
  assert.equal(quoteTable({ ...base, driverType: "postgres" }), '"s.name"."t.name"');
  assert.equal(quoteTable({ ...base, driverType: "sqlite" }), '"s.name"."t.name"');
  assert.equal(quoteTable({ ...base, driverType: "sqlserver" }), "[db.name].[s.name].[t.name]");
  assert.equal(quoteTable({ database: "db", schema: 's"x', table: 't"x', driverType: "postgres" }), '"s""x"."t""x"');
  assert.equal(quoteTable({ database: "d]x", schema: "s]x", table: "t]x", driverType: "mssql" }), "[d]]x].[s]]x].[t]]x]");
});

const result: QueryResult = {
  columns: ["id", "value"].map((name) => ({ name, dataType: "TEXT", unifiedType: "String", nullable: false })),
  rows: [{ values: ["1", "旧值"] }, { values: ["2", "删除值"] }], executionTimeMs: 1,
};

for (const [driverType, target, id, value] of [
  ["mysql", "`target.db`.`user.name`", "`id`", "`value`"],
  ["postgres", '"other.schema"."user.name"', '"id"', '"value"'],
  ["sqlite", '"other.schema"."user.name"', '"id"', '"value"'],
  ["mssql", "[target.db].[other.schema].[user.name]", "[id]", "[value]"],
]) {
  test(`${driverType} 的 UPDATE、DELETE、INSERT 使用同一个完整目标`, () => {
    const context: EditContext = {
      connectionId: "A", database: "target.db", schema: "other.schema", table: "user.name", driverType, primaryKeys: ["id"],
    };
    assert.deepEqual(buildEditStatements(result, context, { "0:1": "新值", "1:1": "忽略" }, [{ 0: "3", 1: "新增" }], new Set([1])), [
      `UPDATE ${target} SET ${value} = '新值' WHERE ${id} = '1';`,
      `DELETE FROM ${target} WHERE ${id} = '2';`,
      `INSERT INTO ${target} (${id}, ${value}) VALUES ('3', '新增');`,
    ]);
  });
}
