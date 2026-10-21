import { useEffect, useRef, useState } from "react";
import { Dialog, Button, Label, Alert } from "@/components/ui";
import { tauriService } from "@/services/tauriService";
import type { ImportPlan, ImportReport } from "@/types/importJob";
import type { TaskSnapshot } from "@/types/task";
import { open } from "@tauri-apps/plugin-dialog";
import { FileCode2, Loader2 } from "lucide-react";
import { useT } from "@/i18n";

type OnConflict = "error" | "skip" | "overwrite";

const CONFLICT_OPTIONS = [
  { value: "error", labelKey: "importSql.conflictError.label", descKey: "importSql.conflictError.desc" },
  { value: "skip", labelKey: "importSql.conflictSkip.label", descKey: "importSql.conflictSkip.desc" },
  { value: "overwrite", labelKey: "importSql.conflictOverwrite.label", descKey: "importSql.conflictOverwrite.desc" },
] as const;

interface ImportSqlDialogProps {
  open: boolean;
  connectionId: string;
  database: string;
  onClose: () => void;
  /** 导入成功后回调（用于刷新库节点） */
  onImported: () => void;
}

export function ImportSqlDialog({
  open: isOpen,
  connectionId,
  database,
  onClose,
  onImported,
}: ImportSqlDialogProps) {
  const t = useT();
  const [path, setPath] = useState("");
  const [onConflict, setOnConflict] = useState<OnConflict>("error");
  const [running, setRunning] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [plan, setPlan] = useState<ImportPlan | null>(null);
  const [report, setReport] = useState<ImportReport | null>(null);
  const [task, setTask] = useState<TaskSnapshot | null>(null);
  const [taskId, setTaskId] = useState<string | null>(null);
  const [cancelRequested, setCancelRequested] = useState(false);
  const [receiptId, setReceiptId] = useState("");
  const epoch = useRef(0);
  const summary = report?.summary;

  useEffect(() => {
    epoch.current += 1;
    setPath(""); setOnConflict("error"); setPlan(null); setReport(null);
    setTask(null); setTaskId(null); setRunning(false); setError(null); setCancelRequested(false); setReceiptId("");
    return () => { epoch.current += 1; };
  }, [isOpen, connectionId, database]);

  useEffect(() => {
    if (!running || !taskId) return;
    const current = epoch.current;
    let stopped = false;
    const poll = async () => {
      try {
        const next = await tauriService.getTaskStatus(taskId);
        if (!stopped && current === epoch.current) setTask(previous => !previous || previous.taskId !== next.taskId || previous.sequence <= next.sequence ? next : previous);
      } catch { /* 刚创建的任务可能尚未注册，等待下一次状态。 */ }
    };
    void poll();
    const timer = setInterval(() => void poll(), 350);
    return () => { stopped = true; clearInterval(timer); };
  }, [running, taskId]);

  const reset = () => {
    epoch.current += 1;
    setPath("");
    setOnConflict("error");
    setError(null);
    setPlan(null); setReport(null); setTask(null); setTaskId(null); setCancelRequested(false);
  };

  const pickFile = async () => {
    const current = ++epoch.current;
    setPlan(null); setReport(null); setError(null);
    try {
    const selected = await open({
      multiple: false,
      filters: [{ name: t("importSql.filterName"), extensions: ["sql", "txt"] }],
    });
    if (current === epoch.current && typeof selected === "string") setPath(selected);
    } catch (err) { if (current === epoch.current) setError(String(err)); }
  };

  const handleImport = async () => {
    if (!path) {
      setError(t("importSql.selectFirst"));
      return;
    }
    const current = ++epoch.current;
    const id = crypto.randomUUID();
    setTaskId(id); setTask(null); setCancelRequested(false); setRunning(true);
    setError(null);
    setReport(null);
    try {
      if (!plan) {
        const prepared = await tauriService.previewImport({ connectionId, database, path, onConflict, taskId: id });
        if (current === epoch.current) { setPlan(prepared); setTask(prepared.task); }
      } else {
        const result = await tauriService.executeImport({ planId: plan.planId, taskId: id });
        if (current !== epoch.current) return;
        setReport(result); setTask(result.task); setPlan(null);
        if (result.errorCode) setError(result.message || result.errorCode);
        if (result.sentStatements > 0) onImported();
      }
    } catch (err) {
      if (current === epoch.current) { setError(String(err)); setPlan(null); }
    } finally {
      if (current === epoch.current) { setRunning(false); setTaskId(null); }
    }
  };

  const cancel = async () => {
    if (!taskId || cancelRequested) return;
    const current = epoch.current;
    try {
      await tauriService.cancelImport(connectionId, taskId);
      if (current === epoch.current) setCancelRequested(true);
    } catch (err) { if (current === epoch.current) setError(String(err)); }
  };

  const stages: Record<string, string> = { validating: "核对文件", parsing: "预检拆分", executing: "执行语句", cleanup: "清理会话" };
  const readReceipt = async () => {
    const current = ++epoch.current;
    setError(null);
    try {
      const result = await tauriService.getImportReceipt(connectionId, receiptId.trim());
      if (current === epoch.current) { setReport(result); setTask(result.task); setPlan(null); }
    } catch (err) { if (current === epoch.current) setError(String(err)); }
  };
  const progress = task?.progress;

  return (
    <Dialog
      open={isOpen}
      onClose={() => {
        if (running) return;
        reset();
        onClose();
      }}
      title={t("importSql.title", { database })}
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
            <Button onClick={handleImport} disabled={running || !path}>
              {running && <Loader2 className="h-4 w-4 animate-spin" />}
              {plan ? t("importSql.start") : "预检文件"}
            </Button>
          )}
        </>
      }
    >
      <div className="space-y-4">
        {error && <Alert variant="destructive">{error}</Alert>}
        {running && <Alert>
          {taskId && <p>恢复编号（可复制，用于查询执行记录）：<code className="select-text">{taskId}</code></p>}
          <p>{progress ? stages[progress.stage] || progress.stage : "准备导入任务"}；已处理 {progress?.processedStatements ?? 0} 条语句</p>
          {progress?.totalBytes != null && <p>已处理 {progress.processedBytes} / {progress.totalBytes} 字节（{progress.totalBytes === 0 ? 100 : Math.floor(progress.processedBytes / progress.totalBytes * 100)}%）</p>}
          <Button variant="outline" onClick={() => void cancel()} disabled={cancelRequested || task?.capabilities?.cancellation === false && task.phase === "running"}>
            {cancelRequested || task?.cancelRequested ? "已请求取消，等待执行边界与清理结果" : "请求取消"}
          </Button>
          {task?.capabilities?.cancellation === false && <p>{task.capabilities.cancellationReason}</p>}
        </Alert>}
        {plan && !running && <Alert>
          <p>预检完成：{plan.statementCount} 条语句，{plan.fileBytes} 字节。计划有效至 {new Date(plan.expiresAt).toLocaleTimeString()}。</p>
          {plan.warnings.map(warning => <p key={warning}>{warning}</p>)}
        </Alert>}
        {summary && (
          <Alert>
            {t("importSql.summary", { total: summary.statements, executed: summary.executed })}
            {summary.skipped > 0 && t("importSql.summarySkipped", { skipped: summary.skipped })}
          </Alert>
        )}
        {summary?.cleanupNotice && <Alert>{summary.cleanupNotice}</Alert>}
        {report && <Alert>
          <p>执行记录：{report.runId}</p>
          <p>状态：{report.task.phase}；已发送或无法排除已发送 {report.sentStatements} 条；执行成功 {report.summary.executed} 条；失败序号 {report.failedStatement ?? "无"}。</p>
          <p>提交状态：{report.commitState}；清理状态：{report.cleanupState}。{report.recovery === "safeToRetry" ? "未发送 SQL，计划有效且文件未变时可重新预检后重试。" : "请人工核对数据库，禁止按序号自动重放。"}</p>
          {!report.durable && <p>当前仅保存内存记录，应用重启后不可恢复。</p>}
        </Alert>}

        <div className="space-y-1.5">
          <Label>{t("importSql.fileLabel")}</Label>
          <div className="flex gap-2">
            <div className="flex-1 h-9 rounded-md border bg-muted/40 px-3 flex items-center text-sm truncate">
              {path || <span className="text-muted-foreground">{t("importSql.noFile")}</span>}
            </div>
            <Button variant="outline" onClick={pickFile} disabled={running}>
              <FileCode2 className="h-4 w-4 mr-1.5" />
              {t("importSql.choose")}
            </Button>
          </div>
        </div>

        <div className="space-y-2">
          <Label>{t("importSql.conflictLabel")}</Label>
          <div className="space-y-1.5">
            {CONFLICT_OPTIONS.map((opt) => (
              <label
                key={opt.value}
                className={`flex items-start gap-2 rounded-md border px-3 py-2 cursor-pointer text-sm ${
                  onConflict === opt.value ? "border-blue-400 bg-blue-50/50 dark:bg-white/5" : ""
                } ${opt.value === "overwrite" ? "text-red-700" : ""}`}
              >
                <input
                  type="radio"
                  className="mt-0.5"
                  checked={onConflict === opt.value}
                  disabled={running}
                  onChange={() => { epoch.current += 1; setOnConflict(opt.value); setPlan(null); setReport(null); setError(null); }}
                />
                <span>
                  <span className="font-medium">{t(opt.labelKey)}</span>
                  <span className="block text-xs text-muted-foreground mt-0.5">{t(opt.descKey)}</span>
                </span>
              </label>
            ))}
          </div>
        </div>

        <p className="text-xs text-muted-foreground">{t("importSql.hint")}</p>
        {!running && <div className="space-y-1.5">
          <Label>查询执行记录（恢复编号或 runId，只供人工核对，不会重放 SQL）</Label>
          <div className="flex gap-2">
            <input aria-label="执行记录编号" className="flex-1 min-w-0 rounded-md border bg-background px-3 text-sm" value={receiptId} onChange={event => { epoch.current += 1; setReceiptId(event.target.value); }} placeholder="恢复编号 / runId" />
            <Button variant="outline" disabled={!receiptId.trim()} onClick={() => void readReceipt()}>查询执行记录</Button>
          </div>
        </div>}
      </div>
    </Dialog>
  );
}
