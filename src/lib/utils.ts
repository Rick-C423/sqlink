import { clsx, type ClassValue } from "clsx";
import { twMerge } from "tailwind-merge";

export function cn(...inputs: ClassValue[]) {
  return twMerge(clsx(inputs));
}

/**
 * 从 SQL 中提取单表查询的表名（用于数据网格编辑）。
 * 只认"单表 SELECT"：含 JOIN / 子查询 / 多表时返回 null（禁止编辑）。
 */
export function extractSingleTable(sql: string): string | null {
  if (/\bjoin\b/i.test(sql)) return null;
  if (/\b(insert|update|delete|create|drop|alter)\b/i.test(sql)) return null;
  const fromCount = (sql.match(/\bfrom\b/gi) || []).length;
  if (fromCount !== 1) return null; // 子查询会有多个 FROM
  const m = sql.match(/^\s*select[\s\S]+?\bfrom\s+(?:[`"\[]?[\w]+[`"\]]?\.)?[`"\[]?([A-Za-z0-9_]+)[`"\]]?/i);
  return m ? m[1] : null;
}

/** JS 值转 SQL 字面量（字符串转义单引号；输入大写 NULL 视为 NULL） */
export function sqlLiteral(v: string | number | boolean | null | undefined): string {
  if (v === null || v === undefined) return "NULL";
  if (typeof v === "number") return String(v);
  if (typeof v === "boolean") return v ? "1" : "0";
  if (v.trim().toUpperCase() === "NULL") return "NULL";
  return `'${v.replace(/'/g, "''")}'`;
}

/** 标识符引用：MySQL 反引号，PostgreSQL/SQLite 双引号，SQL Server 方括号 */
export function quoteIdent(name: string, driverType: string): string {
  if (driverType === "postgres" || driverType === "postgresql" || driverType === "sqlite") {
    return `"${name.replace(/"/g, '""')}"`;
  }
  if (driverType === "sqlserver" || driverType === "mssql") {
    return `[${name.replace(/]/g, "]]")}]`;
  }
  return `\`${name.replace(/`/g, "``")}\``;
}

/** 结构编辑器"类型"列的下拉建议（仍可自由输入，datalist 只提供候选） */
export function commonDataTypes(driverType: string): string[] {
  if (driverType === "postgres" || driverType === "postgresql") {
    return [
      "integer", "bigint", "smallint", "serial", "bigserial",
      "numeric(10,2)", "real", "double precision", "boolean",
      "varchar(255)", "char(32)", "text",
      "date", "time", "timestamp", "timestamptz",
      "json", "jsonb", "uuid", "bytea",
    ];
  }
  if (driverType === "sqlite") {
    return [
      "INTEGER", "REAL", "TEXT", "BLOB", "NUMERIC",
      "VARCHAR(255)", "BOOLEAN", "DATE", "DATETIME",
    ];
  }
  if (driverType === "sqlserver" || driverType === "mssql") {
    return [
      "INT", "BIGINT", "SMALLINT", "TINYINT", "BIT",
      "DECIMAL(18,2)", "FLOAT", "REAL", "MONEY",
      "NVARCHAR(255)", "NCHAR(32)", "VARCHAR(255)", "CHAR(32)", "NTEXT", "TEXT",
      "DATE", "TIME", "DATETIME", "DATETIME2", "DATETIMEOFFSET",
      "UNIQUEIDENTIFIER", "VARBINARY(MAX)", "XML",
    ];
  }
  return [
    "INT", "BIGINT", "SMALLINT", "TINYINT",
    "DECIMAL(10,2)", "FLOAT", "DOUBLE", "BIT",
    "CHAR(32)", "VARCHAR(255)", "TEXT", "MEDIUMTEXT", "LONGTEXT",
    "DATE", "TIME", "DATETIME", "TIMESTAMP", "YEAR",
    "JSON", "BLOB",
  ];
}

/** 系统数据库：默认在连接树中隐藏，且禁用新建表/导入/删除等危险操作 */
const MYSQL_SYSTEM_DBS = ["information_schema", "mysql", "performance_schema", "sys"];
const PG_SYSTEM_DBS = ["postgres", "template0", "template1"];
const MSSQL_SYSTEM_DBS = ["master", "model", "msdb", "tempdb"];

export function isSystemDatabase(name: string, driverType: string): boolean {
  const n = name.toLowerCase();
  if (driverType === "postgres" || driverType === "postgresql") {
    return PG_SYSTEM_DBS.includes(n);
  }
  if (driverType === "sqlserver" || driverType === "mssql") {
    return MSSQL_SYSTEM_DBS.includes(n);
  }
  if (driverType === "sqlite") {
    return false; // SQLite 单文件库，无系统库概念
  }
  return MYSQL_SYSTEM_DBS.includes(n);
}
