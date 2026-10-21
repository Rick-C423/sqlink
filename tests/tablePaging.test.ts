import assert from "node:assert/strict";
import { test } from "node:test";
import { buildOrderBy, buildPageSql, buildStableSort } from "../src/lib/tablePaging";
import type { TableSort } from "../src/lib/tablePaging";
import type { ColumnInfo } from "../src/types/database";

function column(name: string, isPrimaryKey = false): ColumnInfo {
  return { name, isPrimaryKey, dataType: "TEXT", nullable: !isPrimaryKey };
}

const columns = [column("tenant_id", true), column("ID", true), column("label")];

test("默认补齐复合主键，用户主键升降序均只出现一次", () => {
  assert.deepEqual(buildStableSort(columns, null), [
    { key: "tenant_id", direction: "asc" }, { key: "ID", direction: "asc" },
  ]);
  for (const direction of ["asc", "desc"] as const) {
    assert.deepEqual(buildStableSort(columns, { key: "ID", direction }), [
      { key: "ID", direction }, { key: "tenant_id", direction: "asc" },
    ]);
    assert.deepEqual(buildStableSort([column("ID", true)], { key: "ID", direction }), [
      { key: "ID", direction },
    ]);
  }
});

test("非主键排序优先，完整复合主键作为平级排序依据", () => {
  const sort: TableSort = { key: "label", direction: "desc" };
  assert.deepEqual(buildStableSort(columns, sort), [
    sort, { key: "tenant_id", direction: "asc" }, { key: "ID", direction: "asc" },
  ]);
  assert.deepEqual(sort, { key: "label", direction: "desc" });
  assert.deepEqual(columns.map((item) => item.name), ["tenant_id", "ID", "label"]);
});

test("按元数据恢复大小写，精确匹配优先且不同大小写的独立列不合并", () => {
  assert.deepEqual(buildStableSort(columns, { key: "id", direction: "desc" }), [
    { key: "ID", direction: "desc" }, { key: "tenant_id", direction: "asc" },
  ]);
  const distinct = [column("id", true), column("ID", true), column("value")];
  assert.deepEqual(buildStableSort(distinct, { key: "ID", direction: "desc" }), [
    { key: "ID", direction: "desc" }, { key: "id", direction: "asc" },
  ]);
  assert.equal(buildOrderBy("postgres", distinct, { key: "id", direction: "asc" }), '"id" ASC, "ID" ASC');
});

test("未知、歧义字段和非法方向在生成 SQL 前拒绝", () => {
  assert.throws(() => buildStableSort(columns, { key: "missing", direction: "asc" }), /排序字段不存在/);
  const ambiguous = [column("UserID", true), column("USERID", true)];
  assert.throws(() => buildStableSort(ambiguous, { key: "userid", direction: "asc" }), /排序字段存在歧义/);
  assert.throws(() => buildStableSort([column("ID"), column("ID")], { key: "ID", direction: "asc" }), /排序字段存在歧义/);
  assert.throws(() => buildStableSort(columns, { key: "ID", direction: "desc; DROP TABLE x" } as TableSort), /排序方向/);
  for (const driver of ["mysql", "postgres", "sqlite", "sqlserver"]) {
    assert.throws(() => buildPageSql(driver, "items", "", 1, 20, ambiguous, { key: "userid", direction: "desc" }), /排序字段存在歧义/);
  }
});

for (const [driver, table, tenant, id, label] of [
  ["mysql", "`items`", "`tenant_id`", "`ID`", "`label`"],
  ["postgres", '"items"', '"tenant_id"', '"ID"', '"label"'],
  ["postgresql", '"items"', '"tenant_id"', '"ID"', '"label"'],
  ["sqlite", '"items"', '"tenant_id"', '"ID"', '"label"'],
] as const) {
  test(`${driver} 完整分页 SQL 保留筛选、偏移并补齐稳定排序`, () => {
    assert.equal(buildPageSql(driver, table, ` WHERE ${label} IS NOT NULL`, 3, 20, columns, { key: "label", direction: "desc" }),
      `SELECT * FROM ${table} WHERE ${label} IS NOT NULL ORDER BY ${label} DESC, ${tenant} ASC, ${id} ASC LIMIT 20 OFFSET 40`);
    assert.equal(buildPageSql(driver, table, "", 1, 20, columns, { key: "id", direction: "desc" }),
      `SELECT * FROM ${table} ORDER BY ${id} DESC, ${tenant} ASC LIMIT 20 OFFSET 0`);
    assert.equal(buildPageSql(driver, table, "", 1, 20, columns, null),
      `SELECT * FROM ${table} ORDER BY ${tenant} ASC, ${id} ASC LIMIT 20 OFFSET 0`);
  });
}

