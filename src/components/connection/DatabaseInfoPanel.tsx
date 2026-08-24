import { useCallback, useEffect, useState } from "react";
import { tauriService } from "@/services/tauriService";
import { useConnectionStore } from "@/stores/connectionStore";
import { QueryResult } from "@/types/database";
import { useT } from "@/i18n";
import { Button } from "@/components/ui";
import { Database, RefreshCw, ChevronDown, ChevronUp, Loader2 } from "lucide-react";

interface DbInfo {
  charset?: string;
  collation?: string;
  tableCount?: number;
  sizeBytes?: number;
}

/** 各驱动一条（或两条）查询取库级信息；在目标库上下文执行 */
function infoQueries(driver: string, db: string): string[] {
  const esc = db.replace(/'/g, "''");
  if (driver === "mysql" || driver === "mariadb") {
    return [
      `SELECT s.DEFAULT_CHARACTER_SET_NAME AS charset, s.DEFAULT_COLLATION_NAME AS collation, ` +
        `COUNT(t.table_name) AS table_count, IFNULL(SUM(t.data_length + t.index_length), 0) AS size_bytes ` +
        `FROM information_schema.SCHEMATA s ` +
        `LEFT JOIN information_schema.TABLES t ON t.table_schema = s.SCHEMA_NAME ` +
        `WHERE s.SCHEMA_NAME = '${esc}' ` +
        `GROUP BY s.DEFAULT_CHARACTER_SET_NAME, s.DEFAULT_COLLATION_NAME`,
    ];
  }
  if (driver === "postgres" || driver === "postgresql") {
    return [
      `SELECT pg_encoding_to_char(encoding) AS charset, datcollate AS collation, ` +
        `pg_database_size(datname) AS size_bytes, ` +
        `(SELECT COUNT(*) FROM information_schema.tables ` +
        ` WHERE table_schema NOT IN ('pg_catalog', 'information_schema')) AS table_count ` +
        `FROM pg_database WHERE datname = current_database()`,
    ];
  }
  if (driver === "sqlserver" || driver === "mssql") {
    return [
      `SELECT CONVERT(varchar(128), DATABASEPROPERTYEX(DB_NAME(), 'Collation')) AS collation, ` +
        `(SELECT COUNT(*) FROM sys.tables) AS table_count, ` +
        `(SELECT SUM(size) * 8 * 1024 FROM sys.database_files) AS size_bytes`,
    ];
  }
  // sqlite
  return [
    `SELECT (SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%') AS table_count, ` +
      `(SELECT page_count FROM pragma_page_count) * (SELECT page_size FROM pragma_page_size) AS size_bytes`,
    `PRAGMA encoding`,
  ];
}

function pickString(r: QueryResult, name: string): string | undefined {
  const i = r.columns.findIndex((c) => c.name.toLowerCase() === name);
  const v = i >= 0 ? r.rows[0]?.values[i] : undefined;
  return v !== undefined && v !== null ? String(v) : undefined;
}

function pickNumber(r: QueryResult, name: string): number | undefined {
  const i = r.columns.findIndex((c) => c.name.toLowerCase() === name);
  const v = i >= 0 ? r.rows[0]?.values[i] : undefined;
  const n = Number(v);
  return v !== undefined && v !== null && !Number.isNaN(n) ? n : undefined;
}

function formatBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  const units = ["KB", "MB", "GB", "TB"];
  let v = n / 1024;
  let u = 0;
  while (v >= 1024 && u < units.length - 1) {
    v /= 1024;
    u++;
  }
  return `${v >= 100 ? Math.round(v) : v.toFixed(1)} ${units[u]}`;
}

