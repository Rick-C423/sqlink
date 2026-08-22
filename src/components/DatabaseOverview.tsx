import { useCallback, useEffect, useState } from "react";
import { TableInfo } from "@/types/database";
import { tauriService } from "@/services/tauriService";
import { useWorkspaceStore } from "@/stores/workspaceStore";
import { Button } from "@/components/ui";
import { Loader2, RefreshCw, Table2 } from "lucide-react";

interface DatabaseOverviewProps {
  connectionId: string;
  database: string;
}

/** 数据库概览：该库下的所有表（名称/引擎/估算行数/注释），双击打开表视图 */
export function DatabaseOverview({ connectionId, database }: DatabaseOverviewProps) {
  const [tables, setTables] = useState<TableInfo[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const openTable = useWorkspaceStore((s) => s.openTable);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const list = await tauriService.getTables(connectionId, database);
      setTables(list);
    } catch (err) {
      setError(String(err));
    } finally {
      setLoading(false);
    }
  }, [connectionId, database]);

  useEffect(() => {
    load();
  }, [load]);

  return (
    <div className="flex flex-col h-full">
      <div className="flex items-center gap-2 px-3 py-1.5 border-b bg-muted/20">
        <span className="text-sm font-medium">数据库: {database}</span>
        <span className="text-xs text-muted-foreground">
          {tables ? `${tables.length} 张表` : ""}
        </span>
        <Button variant="ghost" size="sm" className="h-6 ml-auto" onClick={load} disabled={loading}>
          {loading ? <Loader2 className="h-3 w-3 animate-spin" /> : <RefreshCw className="h-3 w-3" />}
          <span className="ml-1 text-xs">刷新</span>
        </Button>
      </div>

      {error && (
        <div className="px-3 py-2 bg-red-50 text-red-700 text-xs border-b border-red-200">
          加载失败: {error}
        </div>
      )}

      <div className="flex-1 overflow-auto custom-scrollbar">
        {tables && tables.length === 0 && (
          <div className="flex items-center justify-center h-32 text-muted-foreground text-sm">
            该数据库下没有表
          </div>
        )}
        {tables && tables.length > 0 && (
          <table className="w-full text-sm border-collapse">
            <thead className="sticky top-0 z-10">
              <tr className="bg-gray-100 dark:bg-gray-800">
                <th className="px-3 py-1.5 text-left font-semibold text-xs border-b">表名</th>
                <th className="px-3 py-1.5 text-left font-semibold text-xs border-b">引擎</th>
                <th className="px-3 py-1.5 text-right font-semibold text-xs border-b">行数(估算)</th>
                <th className="px-3 py-1.5 text-left font-semibold text-xs border-b">注释</th>
              </tr>
            </thead>
            <tbody>
              {tables.map((t) => (
                <tr
                  key={t.name}
                  className="row-hover cursor-pointer"
                  onDoubleClick={() => openTable(connectionId, database, t.name)}
                  title="双击打开表"
                >
                  <td className="px-3 py-1.5 border-b">
                    <span className="flex items-center gap-1.5">
                      <Table2 className="h-3.5 w-3.5 text-green-600" />
                      {t.name}
                    </span>
                  </td>
                  <td className="px-3 py-1.5 border-b text-muted-foreground">{t.engine ?? "-"}</td>
                  <td className="px-3 py-1.5 border-b text-right text-muted-foreground">
                    {t.rowCount !== undefined && t.rowCount !== null
                      ? t.rowCount.toLocaleString()
                      : "-"}
                  </td>
                  <td className="px-3 py-1.5 border-b text-muted-foreground truncate max-w-[300px]">
                    {t.comment ?? ""}
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