for (const driver of ["sqlserver", "mssql"]) {
  test(`${driver} 完整 SQL 使用 ROW_NUMBER，主键不重复且投影不泄漏分页字段`, () => {
    for (const [sort, order] of [
      [null, "[tenant_id] ASC, [ID] ASC"],
      [{ key: "id", direction: "asc" }, "[ID] ASC, [tenant_id] ASC"],
      [{ key: "ID", direction: "desc" }, "[ID] DESC, [tenant_id] ASC"],
      [{ key: "label", direction: "desc" }, "[label] DESC, [tenant_id] ASC, [ID] ASC"],
    ] as [TableSort | null, string][]) {
      assert.equal(buildPageSql(driver, "[db].[dbo].[items]", " WHERE [label] IS NOT NULL", 2, 50, columns, sort),
        "SELECT [__sqlconnect_page].[tenant_id], [__sqlconnect_page].[ID], [__sqlconnect_page].[label] FROM (" +
        `SELECT *, ROW_NUMBER() OVER (ORDER BY ${order}) AS [__sqlconnect_row_number] ` +
        "FROM [db].[dbo].[items] WHERE [label] IS NOT NULL) AS [__sqlconnect_page] " +
        "WHERE [__sqlconnect_page].[__sqlconnect_row_number] BETWEEN 51 AND 100 " +
        "ORDER BY [__sqlconnect_page].[__sqlconnect_row_number]");
    }
  });
}

test("特殊转义列名按列身份去重，用户方向和规范列名保留", () => {
  for (const [driver, name, quoted] of [
    ["mysql", "a`b", "`a``b`"],
    ["postgres", 'a"b', '"a""b"'],
    ["sqlite", 'a"b', '"a""b"'],
    ["sqlserver", "a]b", "[a]]b]"],
  ] as const) {
    const escaped = [column(name, true), column("other", true)];
    const other = driver === "mysql" ? "`other`" : driver === "sqlserver" ? "[other]" : '"other"';
    assert.equal(buildOrderBy(driver, escaped, { key: name.toUpperCase(), direction: "desc" }), `${quoted} DESC, ${other} ASC`);
    assert.deepEqual(buildStableSort(escaped, { key: name.toUpperCase(), direction: "desc" }), [
      { key: name, direction: "desc" }, { key: "other", direction: "asc" },
    ]);
  }
});

test("无主键保留既有降级行为，SQL Server 空排序沿用兼容占位", () => {
  const noKeys = [column("value")];
  assert.deepEqual(buildStableSort(noKeys, null), []);
  assert.equal(buildPageSql("sqlite", '"items"', "", 1, 20, noKeys, null), 'SELECT * FROM "items" LIMIT 20 OFFSET 0');
  assert.equal(buildPageSql("sqlserver", "[items]", "", 1, 20, noKeys, null),
    "SELECT [__sqlconnect_page].[value] FROM (SELECT *, ROW_NUMBER() OVER (ORDER BY (SELECT 1)) AS [__sqlconnect_row_number] " +
    "FROM [items]) AS [__sqlconnect_page] WHERE [__sqlconnect_page].[__sqlconnect_row_number] BETWEEN 1 AND 20 " +
    "ORDER BY [__sqlconnect_page].[__sqlconnect_row_number]");
  assert.equal(buildPageSql("sqlite", '"items"', "", 2, 20, noKeys, { key: "value", direction: "desc" }),
    'SELECT * FROM "items" ORDER BY "value" DESC LIMIT 20 OFFSET 20');
});