/** 侧栏底部：当前选中数据库的基本信息（字符集/排序规则/表数量/大小），可折叠 */
export function DatabaseInfoPanel({ connectionId, database }: { connectionId: string; database: string }) {
  const t = useT();
  const driver = useConnectionStore(
    (s) => s.saved.find((c) => c.id === connectionId)?.driverType ?? "mysql"
  );
  const [info, setInfo] = useState<DbInfo | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [collapsed, setCollapsed] = useState(
    () => localStorage.getItem("sqlink.dbInfoCollapsed") === "1"
  );

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const queries = infoQueries(driver, database);
      const main = await tauriService.executeQuery(connectionId, queries[0], database);
      if (main.error) throw new Error(main.error);
      const next: DbInfo = {
        charset: pickString(main, "charset"),
        collation: pickString(main, "collation"),
        tableCount: pickNumber(main, "table_count"),
        sizeBytes: pickNumber(main, "size_bytes"),
      };
      if (queries[1]) {
        // SQLite：编码单独用 PRAGMA 取
        const enc = await tauriService.executeQuery(connectionId, queries[1], database);
        if (!enc.error) next.charset = pickString(enc, "encoding");
      }
      setInfo(next);
    } catch (err) {
      setError(String(err));
      setInfo(null);
    } finally {
      setLoading(false);
    }
  }, [connectionId, database, driver]);

  useEffect(() => {
    if (!collapsed) load();
  }, [load, collapsed]);

  const toggleCollapsed = () => {
    setCollapsed((c) => {
      localStorage.setItem("sqlink.dbInfoCollapsed", c ? "0" : "1");
      return !c;
    });
  };

  const isPgLike = driver === "postgres" || driver === "postgresql" || driver === "sqlite";

  return (
    <div className="flex-shrink-0 border-t border-r bg-muted/20">
      {/* 标题栏：数据库名 + 刷新 + 折叠 */}
      <div
        className="flex items-center gap-1.5 px-2 py-1 cursor-pointer select-none"
        onClick={toggleCollapsed}
        title={collapsed ? t("dbinfo.expand") : t("dbinfo.collapse")}
      >
        <Database className="h-3.5 w-3.5 text-blue-500 flex-shrink-0" />
        <span className="flex-1 min-w-0 truncate text-xs font-medium">{database}</span>
        {!collapsed && (
          <Button
            variant="ghost"
            size="icon"
            className="h-5 w-5 flex-shrink-0"
            title={t("common.refresh")}
            disabled={loading}
            onClick={(e) => {
              e.stopPropagation();
              load();
            }}
          >
            {loading ? (
              <Loader2 className="h-3 w-3 animate-spin" />
            ) : (
              <RefreshCw className="h-3 w-3" />
            )}
          </Button>
        )}
        {collapsed ? (
          <ChevronUp className="h-3.5 w-3.5 text-muted-foreground flex-shrink-0" />
        ) : (
          <ChevronDown className="h-3.5 w-3.5 text-muted-foreground flex-shrink-0" />
        )}
      </div>

      {!collapsed && (
        <div className="px-2 pb-1.5 text-xs">
          {error && <div className="text-red-600 truncate" title={error}>{t("common.loadFailed", { error })}</div>}
          {!error && !info && loading && (
            <div className="text-muted-foreground">{t("common.loading")}</div>
          )}
          {!error && info && (
            <div className="space-y-0.5">
              {info.charset && (
                <div className="flex justify-between gap-2">
                  <span className="text-muted-foreground flex-shrink-0">
                    {isPgLike ? t("dbinfo.encoding") : t("dbinfo.charset")}
                  </span>
                  <span className="truncate" title={info.charset}>{info.charset}</span>
                </div>
              )}
              {info.collation && (
                <div className="flex justify-between gap-2">
                  <span className="text-muted-foreground flex-shrink-0">{t("dbinfo.collation")}</span>
                  <span className="truncate" title={info.collation}>{info.collation}</span>
                </div>
              )}
              {info.tableCount !== undefined && (
                <div className="flex justify-between gap-2">
                  <span className="text-muted-foreground flex-shrink-0">{t("dbinfo.tables")}</span>
                  <span>{info.tableCount.toLocaleString()}</span>
                </div>
              )}
              {info.sizeBytes !== undefined && (
                <div className="flex justify-between gap-2">
                  <span className="text-muted-foreground flex-shrink-0">{t("dbinfo.size")}</span>
                  <span>{formatBytes(info.sizeBytes)}</span>
                </div>
              )}
            </div>
          )}
        </div>
      )}
    </div>
  );
}
