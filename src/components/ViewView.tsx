import { useCallback, useEffect, useState } from "react";
import { tauriService } from "@/services/tauriService";
import { TableData } from "@/components/TableView";
import { Button } from "@/components/ui";
import { cn } from "@/lib/utils";
import { Check, Copy, Loader2, RefreshCw } from "lucide-react";
import { useT } from "@/i18n";

interface ViewViewProps {
  connectionId: string;
  database: string;
  view: string;
}

type SubTab = "data" | "definition";

/** 视图页：数据（只读网格，复用 TableData） + 定义（CREATE VIEW 语句只读展示） */
export function ViewView({ connectionId, database, view }: ViewViewProps) {
  const t = useT();
  const [subTab, setSubTab] = useState<SubTab>("data");

  return (
    <div className="flex flex-col h-full">
      <div className="flex items-center border-b bg-muted/20 px-2">
        {(["data", "definition"] as SubTab[]).map((st) => (
          <button
            key={st}
            className={cn(
              "px-3 py-1.5 text-sm border-b-2 -mb-px",
              subTab === st
                ? "border-blue-500 text-blue-600 font-medium"
                : "border-transparent text-muted-foreground hover:text-foreground"
            )}
            onClick={() => setSubTab(st)}
          >
            {st === "data" ? t("tableView.data") : t("viewView.definition")}
          </button>
        ))}
        <span className="ml-2 text-[10px] px-1 rounded bg-muted text-muted-foreground">{t("viewView.badge")}</span>
        <span className="ml-auto text-xs text-muted-foreground pr-2">
          {database} / {view}
        </span>
      </div>

      {subTab === "data" ? (
        <TableData connectionId={connectionId} database={database} table={view} readonly />
      ) : (
        <ViewDefinition connectionId={connectionId} database={database} view={view} />
      )}
    </div>
  );
}

/** 视图定义：只读 SQL 文本，支持复制与刷新 */
function ViewDefinition({ connectionId, database, view }: ViewViewProps) {
  const t = useT();
  const [definition, setDefinition] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [copied, setCopied] = useState(false);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      setDefinition(await tauriService.getViewDefinition(connectionId, database, view));
    } catch (err) {
      setError(String(err));
      setDefinition(null);
    } finally {
      setLoading(false);
    }
  }, [connectionId, database, view]);

  useEffect(() => {
    load();
  }, [load]);

  const copy = async () => {
    if (!definition) return;
    try {
      await navigator.clipboard.writeText(definition);
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    } catch {
      // 剪贴板不可用时忽略
    }
  };

  return (
    <div className="flex flex-col h-full min-h-0">
      <div className="flex items-center gap-2 px-3 py-1.5 border-b bg-muted/10">
        <Button variant="ghost" size="sm" className="h-6" onClick={load} disabled={loading}>
          {loading ? <Loader2 className="h-3 w-3 animate-spin" /> : <RefreshCw className="h-3 w-3" />}
          <span className="ml-1">{t("common.refresh")}</span>
        </Button>
        <Button variant="ghost" size="sm" className="h-6" onClick={copy} disabled={!definition}>
          {copied ? <Check className="h-3 w-3 text-green-500" /> : <Copy className="h-3 w-3" />}
          <span className="ml-1">{copied ? t("grid.copied") : t("viewView.copy")}</span>
        </Button>
      </div>
      {error && (
        <div className="px-3 py-1.5 bg-red-50 text-red-700 text-xs border-b border-red-200">
          {t("common.loadFailed", { error })}
        </div>
      )}
      <div className="flex-1 overflow-auto custom-scrollbar">
        <pre className="p-3 text-xs font-mono leading-5 whitespace-pre-wrap break-all select-text">
          {definition ?? (loading ? t("viewView.loading") : t("viewView.noDefinition"))}
        </pre>
      </div>
    </div>
  );
}
