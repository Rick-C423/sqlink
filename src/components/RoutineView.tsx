import { useCallback, useEffect, useState } from "react";
import { tauriService } from "@/services/tauriService";
import { Button } from "@/components/ui";
import { useT } from "@/i18n";
import type { MessageKey } from "@/i18n/zh-CN";
import type { RoutineKind } from "@/stores/workspaceStore";
import { Check, Copy, Loader2, RefreshCw } from "lucide-react";

interface RoutineViewProps {
  connectionId: string;
  database: string;
  routineKind: RoutineKind;
  name: string;
}

const BADGE_KEY: Record<RoutineKind, MessageKey> = {
  trigger: "routineView.trigger",
  procedure: "routineView.procedure",
  function: "routineView.function",
};

/** 触发器/存储过程/函数页：创建定义只读展示，支持复制与刷新 */
export function RoutineView({ connectionId, database, routineKind, name }: RoutineViewProps) {
  const t = useT();
  const [definition, setDefinition] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [copied, setCopied] = useState(false);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const def =
        routineKind === "trigger"
          ? await tauriService.getTriggerDefinition(connectionId, database, name)
          : routineKind === "procedure"
            ? await tauriService.getProcedureDefinition(connectionId, database, name)
            : await tauriService.getFunctionDefinition(connectionId, database, name);
      setDefinition(def);
    } catch (err) {
      setError(String(err));
      setDefinition(null);
    } finally {
      setLoading(false);
    }
  }, [connectionId, database, routineKind, name]);

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
        <span className="text-[10px] px-1 rounded bg-muted text-muted-foreground">
          {t(BADGE_KEY[routineKind])}
        </span>
        <span className="ml-auto text-xs text-muted-foreground pr-2">
          {database} / {name}
        </span>
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
