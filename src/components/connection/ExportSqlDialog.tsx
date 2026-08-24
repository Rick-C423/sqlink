import { useEffect, useState } from "react";
import { Dialog, Button, Label, Alert } from "@/components/ui";
import { tauriService } from "@/services/tauriService";
import { DumpSummary, TableInfo } from "@/types/database";
import { save } from "@tauri-apps/plugin-dialog";
import { FileDown, Loader2 } from "lucide-react";
import { useT } from "@/i18n";

interface ExportSqlDialogProps {
  open: boolean;
  connectionId: string;
  database: string;
  onClose: () => void;
}

/** 转储 SQL 文件对话框：选表 + DROP/数据选项 + 保存路径 */
export function ExportSqlDialog({ open: isOpen, connectionId, database, onClose }: ExportSqlDialogProps) {
  const t = useT();
  const [tables, setTables] = useState<TableInfo[]>([]);
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [includeDrop, setIncludeDrop] = useState(true);
  const [includeData, setIncludeData] = useState(true);
  const [path, setPath] = useState("");
  const [loadingTables, setLoadingTables] = useState(false);
  const [running, setRunning] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [summary, setSummary] = useState<DumpSummary | null>(null);

  useEffect(() => {
    if (!isOpen) return;
    setLoadingTables(true);
    setError(null);
    tauriService
      .getTables(connectionId, database)
      .then((ts) => {
        setTables(ts);
        setSelected(new Set(ts.map((tb) => tb.name)));
      })
      .catch((err) => setError(t("exportSql.loadTablesFailed", { error: String(err) })))
      .finally(() => setLoadingTables(false));
  }, [isOpen, connectionId, database, t]);

  const reset = () => {
    setPath("");
    setIncludeDrop(true);
    setIncludeData(true);
    setError(null);
    setSummary(null);
    setTables([]);
    setSelected(new Set());
  };

  const toggle = (name: string) =>
    setSelected((prev) => {
      const next = new Set(prev);
      if (next.has(name)) next.delete(name);
      else next.add(name);
      return next;
    });

  const pickPath = async () => {
    const p = await save({
      defaultPath: `${database}.sql`,
      filters: [{ name: t("importSql.filterName"), extensions: ["sql"] }],
    });
    if (p) setPath(p);
  };

  const handleExport = async () => {
    if (!path) {
      setError(t("exportSql.choosePathFirst"));
      return;
    }
    if (selected.size === 0) {
      setError(t("exportSql.chooseOneTable"));
      return;
    }
    setRunning(true);
    setError(null);
    try {
      const allSelected = selected.size === tables.length;
      const s = await tauriService.dumpSqlFile(
        connectionId,
        database,
        allSelected ? undefined : [...selected],
        path,
        includeDrop,
        includeData
      );
      setSummary(s);
    } catch (err) {
      setError(String(err));
    } finally {
      setRunning(false);
    }
  };

  return (
    <Dialog
      open={isOpen}
      onClose={() => {
        if (running) return;
        reset();
        onClose();
      }}
      title={t("exportSql.title", { database })}
      footer={
        <>
          <Button
            variant="outline"
            disabled={running}
            onClick={() => {
              reset();
              onClose();
            }}
          >
            {summary ? t("importSql.done") : t("common.cancel")}
          </Button>
          {!summary && (
            <Button onClick={handleExport} disabled={running || !path || selected.size === 0}>
              {running && <Loader2 className="h-4 w-4 animate-spin" />}
              {t("exportSql.start")}
            </Button>
          )}
        </>
      }
    >
      <div className="space-y-4">
        {error && <Alert variant="destructive">{error}</Alert>}
        {summary && (
          <Alert>
            {t("exportSql.summary", { tables: summary.tables, rows: summary.rows.toLocaleString(), path })}
          </Alert>
        )}

        <div className="space-y-1.5">
          <div className="flex items-center justify-between">
            <Label>{t("exportSql.selectTables", { selected: selected.size, total: tables.length })}</Label>
            <span className="flex gap-2 text-xs">
              <button
                className="text-blue-600 hover:underline"
                onClick={() => setSelected(new Set(tables.map((tb) => tb.name)))}
              >
                {t("exportSql.selectAll")}
              </button>
              <button className="text-blue-600 hover:underline" onClick={() => setSelected(new Set())}>
                {t("exportSql.selectNone")}
              </button>
            </span>
          </div>
          <div className="max-h-44 overflow-auto custom-scrollbar rounded-md border p-2 space-y-0.5">
            {loadingTables ? (
              <div className="flex items-center gap-2 text-xs text-muted-foreground py-2">
                <Loader2 className="h-3.5 w-3.5 animate-spin" /> {t("exportSql.loadingTables")}
              </div>
            ) : (
              tables.map((tb) => (
                <label key={tb.name} className="flex items-center gap-2 text-sm py-0.5 cursor-pointer">
                  <input
                    type="checkbox"
                    checked={selected.has(tb.name)}
                    disabled={running}
                    onChange={() => toggle(tb.name)}
                  />
                  <span className="truncate">{tb.name}</span>
                  {tb.rowCount != null && (
                    <span className="text-xs text-muted-foreground ml-auto">
                      {t("exportSql.rowsUnit", { count: Number(tb.rowCount).toLocaleString() })}
                    </span>
                  )}
                </label>
              ))
            )}
          </div>
        </div>

        <div className="space-y-1.5">
          <Label>{t("exportSql.options")}</Label>
          <label className="flex items-center gap-2 text-sm cursor-pointer">
            <input
              type="checkbox"
              checked={includeDrop}
              disabled={running}
              onChange={(e) => setIncludeDrop(e.target.checked)}
            />
            {t("exportSql.includeDrop")}
          </label>
          <label className="flex items-center gap-2 text-sm cursor-pointer">
            <input
              type="checkbox"
              checked={includeData}
              disabled={running}
              onChange={(e) => setIncludeData(e.target.checked)}
            />
            {t("exportSql.includeData")}
          </label>
        </div>

        <div className="space-y-1.5">
          <Label>{t("exportSql.saveTo")}</Label>
          <div className="flex gap-2">
            <div className="flex-1 h-9 rounded-md border bg-muted/40 px-3 flex items-center text-sm truncate">
              {path || <span className="text-muted-foreground">{t("exportSql.noPath")}</span>}
            </div>
            <Button variant="outline" onClick={pickPath} disabled={running}>
              <FileDown className="h-4 w-4 mr-1.5" />
              {t("importSql.choose")}
            </Button>
          </div>
        </div>
      </div>
    </Dialog>
  );
}
