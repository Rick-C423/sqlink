import type { EditContext } from "@/types/database";
import { quoteIdent } from "@/lib/utils";

interface Token {
  kind: "word" | "identifier" | "string" | "symbol";
  value: string;
  start: number;
  end: number;
}

export interface TableReference {
  parts: string[];
  quoted: boolean[];
  start: number;
  end: number;
}

/** 只识别可确定来源的单表 SELECT；字符串与注释不参与语法判断。 */
export function extractSingleTable(sql: string, driverType?: string): TableReference | null {
  const tokens: Token[] = [];
  let i = 0;
  while (i < sql.length) {
    const start = i;
    const ch = sql[i];
    if (/\s/.test(ch)) { i++; continue; }
    if (sql.startsWith("--", i) || ch === "#") {
      while (i < sql.length && sql[i] !== "\n") i++;
      continue;
    }
    if (sql.startsWith("/*", i)) {
      // MySQL/MariaDB 可执行注释可能藏有 JOIN 等语法，不能当作普通注释忽略。
      if (sql.startsWith("/*!", i) || /^\/\*[mM]!/.test(sql.slice(i, i + 4))) return null;
      i += 2;
      let depth = 1;
      while (i < sql.length && depth > 0) {
        if (sql.startsWith("/*", i)) { depth++; i += 2; }
        else if (sql.startsWith("*/", i)) { depth--; i += 2; }
        else i++;
      }
      if (depth) return null;
      continue;
    }
    if (ch === "'" || ch === '"' || ch === "`" || ch === "[") {
      const closing = ch === "[" ? "]" : ch;
      let value = "";
      let closed = false;
      i++;
      while (i < sql.length) {
        if (sql[i] === closing) {
          if (sql[i + 1] === closing) { value += closing; i += 2; continue; }
          i++;
          closed = true;
          break;
        }
        if (ch === "'" && sql[i] === "\\") {
          value += sql.slice(i, i + 2);
          i += 2;
        } else value += sql[i++];
      }
      if (!closed) return null;
      tokens.push({ kind: ch === "'" ? "string" : "identifier", value, start, end: i });
      continue;
    }
    if (/[\p{L}\p{N}_$]/u.test(ch)) {
      while (i < sql.length && /[\p{L}\p{N}_$]/u.test(sql[i])) i++;
      tokens.push({ kind: "word", value: sql.slice(start, i), start, end: i });
    } else {
      tokens.push({ kind: "symbol", value: ch, start, end: ++i });
    }
  }

  const isWord = (token: Token | undefined, word: string) =>
    token?.kind === "word" && token.value.toUpperCase() === word;
  const identifier = (token: Token | undefined) => {
    if (!token) return false;
    if (token.kind === "word") {
      if (/^\d+$/.test(token.value)) return false;
      if (driverType === "postgres" || driverType === "postgresql") return /^[\p{L}_]/u.test(token.value);
      return true;
    }
    if (token.kind !== "identifier") return false;
    const quote = sql[token.start];
    // 不把其他方言的引用方式重写成合法 SQL；无法确认的形式仍原样查询且只读。
    if (driverType === "postgres" || driverType === "postgresql") return quote === '"';
    if (driverType === "mysql" || driverType === "mariadb") return quote === "`";
    if (driverType === "sqlserver" || driverType === "mssql") return quote === "[" || quote === '"';
    return true;
  };
  if (!isWord(tokens[0], "SELECT")) return null;
  const words = tokens.filter((token) => token.kind === "word").map((token) => token.value.toUpperCase());
  if (words.filter((word) => word === "SELECT").length !== 1 ||
      words.filter((word) => word === "FROM").length !== 1 ||
      words.some((word) => ["JOIN", "APPLY", "UNION", "INTERSECT", "EXCEPT", "INTO", "INSERT", "UPDATE", "DELETE", "CREATE", "DROP", "ALTER"].includes(word))) {
    return null;
  }
  if (tokens.some((token, index) => token.value === ";" && token.kind === "symbol" && index !== tokens.length - 1)) return null;
  let index = tokens.findIndex((token) => isWord(token, "FROM")) + 1;
  if (isWord(tokens[index], "ONLY") || isWord(tokens[index], "LATERAL")) return null;
  if (!identifier(tokens[index])) return null;
  const start = tokens[index].start;
  const parts = [tokens[index].value];
  const quoted = [tokens[index++].kind === "identifier"];
  while (tokens[index]?.kind === "symbol" && tokens[index].value === ".") {
    index++;
    if (!identifier(tokens[index])) return null;
    parts.push(tokens[index].value);
    quoted.push(tokens[index++].kind === "identifier");
  }
  if (parts.length > 3 || parts.some((part) => !part)) return null;
  const end = tokens[index - 1].end;
  const clauses = ["WHERE", "GROUP", "HAVING", "ORDER", "LIMIT", "OFFSET", "FETCH", "FOR"];
  if (isWord(tokens[index], "AS")) {
    index++;
    if (!identifier(tokens[index])) return null;
    index++;
  } else if (identifier(tokens[index]) && !clauses.some((word) => isWord(tokens[index], word))) {
    index++;
  }
  const next = tokens[index];
  if (next && !(next.kind === "symbol" && next.value === ";") && !clauses.some((word) => isWord(next, word))) return null;
  return { parts, quoted, start, end };
}

/** 标识符逐段引用；名称自身含点号时仍作为一个名称保留。 */
export function quoteTable(context: Pick<EditContext, "database" | "schema" | "table" | "driverType">): string {
  return tableParts(context).map((part) => quoteIdent(part, context.driverType)).join(".");
}

export function tableParts(context: Pick<EditContext, "database" | "schema" | "table" | "driverType">): string[] {
  const { database, schema, table, driverType } = context;
  if (driverType === "mysql" || driverType === "mariadb") return [database, table];
  if (driverType === "sqlserver" || driverType === "mssql") return schema ? [database, schema, table] : [table];
  return schema ? [schema, table] : [table];
}
