import { quoteIdent, sqlLiteral } from "./utils";
import { quoteTable, tableParts } from "./sqlTable";

export interface DdlTableIdentity { database: string; schema?: string | null; table: string }

/** 名称中的逗号通过 SQL 标识符引用保留；不把引用内的逗号当分隔符。 */
export function parseIdentifierList(input: string): string[] {
  const result: string[] = [];
  let offset = 0;
  while (offset < input.length) {
    while (/\s/.test(input[offset] ?? "") && offset < input.length) offset++;
    const opening = input[offset];
    let name = "";
    if (opening === '"' || opening === "`" || opening === "[") {
      const closing = opening === "[" ? "]" : opening;
      offset++;
      let closed = false;
      while (offset < input.length) {
        if (input[offset] === closing) {
          if (input[offset + 1] === closing) { name += closing; offset += 2; continue; }
          offset++; closed = true; break;
        }
        name += input[offset++];
      }
      if (!closed) throw new Error("列名引用未闭合，请使用完整的引用标识符。");
      while (offset < input.length && /\s/.test(input[offset])) offset++;
      if (offset < input.length && input[offset] !== ",") throw new Error("列名引用后只能使用逗号分隔。");
    } else {
      while (offset < input.length && input[offset] !== ",") name += input[offset++];
      name = name.trim();
      if (/[\[\]`"']/.test(name)) throw new Error("复杂列名请使用完整的引用标识符。");
    }
    if (!name) throw new Error("列名列表包含空名称。");
    result.push(name);
    if (offset < input.length) {
      offset++;
      if (!input.slice(offset).trim()) throw new Error("列名列表不能以逗号结尾。");
    }
  }
  return result;
}

function referenceTable(fk: FkDraft, driver: string): string {
  const context = { database: fk.refDatabase ?? "", schema: fk.refSchema, table: fk.refTable.trim(), driverType: driver };
  if (context.database && !context.schema && (driver === "mssql" || driver === "sqlserver")) {
    throw new Error("指定引用数据库时，请同时填写引用 schema。");
  }
  // 未填写引用库时保留当前库语义；点号始终属于名称自身。
  if (!context.database && (driver === "mysql" || driver === "mariadb")) return quoteIdent(context.table, driver);
  if (!context.database && (driver === "mssql" || driver === "sqlserver")) {
    return [context.schema, context.table].filter((part): part is string => Boolean(part)).map((part) => quoteIdent(part, driver)).join(".");
  }
  return quoteTable(context);
}

/**
 * 默认值三态（参考 Beekeeper）：
 * - "null"  —— 无默认值 / DEFAULT NULL（MySQL 无法区分这两者，效果相同）
 * - "empty" —— DEFAULT ''（空字符串）
 * - "value" —— 自定义值（按 defaultExpr 规则生成）
 */
export type DefaultKind = "null" | "empty" | "value";

/** 列编辑草稿（位置与原始列一一对应，设计器暂不提供排序） */
export interface ColumnDraft {
  name: string;
  dataType: string;
  nullable: boolean;
  defaultKind: DefaultKind;
  /** defaultKind = "value" 时生效 */
  defaultValue: string;
  isPrimary?: boolean;
  /** 仅 MySQL 可编辑 */
  autoIncrement?: boolean;
  /** 列注释（空字符串 = 无注释） */
  comment: string;
  isNew?: boolean;
  deleted?: boolean;
}

export interface IndexDraft {
  name: string;
  /** 逗号分隔的列名；名称含逗号时必须引用标识符。 */
  columns: string;
  isUnique: boolean;
  isPrimary?: boolean;
  isNew?: boolean;
  deleted?: boolean;
  /** 已有表达式、包含列和部分索引仅展示，按原名精确删除。 */
  details?: string;
}

/** 外键编辑草稿（已有外键只支持删除，修改 = 删旧建新） */
export interface FkDraft {
  name: string;
  /** 逗号分隔的本表列名；名称含逗号时必须引用标识符。 */
  columns: string;
  refTable: string;
  refSchema?: string;
  refDatabase?: string;
  /** 逗号分隔的引用列名；名称含逗号时必须引用标识符。 */
  refColumns: string;
  /** 空 = 不生成子句 */
  onDelete: string;
  onUpdate: string;
  isNew?: boolean;
  deleted?: boolean;
}

/**
 * 自定义默认值 → SQL 表达式。
 * 纯数字 / 已带引号 / 函数调用或关键字（如 now()、CURRENT_TIMESTAMP）原样输出，
 * 其余按字符串字面量加引号。
 */
function defaultExpr(v: string): string {
  const t = v.trim();
  if (/^-?\d+(\.\d+)?$/.test(t)) return t;
  if (/^'.*'$/.test(t) || /^".*"$/.test(t)) return t;
  if (/^[A-Za-z_][A-Za-z0-9_]*(\(.*\))?$/.test(t)) return t;
  return sqlLiteral(t);
}

function columnDef(d: ColumnDraft, driver: string): string {
  const isMySql = driver === "mysql";
  const parts = [`${quoteIdent(d.name.trim(), driver)} ${d.dataType.trim() || "TEXT"}`];
  parts.push(d.nullable ? "NULL" : "NOT NULL");
  if (d.defaultKind === "empty") {
    parts.push("DEFAULT ''");
  } else if (d.defaultKind === "value" && d.defaultValue.trim() !== "") {
    parts.push(`DEFAULT ${defaultExpr(d.defaultValue)}`);
  }
  if (isMySql && d.autoIncrement) {
    parts.push("AUTO_INCREMENT");
  }
  // MySQL 的列注释跟在列定义里；不指定则 MODIFY 后注释被清空（完整定义替换语义）
  if (isMySql && d.comment.trim() !== "") {
    parts.push(`COMMENT ${sqlLiteral(d.comment.trim())}`);
  }
  return parts.join(" ");
}

/**
 * 对比原始结构与草稿，生成 ALTER/CREATE/DROP 语句序列（按驱动区分方言）。
 * 顺序：先 DROP 主键 → 列变更（含注释） → ADD 主键 → 索引变更 → 外键变更。
 * 注意：MySQL 的 DDL 不在事务内（隐式提交），PG 的 DDL 可回滚。
 */
export function buildAlterStatements(
  driver: string,
  table: string | DdlTableIdentity,
  origCols: ColumnDraft[],
  cols: ColumnDraft[],
  origIdxs: IndexDraft[],
  idxs: IndexDraft[],
  origFks: FkDraft[] = [],
  fks: FkDraft[] = []
): string[] {
  const identity = typeof table === "string" ? undefined : { ...table, driverType: driver };
  const tableName = typeof table === "string" ? table : table.table;
  const qt = identity ? quoteTable(identity) : quoteIdent(tableName, driver);
  const qualifiedIndex = (name: string) => identity?.schema
    ? `${quoteIdent(identity.schema, driver)}.${quoteIdent(name, driver)}` : quoteIdent(name, driver);
  const isPg = driver === "postgres" || driver === "postgresql";
  const isSqlite = driver === "sqlite";
  const isMssql = driver === "sqlserver" || driver === "mssql";
  const stmts: string[] = [];

  // ---- 主键（先算差异，DROP 放最前、ADD 放列变更之后） ----
  const origPk = origCols.filter((c) => c.isPrimary).map((c) => c.name);
  const newPk = cols
    .filter((c) => !c.deleted && c.isPrimary && c.name.trim())
    .map((c) => c.name.trim());
  const pkChanged =
    origPk.length !== newPk.length ||
    JSON.stringify([...origPk].sort()) !== JSON.stringify([...newPk].sort());

  if (pkChanged && origPk.length > 0) {
    if (isSqlite) {
      // SQLite 不支持删除主键约束，需重建表，跳过
    } else if (isPg || isMssql) {
      const pkName = origIdxs.find((i) => i.isPrimary)?.name ?? `${tableName}_pkey`;
      stmts.push(`ALTER TABLE ${qt} DROP CONSTRAINT ${quoteIdent(pkName, driver)};`);
    } else {
      stmts.push(`ALTER TABLE ${qt} DROP PRIMARY KEY;`);
    }
  }

  // ---- 列 ----
  cols.forEach((c, i) => {
    if (c.isNew) {
      if (!c.deleted && c.name.trim()) {
        // T-SQL 的 ADD 不带 COLUMN 关键字
        stmts.push(
          isMssql
            ? `ALTER TABLE ${qt} ADD ${columnDef(c, driver)};`
            : `ALTER TABLE ${qt} ADD COLUMN ${columnDef(c, driver)};`
        );
        // PG 的列注释是独立语句（MySQL 已在列定义里带 COMMENT）
        if (isPg && c.comment.trim() !== "") {
          stmts.push(
            `COMMENT ON COLUMN ${qt}.${quoteIdent(c.name.trim(), driver)} IS ${sqlLiteral(c.comment.trim())};`
          );
        }
      }
      return;
    }

    const orig = origCols[i];
    if (!orig) return;

    if (c.deleted) {
      stmts.push(`ALTER TABLE ${qt} DROP COLUMN ${quoteIdent(orig.name, driver)};`);
      return;
    }

    // 重命名（先改名，后续语句用新名）
    let currentName = orig.name;
    if (c.name.trim() && c.name.trim() !== orig.name) {
      stmts.push(
        isMssql
          ? `EXEC sp_rename N'${`${identity ? tableParts(identity).map((part) => quoteIdent(part, driver)).join(".") : quoteIdent(tableName, driver)}.${quoteIdent(orig.name, driver)}`.replace(/'/g, "''")}', N'${c.name.trim().replace(/'/g, "''")}', 'COLUMN';`
          : `ALTER TABLE ${qt} RENAME COLUMN ${quoteIdent(orig.name, driver)} TO ${quoteIdent(c.name.trim(), driver)};`
      );
      currentName = c.name.trim();
    }

    const typeChanged = c.dataType.trim() !== orig.dataType;
    const nullChanged = c.nullable !== orig.nullable;
    const defChanged =
      c.defaultKind !== orig.defaultKind ||
      (c.defaultKind === "value" && c.defaultValue.trim() !== orig.defaultValue.trim());
    const autoIncChanged = !isPg && Boolean(c.autoIncrement) !== Boolean(orig.autoIncrement);
    const commentChanged = c.comment !== orig.comment;

    if (isSqlite) {
      // SQLite 无法修改已有列的类型/可空/默认值（需重建表），跳过
    } else if (isMssql) {
      // T-SQL：ALTER COLUMN 一次给全 类型 + NULL/NOT NULL；默认值改动需操作约束，跳过
      if (typeChanged || nullChanged) {
        stmts.push(
          `ALTER TABLE ${qt} ALTER COLUMN ${quoteIdent(currentName, driver)} ${c.dataType.trim()} ${c.nullable ? "NULL" : "NOT NULL"};`
        );
      }
    } else if (isPg) {
      if (typeChanged) {
        stmts.push(
          `ALTER TABLE ${qt} ALTER COLUMN ${quoteIdent(currentName, driver)} TYPE ${c.dataType.trim()};`
        );
      }
      if (nullChanged) {
        stmts.push(
          `ALTER TABLE ${qt} ALTER COLUMN ${quoteIdent(currentName, driver)} ${c.nullable ? "DROP" : "SET"} NOT NULL;`
        );
      }
      if (defChanged) {
        if (c.defaultKind === "null") {
          stmts.push(`ALTER TABLE ${qt} ALTER COLUMN ${quoteIdent(currentName, driver)} DROP DEFAULT;`);
        } else {
          const expr = c.defaultKind === "empty" ? "''" : defaultExpr(c.defaultValue);
          stmts.push(`ALTER TABLE ${qt} ALTER COLUMN ${quoteIdent(currentName, driver)} SET DEFAULT ${expr};`);
        }
      }
      if (commentChanged) {
        stmts.push(
          c.comment.trim() === ""
            ? `COMMENT ON COLUMN ${qt}.${quoteIdent(currentName, driver)} IS NULL;`
            : `COMMENT ON COLUMN ${qt}.${quoteIdent(currentName, driver)} IS ${sqlLiteral(c.comment.trim())};`
        );
      }
    } else {
      // MySQL: MODIFY COLUMN 一次覆盖类型/可空/默认值/AUTO_INCREMENT/注释
      if (typeChanged || nullChanged || defChanged || autoIncChanged || commentChanged) {
        stmts.push(
          `ALTER TABLE ${qt} MODIFY COLUMN ${columnDef({ ...c, name: currentName }, driver)};`
        );
      }
    }
  });

  // ---- 主键 ADD（在列重命名/新增之后，使用新列名） ----
  if (pkChanged && newPk.length > 0 && !isSqlite) {
    const colList = newPk.map((n) => quoteIdent(n, driver)).join(", ");
    stmts.push(
      isMssql
        ? `ALTER TABLE ${qt} ADD CONSTRAINT ${quoteIdent(`${tableName}_pk`, driver)} PRIMARY KEY (${colList});`
        : `ALTER TABLE ${qt} ADD PRIMARY KEY (${colList});`
    );
  }

  // ---- 索引 ----
  for (const idx of idxs) {
    if (idx.isNew) {
      if (!idx.deleted && idx.name.trim() && idx.columns.trim()) {
        const colList = parseIdentifierList(idx.columns).map((name) => quoteIdent(name, driver)).join(", ");
        stmts.push(
          isPg || isSqlite || isMssql
            ? `CREATE ${idx.isUnique ? "UNIQUE " : ""}INDEX ${isSqlite ? qualifiedIndex(idx.name.trim()) : quoteIdent(idx.name.trim(), driver)} ON ${isSqlite ? quoteIdent(tableName, driver) : qt} (${colList});`
            : `ALTER TABLE ${qt} ADD ${idx.isUnique ? "UNIQUE " : ""}INDEX ${quoteIdent(idx.name.trim(), driver)} (${colList});`
        );
      }
    } else if (idx.deleted && !idx.isPrimary) {
      stmts.push(
        isMssql
          ? `DROP INDEX ${quoteIdent(idx.name, driver)} ON ${qt};`
          : isPg || isSqlite
            ? `DROP INDEX ${qualifiedIndex(idx.name)};`
            : `ALTER TABLE ${qt} DROP INDEX ${quoteIdent(idx.name, driver)};`
      );
    }
  }

  // ---- 外键（已有外键只支持删除；修改 = 删旧建新） ----
  const parseCols = (s: string) =>
    parseIdentifierList(s)
      .map((x) => quoteIdent(x, driver))
      .join(", ");

  for (const fk of fks) {
    if (fk.isNew) {
      // SQLite 不支持事后 ADD 外键（只能建表时定义），跳过
      if (isSqlite) continue;
      if (
        !fk.deleted &&
        fk.name.trim() &&
        fk.columns.trim() &&
        fk.refTable.trim() &&
        fk.refColumns.trim()
      ) {
        let s = `ALTER TABLE ${qt} ADD CONSTRAINT ${quoteIdent(fk.name.trim(), driver)} ` +
          `FOREIGN KEY (${parseCols(fk.columns)}) ` +
          `REFERENCES ${referenceTable(fk, driver)} (${parseCols(fk.refColumns)})`;
        if (fk.onDelete) s += ` ON DELETE ${fk.onDelete}`;
        if (fk.onUpdate) s += ` ON UPDATE ${fk.onUpdate}`;
        stmts.push(s + ";");
      }
    } else if (fk.deleted) {
      if (isSqlite) continue; // SQLite 需重建表才能去外键
      stmts.push(
        isPg || isMssql
          ? `ALTER TABLE ${qt} DROP CONSTRAINT ${quoteIdent(fk.name, driver)};`
          : `ALTER TABLE ${qt} DROP FOREIGN KEY ${quoteIdent(fk.name, driver)};`
      );
    }
  }

  return stmts;
}

/**
 * 生成 CREATE TABLE 语句序列（新建表设计器用）。
 * - MySQL：索引/外键/表注释内联在 CREATE TABLE 里
 * - PG：非唯一索引与列/表注释是独立语句（跟在 CREATE TABLE 之后）
 * 调用方需先自行校验表名非空；没有有效列时返回空数组。
 */
export function buildCreateTableStatements(
  driver: string,
  table: string,
  cols: ColumnDraft[],
  idxs: IndexDraft[] = [],
  fks: FkDraft[] = [],
  tableComment = ""
): string[] {
  const t = table.trim();
  if (!t) return [];
  const qt = quoteIdent(t, driver);
  const isPg = driver === "postgres" || driver === "postgresql";
  // SQLite / SQL Server：索引独立 CREATE，无表注释语法 —— 与 PG 同样处理
  const separateIdx =
    isPg || driver === "sqlite" || driver === "sqlserver" || driver === "mssql";

  const liveCols = cols.filter((c) => !c.deleted && c.name.trim());
  if (liveCols.length === 0) return [];

  const defs: string[] = liveCols.map((c) => columnDef(c, driver));

  // 主键
  const pkCols = liveCols
    .filter((c) => c.isPrimary)
    .map((c) => quoteIdent(c.name.trim(), driver));
  if (pkCols.length > 0) {
    defs.push(`PRIMARY KEY (${pkCols.join(", ")})`);
  }

  const parseCols = (s: string) =>
    parseIdentifierList(s)
      .map((x) => quoteIdent(x, driver))
      .join(", ");

  // 索引（PG 非唯一索引只能建表后单独 CREATE INDEX）
  const postStmts: string[] = [];
  for (const idx of idxs) {
    if (idx.deleted || !idx.columns.trim()) continue;
    const colList = parseCols(idx.columns);
    if (!colList) continue;
    const name = idx.name.trim() || `${t}_${parseIdentifierList(idx.columns)[0]}_idx`;
    if (separateIdx) {
      if (idx.isUnique) {
        defs.push(`CONSTRAINT ${quoteIdent(name, driver)} UNIQUE (${colList})`);
      } else {
        postStmts.push(`CREATE INDEX ${quoteIdent(name, driver)} ON ${qt} (${colList});`);
      }
    } else {
      defs.push(`${idx.isUnique ? "UNIQUE " : ""}KEY ${quoteIdent(name, driver)} (${colList})`);
    }
  }

  // 外键（两种方言都内联）
  for (const fk of fks) {
    if (fk.deleted || !fk.columns.trim() || !fk.refTable.trim() || !fk.refColumns.trim()) continue;
    const name = fk.name.trim() || `${t}_${parseIdentifierList(fk.columns)[0]}_fk`;
    let s =
      `CONSTRAINT ${quoteIdent(name, driver)} FOREIGN KEY (${parseCols(fk.columns)}) ` +
      `REFERENCES ${referenceTable(fk, driver)} (${parseCols(fk.refColumns)})`;
    if (fk.onDelete) s += ` ON DELETE ${fk.onDelete}`;
    if (fk.onUpdate) s += ` ON UPDATE ${fk.onUpdate}`;
    defs.push(s);
  }

  let createSql = `CREATE TABLE ${qt} (\n  ${defs.join(",\n  ")}\n)`;
  if (!separateIdx && tableComment.trim() !== "") {
    createSql += ` COMMENT = ${sqlLiteral(tableComment.trim())}`;
  }
  const stmts = [createSql + ";"];

  // PG：表/列注释是独立语句
  if (isPg) {
    if (tableComment.trim() !== "") {
      stmts.push(`COMMENT ON TABLE ${qt} IS ${sqlLiteral(tableComment.trim())};`);
    }
    for (const c of liveCols) {
      if (c.comment.trim() !== "") {
        stmts.push(
          `COMMENT ON COLUMN ${qt}.${quoteIdent(c.name.trim(), driver)} IS ${sqlLiteral(c.comment.trim())};`
        );
      }
    }
  }

  stmts.push(...postStmts);
  return stmts;
}
