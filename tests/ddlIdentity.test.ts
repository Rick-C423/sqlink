import assert from "node:assert/strict";
import { test } from "node:test";
import { buildAlterStatements, parseIdentifierList, type ColumnDraft, type FkDraft, type IndexDraft } from "../src/lib/ddl";

const col: ColumnDraft = { name: "a,b", dataType: "text", nullable: true, defaultKind: "null", defaultValue: "", comment: "" };
const index: IndexDraft = { name: 'idx.a"b', columns: '"a,b"', isUnique: false };

test("ALTER 表身份逐段引用，含点号表名与 schema 同名对象不会混淆", () => {
  for (const schema of ["public", 'other"schema']) {
    assert.deepEqual(buildAlterStatements("postgres", { database: "db", schema, table: "a.b" }, [col], [{ ...col, deleted: true }], [], []),
      [`ALTER TABLE "${schema.replace(/"/g, '""')}"."a.b" DROP COLUMN "a,b";`]);
  }
  assert.equal(buildAlterStatements("mysql", { database: "db.x", table: "a.b" }, [col], [{ ...col, deleted: true }], [], [])[0],
    "ALTER TABLE `db.x`.`a.b` DROP COLUMN `a,b`;");
});

test("PG/SQLite DROP INDEX 固定 schema，复杂已有索引只按原名删除", () => {
  for (const driver of ["postgres", "sqlite"]) {
    const target = { database: "db", schema: "alt", table: "a.b" };
    const complex = { ...index, columns: 'lower("name") DESC', details: 'INCLUDE ("payload")\nWHERE active = 1' };
    assert.deepEqual(buildAlterStatements(driver, target, [col], [col], [complex], [complex]), []);
    assert.deepEqual(buildAlterStatements(driver, target, [col], [col], [complex], [{ ...complex, deleted: true }]),
      ['DROP INDEX "alt"."idx.a""b";']);
  }
  assert.equal(buildAlterStatements("sqlite", { database: "db", schema: "alt", table: "a.b" }, [], [], [], [{ ...index, isNew: true }])[0],
    'CREATE INDEX "alt"."idx.a""b" ON "a.b" ("a,b");');
});

test("MSSQL sp_rename 保留库/schema/表/列身份以及引号、括号与 NULL 名称", () => {
  assert.deepEqual(buildAlterStatements("mssql", { database: "db]x", schema: "s'x", table: "a.b" }, [col], [{ ...col, name: "NULL" }], [], []),
    ["EXEC sp_rename N'[db]]x].[s''x].[a.b].[a,b]', N'NULL', 'COLUMN';"]);
});

test("外键引用保留 schema 与点号表名，列列表支持引用内逗号", () => {
  const fk: FkDraft = { name: "fk", columns: '"a,b", plain', refSchema: 's"x', refTable: "target.table", refColumns: '"id,part", id', onDelete: "", onUpdate: "", isNew: true };
  assert.deepEqual(buildAlterStatements("postgres", { database: "db", schema: "src", table: "t" }, [], [], [], [], [], [fk]),
    ['ALTER TABLE "src"."t" ADD CONSTRAINT "fk" FOREIGN KEY ("a,b", "plain") REFERENCES "s""x"."target.table" ("id,part", "id");']);
  assert.deepEqual(parseIdentifierList('"a,b", [x]]y], `z``w`'), ["a,b", "x]y", "z`w"]);
  assert.throws(() => parseIdentifierList('"a,b'), /未闭合/);
  assert.throws(() => parseIdentifierList('"a"oops'), /引用后/);
  assert.throws(() => parseIdentifierList("a,,b"), /空名称/);
});

test("主键集合比较保留现有无序语义，同时区分名称内的逗号", () => {
  const original = ["a,b", "c", "a", "b,c"].map((name, i) => ({ ...col, name, isPrimary: i < 2 }));
  const changed = original.map((column, i) => ({ ...column, isPrimary: i >= 2 }));
  const primary = { name: "t_pkey", columns: '"a,b", "c"', isUnique: true, isPrimary: true };
  assert.deepEqual(buildAlterStatements("postgres", { database: "db", schema: "one", table: "t" }, original, changed, [primary], [primary]),
    ['ALTER TABLE "one"."t" DROP CONSTRAINT "t_pkey";', 'ALTER TABLE "one"."t" ADD PRIMARY KEY ("a", "b,c");']);
  // 仅重命名、交换主键列显示位置，最终列名集合不变时，沿用原先不重建主键的规则。
  const renamed = original.map((column, i) => ({ ...column, name: i === 0 ? "c" : i === 1 ? "a,b" : column.name }));
  const statements = buildAlterStatements("postgres", { database: "db", schema: "one", table: "t" }, original, renamed, [primary], [primary]);
  assert.equal(statements.some((statement) => /PRIMARY KEY|DROP CONSTRAINT/.test(statement)), false);
});
