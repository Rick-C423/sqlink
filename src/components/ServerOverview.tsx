import { useCallback, useEffect, useState } from "react";
import { tauriService } from "@/services/tauriService";
import { useConnectionStore } from "@/stores/connectionStore";
import { useWorkspaceStore } from "@/stores/workspaceStore";
import { DbUserInfo } from "@/types/database";
import { Button } from "@/components/ui";
import { isSystemDatabase } from "@/lib/utils";
import { Database, Loader2, RefreshCw, Server, ShieldCheck, UserRound } from "lucide-react";
import { useT } from "@/i18n";

interface ServerOverviewProps {
  connectionId: string;
}

/** 各驱动的版本查询 SQL（第一行第一列作为版本字符串） */
function versionSql(driverType: string): string {
  switch (driverType) {
    case "postgres":
    case "postgresql":
      return "SELECT version()";
    case "sqlserver":
    case "mssql":
      return "SELECT @@VERSION";
    case "sqlite":
      return "SELECT sqlite_version()";
    default:
      return "SELECT VERSION()";
  }
}

const DRIVER_LABELS: Record<string, string> = {
  mysql: "MySQL",
  mariadb: "MariaDB",
  postgres: "PostgreSQL",
  postgresql: "PostgreSQL",
  sqlite: "SQLite",
  sqlserver: "SQL Server",
  mssql: "SQL Server",
};

/**
 * 服务器概览：头部显示连接信息 + 版本；上半部分数据库列表，下半部分账号列表。
 * 用户区只读，账号管理入口跳到「用户与权限」页。
 */
