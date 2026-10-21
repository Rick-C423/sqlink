import { useCallback, useEffect, useRef, useState } from "react";
import { TableInfo } from "@/types/database";
import { tauriService } from "@/services/tauriService";
import { useWorkspaceStore } from "@/stores/workspaceStore";
import { Button } from "@/components/ui";
import { Loader2, RefreshCw, Table2 } from "lucide-react";
import { useT } from "@/i18n";
import { useConnectionStore } from "@/stores/connectionStore";

interface DatabaseOverviewProps {
  connectionId: string;
  database: string;
}

/** 数据库概览：该库下的所有表（名称/引擎/估算行数/注释），双击打开表视图 */
export function DatabaseOverview({ connectionId, database }: DatabaseOverviewProps) {
  const t = useT();
  const [tables, setTables] = useState<TableInfo[] | null>(null);
  const [tablesSource, setTablesSource] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const openTable = useWorkspaceStore((s) => s.openTable);
  const connectionEpoch = useConnectionStore((state) => state.connectionEpochs[connectionId] ?? 0);
  const sequence = useRef(0);
  const source = JSON.stringify([connectionId, connectionEpoch, database]);
  const sourceRef = useRef(source);
  sourceRef.current = source;
  const visibleTables = tablesSource === source ? tables : null;

  const load = useCallback(async () => {
    const request = ++sequence.current;
    const state = useConnectionStore.getState();
    if (sourceRef.current !== source || (state.connectionEpochs[connectionId] ?? 0) !== connectionEpoch ||
        (connectionEpoch > 0 && !state.liveIds.has(connectionId))) return;
    const isCurrent = () => sequence.current === request && sourceRef.current === source &&
      (useConnectionStore.getState().connectionEpochs[connectionId] ?? 0) === connectionEpoch;
    setLoading(true);
    setError(null);
    // 切换数据库或刷新时先清空旧结果，加载失败不能展示上一个库的表。
    setTables(null);
    setTablesSource(null);
    try {
      const list = await tauriService.getTables(connectionId, database);
      if (isCurrent()) { setTables(list); setTablesSource(source); }
    } catch (err) {
      if (isCurrent()) setError(String(err));
    } finally {
      if (isCurrent()) setLoading(false);
    }
  }, [connectionId, connectionEpoch, database, source]);

  useEffect(() => {
    setTables(null); setError(null); setLoading(false);
    void load();
    return () => { sequence.current++; };
  }, [load]);

  return (
    <div className="flex flex-col h-full">
      <div className="flex items-center gap-2 px-3 py-1.5 border-b bg-muted/20">
        <span className="text-sm font-medium">{t("overview.database", { name: database })}</span>
        <span className="text-xs text-muted-foreground">
          {visibleTables ? t("overview.tableCount", { count: visibleTables.length }) : ""}
        </span>
        <Button variant="ghost" size="sm" className="h-6 ml-auto" onClick={load} disabled={loading}>
          {loading ? <Loader2 className="h-3 w-3 animate-spin" /> : <RefreshCw className="h-3 w-3" />}
          <span className="ml-1 text-xs">{t("common.refresh")}</span>
        </Button>
      </div>

      {error && (
        <div className="px-3 py-2 bg-red-50 text-red-700 text-xs border-b border-red-200">
          {t("common.loadFailed", { error })}
        </div>
      )}

      <div className="flex-1 overflow-auto custom-scrollbar">
        {visibleTables && visibleTables.length === 0 && (
          <div className="flex items-center justify-center h-32 text-muted-foreground text-sm">
            {t("overview.noTables")}
          </div>
        )}
        {visibleTables && visibleTables.length > 0 && (
          <table className="w-full text-sm border-collapse table-fixed">
            <colgroup>
              {/* 表名占剩余空间；引擎/行数贴内容宽度，避免自动布局把空隙分到中间列 */}
              <col />
              <col className="w-28" />
              <col className="w-28" />
              <col className="w-[35%]" />
            </colgroup>
            <thead className="sticky top-0 z-10">
              <tr className="bg-gray-100 dark:bg-gray-800">
                <th className="px-3 py-1.5 text-left font-semibold text-xs border-b">{t("overview.colName")}</th>
                <th className="px-3 py-1.5 text-left font-semibold text-xs border-b">{t("overview.colEngine")}</th>
                <th className="px-3 py-1.5 text-right font-semibold text-xs border-b">{t("overview.colRows")}</th>
                <th className="px-3 py-1.5 text-left font-semibold text-xs border-b">{t("overview.colComment")}</th>
              </tr>
            </thead>
            <tbody>
              {visibleTables.map((tb) => (
                <tr
                  key={JSON.stringify([tb.schema ?? null, tb.name])}
                  className="row-hover cursor-pointer"
                  onDoubleClick={() => openTable(connectionId, database, tb.name, undefined, tb.schema)}
                  title={t("overview.doubleClickOpen")}
                >
                  <td className="px-3 py-1.5 border-b">
                    <span className="flex items-center gap-1.5 min-w-0">
                      <Table2 className="h-3.5 w-3.5 text-green-600 flex-shrink-0" />
                      <span className="truncate">{[tb.schema, tb.name].filter(Boolean).join(".")}</span>
                    </span>
                  </td>
                  <td className="px-3 py-1.5 border-b text-muted-foreground truncate">{tb.engine ?? "-"}</td>
                  <td className="px-3 py-1.5 border-b text-right text-muted-foreground">
                    {tb.rowCount !== undefined && tb.rowCount !== null
                      ? tb.rowCount.toLocaleString()
                      : "-"}
                  </td>
                  <td className="px-3 py-1.5 border-b text-muted-foreground truncate">
                    {tb.comment ?? ""}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </div>
    </div>
  );
}
