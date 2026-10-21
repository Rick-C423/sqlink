import { useEffect, useRef, useState } from "react";
import { Dialog, Button, Label, Alert } from "@/components/ui";
import { tauriService } from "@/services/tauriService";
import { DumpSummary, ObjectIdentity, TableInfo } from "@/types/database";
import { save } from "@tauri-apps/plugin-dialog";
import { FileDown, Loader2 } from "lucide-react";
import { useT } from "@/i18n";
import type { SessionCapabilities, TaskSnapshot } from "@/types/task";

interface ExportJob {
  taskId: string;
  connectionId: string;
  database: string;
  path: string;
  tableSources: ObjectIdentity[];
}

const tableSelectionKey = (table: TableInfo) => JSON.stringify([table.schema ?? null, table.name]);

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
  const [job, setJob] = useState<ExportJob | null>(null);
  const jobRef = useRef<ExportJob | null>(null);
  const loadGeneration = useRef(0);
  const capabilitiesGeneration = useRef(0);
  const sourceRef = useRef({ connectionId, database });
  const [loadedSource, setLoadedSource] = useState<string | null>(null);
  const [capabilities, setCapabilities] = useState<SessionCapabilities | null>(null);
  const [phase, setPhase] = useState<TaskSnapshot["phase"]>("queued");
  const [statusUnavailable, setStatusUnavailable] = useState(false);
  const [cancelRequested, setCancelRequested] = useState(false);
  const cancelRequestedRef = useRef(false);
  const [cancelError, setCancelError] = useState<string | null>(null);
  // 一个任务的来源、路径和标题在结果返回前后都保持一致，关闭后才重置。
  const sourceConnectionId = job?.connectionId ?? connectionId;
  const sourceDatabase = job?.database ?? database;
  const sourceKey = JSON.stringify([sourceConnectionId, sourceDatabase]);
  const tablesReady = loadedSource === sourceKey && !loadingTables;
  sourceRef.current = { connectionId: sourceConnectionId, database: sourceDatabase };

  useEffect(() => () => { jobRef.current = null; }, []);

  useEffect(() => {
    if (!isOpen) return;
    const generation = ++loadGeneration.current;
    if (jobRef.current) return;
    setLoadingTables(true);
    setLoadedSource(null);
    setTables([]);
    setSelected(new Set());
    setError(null);
    tauriService
      .getTables(sourceConnectionId, sourceDatabase)
      .then((ts) => {
        if (generation !== loadGeneration.current || jobRef.current) return;
        setTables(ts);
        setSelected(new Set(ts.map(tableSelectionKey)));
        setLoadedSource(sourceKey);
      })
      .catch((err) => {
        if (generation === loadGeneration.current && !jobRef.current) {
          setError(t("exportSql.loadTablesFailed", { error: String(err) }));
        }
      })
      .finally(() => {
        if (generation === loadGeneration.current && !jobRef.current) setLoadingTables(false);
      });
    return () => { ++loadGeneration.current; };
  }, [isOpen, sourceConnectionId, sourceDatabase, sourceKey, t]);

  useEffect(() => {
    if (!isOpen) return;
    const generation = ++capabilitiesGeneration.current;
    setCapabilities(null);
    tauriService.getSessionCapabilities(sourceConnectionId)
      .then((value) => {
        if (generation === capabilitiesGeneration.current) setCapabilities(value);
      })
      .catch(() => { /* 未确认能力时禁用取消，导出本身仍可执行。 */ });
    return () => { ++capabilitiesGeneration.current; };
  }, [isOpen, sourceConnectionId]);

  useEffect(() => {
    if (!running || !job) return;
    let stopped = false;
    let timer: ReturnType<typeof setTimeout> | undefined;
    const poll = async () => {
      try {
        const snapshot = await tauriService.getTaskStatus(job.taskId);
        if (stopped || jobRef.current !== job) return;
        if (snapshot.taskId !== job.taskId || snapshot.connectionId !== job.connectionId) return;
        setPhase(snapshot.phase);
        setStatusUnavailable(false);
        if (snapshot.capabilities) setCapabilities(snapshot.capabilities);
        if (snapshot.cancelRequested) {
          cancelRequestedRef.current = true;
          setCancelRequested(true);
        }
      } catch {
        if (!stopped && jobRef.current === job) setStatusUnavailable(true);
      } finally {
        if (!stopped && jobRef.current === job) timer = setTimeout(poll, 700);
      }
    };
    void poll();
    return () => { stopped = true; if (timer) clearTimeout(timer); };
  }, [running, job]);

  const reset = () => {
    if (jobRef.current) return;
    setPath("");
    setIncludeDrop(true);
    setIncludeData(true);
    setError(null);
    setSummary(null);
    setTables([]);
    setSelected(new Set());
    setLoadedSource(null);
    setJob(null);
    cancelRequestedRef.current = false;
    setCancelRequested(false);
    setCancelError(null);
  };

  const toggle = (name: string) => {
    if (jobRef.current || !tablesReady) return;
    setSelected((prev) => {
      const next = new Set(prev);
      if (next.has(name)) next.delete(name);
      else next.add(name);
      return next;
    });
  };

  const pickPath = async () => {
    if (jobRef.current) return;
    const source = sourceRef.current;
    const p = await save({
      defaultPath: `${source.database}.sql`,
      filters: [{ name: t("importSql.filterName"), extensions: ["sql"] }],
    });
    if (p && !jobRef.current && sourceRef.current.connectionId === source.connectionId &&
        sourceRef.current.database === source.database) setPath(p);
  };

  const handleExport = async () => {
    if (jobRef.current || !tablesReady) return;
    if (!path) {
      setError(t("exportSql.choosePathFirst"));
      return;
    }
    if (selected.size === 0) {
      setError(t("exportSql.chooseOneTable"));
      return;
    }
    const tableSources = tables.filter((table) => selected.has(tableSelectionKey(table))).map((table) => ({
      database: sourceDatabase, schema: table.schema, table: table.name,
    }));
    if (tableSources.length === 0) return;
    const current: ExportJob = {
      taskId: crypto.randomUUID(), connectionId: sourceConnectionId, database: sourceDatabase, path, tableSources,
    };
    jobRef.current = current;
    setJob(current);
    ++loadGeneration.current;
    setRunning(true);
    setError(null);
    setCancelError(null);
    cancelRequestedRef.current = false;
    setCancelRequested(false);
    setStatusUnavailable(false);
    setPhase("queued");
    try {
      const s = await tauriService.dumpSqlFile(
        current.connectionId,
        current.database,
        undefined,
        path,
        includeDrop,
        includeData,
        current.taskId,
        current.tableSources
      );
      if (jobRef.current === current) setSummary(s);
    } catch (err) {
      if (jobRef.current === current) setError(String(err));
    } finally {
      if (jobRef.current === current) {
        jobRef.current = null;
        cancelRequestedRef.current = false;
        setRunning(false);
        setCancelRequested(false);
      }
    }
  };

  const cancelExport = async () => {
    const current = jobRef.current;
    if (!current || cancelRequestedRef.current || capabilities?.cancellation !== true) return;
    cancelRequestedRef.current = true;
    setCancelRequested(true);
    setCancelError(null);
    try {
      await tauriService.cancelExport(current.connectionId, current.taskId);
      // 这里只确认取消请求已被接收，最终状态由原导出调用返回。
    } catch (err) {
      if (jobRef.current !== current) return;
      cancelRequestedRef.current = false;
      setCancelRequested(false);
      setCancelError(t("exportSql.cancelFailed", { error: String(err) }));
    }
  };

  const phaseMessage = phase === "queued" || phase === "preparing" || phase === "running" || phase === "committing"
    ? t(`exportSql.phase.${phase}`) : t("exportSql.awaitingResult");

  return (
    <Dialog
      open={isOpen}
      onClose={() => {
        if (jobRef.current) return;
        reset();
        onClose();
      }}
      title={t("exportSql.title", { database: sourceDatabase })}
      footer={
        <>
          <Button
            variant="outline"
            disabled={running}
            onClick={() => {
              if (jobRef.current) return;
              reset();
              onClose();
            }}
          >
            {summary ? t("importSql.done") : t("common.cancel")}
          </Button>
          {running && (
            <Button variant="outline" onClick={cancelExport}
              disabled={cancelRequested || capabilities?.cancellation !== true}
              title={capabilities?.cancellation === false
                ? capabilities.cancellationReason ?? t("exportSql.cancelUnsupported")
                : !capabilities ? t("exportSql.capabilitiesUnavailable") : undefined}>
              {t("exportSql.cancel")}
            </Button>
          )}
          {!summary && (
            <Button onClick={handleExport} disabled={running || !tablesReady || !path || selected.size === 0}>
              {running && <Loader2 className="h-4 w-4 animate-spin" />}
              {t("exportSql.start")}
            </Button>
          )}
        </>
      }
    >
      <div className="space-y-4">
        {error && <Alert variant="destructive">{error}</Alert>}
        {cancelError && <Alert variant="destructive">{cancelError}</Alert>}
        {running && <div role="status" className="text-sm text-muted-foreground">
          {cancelRequested ? t("exportSql.cancelRequested")
            : statusUnavailable ? t("exportSql.statusUnavailable") : phaseMessage}
          {capabilities?.cancellation === false && <p>{capabilities.cancellationReason ?? t("exportSql.cancelUnsupported")}</p>}
        </div>}
        {summary && (
          <Alert>
            {t("exportSql.summary", { tables: summary.tables, rows: summary.rows.toLocaleString(), path: job?.path ?? path })}
          </Alert>
        )}

        <div className="space-y-1.5">
          <div className="flex items-center justify-between">
            <Label>{t("exportSql.selectTables", { selected: selected.size, total: tables.length })}</Label>
            <span className="flex gap-2 text-xs">
              <button
                className="text-blue-600 hover:underline"
                disabled={running || !tablesReady}
                onClick={() => { if (!jobRef.current && tablesReady) setSelected(new Set(tables.map(tableSelectionKey))); }}
              >
                {t("exportSql.selectAll")}
              </button>
              <button className="text-blue-600 hover:underline" disabled={running || !tablesReady}
                onClick={() => { if (!jobRef.current && tablesReady) setSelected(new Set()); }}>
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
                <label key={tableSelectionKey(tb)} className="flex items-center gap-2 text-sm py-0.5 cursor-pointer">
                  <input
                    type="checkbox"
                    checked={selected.has(tableSelectionKey(tb))}
                    disabled={running}
                    onChange={() => toggle(tableSelectionKey(tb))}
                  />
                  <span className="truncate">{[tb.schema, tb.name].filter(Boolean).join(".")}</span>
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
