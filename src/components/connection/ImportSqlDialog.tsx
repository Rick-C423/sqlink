import { useState } from "react";
import { Dialog, Button, Label, Alert } from "@/components/ui";
import { tauriService } from "@/services/tauriService";
import { ImportSummary } from "@/types/database";
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
  const [summary, setSummary] = useState<ImportSummary | null>(null);

  const reset = () => {
    setPath("");
    setOnConflict("error");
    setError(null);
    setSummary(null);
  };

  const pickFile = async () => {
    const selected = await open({
      multiple: false,
      filters: [{ name: t("importSql.filterName"), extensions: ["sql", "txt"] }],
    });
    if (typeof selected === "string") setPath(selected);
  };

  const handleImport = async () => {
    if (!path) {
      setError(t("importSql.selectFirst"));
      return;
    }
    setRunning(true);
    setError(null);
    setSummary(null);
    try {
      const s = await tauriService.importSqlFile(connectionId, database, path, onConflict);
      setSummary(s);
      onImported();
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
              {t("importSql.start")}
            </Button>
          )}
        </>
      }
    >
      <div className="space-y-4">
        {error && <Alert variant="destructive">{error}</Alert>}
        {summary && (
          <Alert>
            {t("importSql.summary", { total: summary.statements, executed: summary.executed })}
            {summary.skipped > 0 && t("importSql.summarySkipped", { skipped: summary.skipped })}
          </Alert>
        )}

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
                  onChange={() => setOnConflict(opt.value)}
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
      </div>
    </Dialog>
  );
}