export function ServerOverview({ connectionId }: ServerOverviewProps) {
  const t = useT();
  const cfg = useConnectionStore((s) => s.saved.find((c) => c.id === connectionId));
  const showSystemDatabases = useConnectionStore((s) => s.showSystemDatabases);
  const openDatabase = useWorkspaceStore((s) => s.openDatabase);
  const openUsers = useWorkspaceStore((s) => s.openUsers);

  const driverType = cfg?.driverType ?? "mysql";
  const isSqlite = driverType === "sqlite";

  const [version, setVersion] = useState<string | null>(null);
  const [databases, setDatabases] = useState<string[] | null>(null);
  const [users, setUsers] = useState<DbUserInfo[] | null>(null);
  const [usersError, setUsersError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    setUsersError(null);
    try {
      // 版本失败不阻塞其余信息（个别驱动/权限下可能拿不到）
      const versionP = tauriService
        .executeQuery(connectionId, versionSql(driverType))
        .then((r) => setVersion(String(r.rows[0]?.values[0] ?? "")))
        .catch(() => setVersion(null));
      const dbsP = tauriService.getDatabases(connectionId).then(setDatabases);
      const usersP = isSqlite
        ? Promise.resolve(setUsers([]))
        : tauriService
            .getUsers(connectionId)
            .then(setUsers)
            .catch((err) => {
              setUsers([]);
              setUsersError(String(err));
            });
      await Promise.all([versionP, dbsP, usersP]);
    } catch (err) {
      setError(String(err));
    } finally {
      setLoading(false);
    }
  }, [connectionId, driverType, isSqlite]);

  useEffect(() => {
    load();
  }, [load]);

  const visibleDbs = (databases ?? [])
    .filter((db) => showSystemDatabases || !isSystemDatabase(db, driverType))
    .sort(
      (a, b) =>
        Number(isSystemDatabase(a, driverType)) - Number(isSystemDatabase(b, driverType))
    );

  return (
    <div className="flex flex-col h-full min-h-0">
      {/* 头部：连接信息 + 版本 */}
      <div className="flex items-center gap-2 px-3 py-2 border-b bg-muted/20 flex-shrink-0">
        <Server className="h-4 w-4 text-orange-500" />
        <span className="text-sm font-medium">{cfg?.name ?? connectionId}</span>
        <span className="text-xs text-muted-foreground">
          {DRIVER_LABELS[driverType] ?? driverType}
          {isSqlite
            ? cfg?.database
              ? ` · ${cfg.database}`
              : ""
            : cfg
              ? ` · ${cfg.host}:${cfg.port}`
              : ""}
        </span>
        {version && (
          <span
            className="text-[10px] px-1.5 py-0.5 rounded bg-muted text-muted-foreground max-w-[40%] truncate"
            title={version}
          >
            {version}
          </span>
        )}
        <Button variant="ghost" size="sm" className="h-7 ml-auto" onClick={load} disabled={loading}>
          {loading ? <Loader2 className="h-3.5 w-3.5 animate-spin" /> : <RefreshCw className="h-3.5 w-3.5" />}
          <span className="ml-1 text-xs">{t("common.refresh")}</span>
        </Button>
      </div>

      {error && (
        <div className="px-3 py-1.5 bg-red-50 text-red-700 text-xs border-b border-red-200 flex-shrink-0">
          {t("common.loadFailed", { error })}
        </div>
      )}

      {/* 上半：数据库列表 */}
      <div className="flex-1 min-h-0 flex flex-col">
        <div className="px-3 py-1.5 border-b bg-muted/30 text-xs text-muted-foreground flex-shrink-0">
          {t("server.databases", { count: visibleDbs.length })}
        </div>
        <div className="flex-1 overflow-auto custom-scrollbar">
          {visibleDbs.length === 0 ? (
            <div className="px-3 py-6 text-center text-xs text-muted-foreground">
              {loading ? t("viewView.loading") : t("server.noDatabases")}
            </div>
          ) : (
            visibleDbs.map((db) => {
              const sysDb = isSystemDatabase(db, driverType);
              return (
                <div
                  key={db}
                  className="flex items-center gap-2 px-3 py-1.5 text-sm border-b border-border/50 hover:bg-muted/30 cursor-pointer"
                  title={t("overview.doubleClickOpen")}
                  onDoubleClick={() => openDatabase(connectionId, db)}
                >
                  <Database className="h-3.5 w-3.5 text-blue-500 flex-shrink-0" />
                  <span className={sysDb ? "text-gray-400" : ""}>{db}</span>
                  {sysDb && (
                    <span className="text-[10px] px-1 rounded bg-gray-200 dark:bg-gray-700 text-gray-500 dark:text-gray-400">
                      {t("tree.system")}
                    </span>
                  )}
                </div>
              );
            })
          )}
        </div>
      </div>

      {/* 下半：账号列表（只读，管理入口跳用户与权限页） */}
      <div className="flex-1 min-h-0 flex flex-col border-t">
        <div className="px-3 py-1.5 border-b bg-muted/30 text-xs text-muted-foreground flex items-center flex-shrink-0">
          <span>{t("server.users", { count: users?.length ?? 0 })}</span>
          {!isSqlite && (
            <button
              className="ml-auto flex items-center gap-1 text-blue-600 hover:underline"
              title={t("server.manageUsersTitle")}
              onClick={() => openUsers(connectionId)}
            >
              <ShieldCheck className="h-3 w-3" />
              {t("server.manageUsers")}
            </button>
          )}
        </div>
        <div className="flex-1 overflow-auto custom-scrollbar">
          {isSqlite ? (
            <div className="px-3 py-6 text-center text-xs text-muted-foreground">
              {t("server.usersUnsupported")}
            </div>
          ) : usersError ? (
            <div className="px-3 py-6 text-center text-xs text-muted-foreground">
              {t("server.usersFailed", { error: usersError })}
            </div>
          ) : !users || users.length === 0 ? (
            <div className="px-3 py-6 text-center text-xs text-muted-foreground">
              {loading ? t("viewView.loading") : t("server.noUsers")}
            </div>
          ) : (
            users.map((u) => (
              <div
                key={u.host ? `${u.name}@${u.host}` : u.name}
                className="flex items-center gap-2 px-3 py-1.5 text-sm border-b border-border/50"
              >
                <UserRound className="h-3.5 w-3.5 text-muted-foreground flex-shrink-0" />
                <span className="font-mono text-xs">
                  {u.host ? `${u.name}@${u.host}` : u.name}
                </span>
                {u.note && (
                  <span className="text-[10px] px-1 rounded bg-muted text-muted-foreground">
                    {u.note}
                  </span>
                )}
              </div>
            ))
          )}
        </div>
      </div>
    </div>
  );
}
