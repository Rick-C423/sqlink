import { useState } from "react";
import { Dialog, Button, Label, Alert } from "@/components/ui";
import { tauriService } from "@/services/tauriService";
import { ImportSummary } from "@/types/database";
import { open } from "@tauri-apps/plugin-dialog";
import { FileCode2, Loader2 } from "lucide-react";

type OnConflict = "error" | "skip" | "overwrite";

const CONFLICT_OPTIONS: { value: OnConflict; label: string; desc: string }[] = [
  {
    value: "error",
    label: "报错中止（默认）",
    desc: "遇到已存在的表由服务器报错，导入停止。适合导入到空库。",
  },
  {
    value: "skip",
    label: "跳过已存在的表",
    desc: "已存在的表保留原结构和数据，跳过其建表语句；对应的 INSERT 仍会执行，可能产生主键冲突。",
  },
  {
    value: "overwrite",
    label: "覆盖已存在的表",
    desc: "建表前先 DROP TABLE IF EXISTS —— 已存在表的结构和数据会被完全替换，不可恢复！",
  },
];

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
      filters: [{ name: "SQL 转储", extensions: ["sql", "txt"] }],
    });
    if (typeof selected === "string") setPath(selected);
  };

  const handleImport = async () => {
    if (!path) {
      setError("请先选择 SQL 文件");
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
      title={`导入 SQL 文件 → ${database}`}
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
            {summary ? "完成" : "取消"}
          </Button>
          {!summary && (
            <Button onClick={handleImport} disabled={running || !path}>
              {running && <Loader2 className="h-4 w-4 animate-spin" />}
              开始导入
            </Button>
          )}
        </>
      }
    >
      <div className="space-y-4">
        {error && <Alert variant="destructive">{error}</Alert>}
        {summary && (
          <Alert>
            导入完成：共 {summary.statements} 条语句，执行 {summary.executed} 条
            {summary.skipped > 0 && `，跳过已存在表 ${summary.skipped} 条`}
          </Alert>
        )}

        <div className="space-y-1.5">
          <Label>SQL 文件</Label>
          <div className="flex gap-2">
            <div className="flex-1 h-9 rounded-md border bg-muted/40 px-3 flex items-center text-sm truncate">
              {path || <span className="text-muted-foreground">未选择文件</span>}
            </div>
            <Button variant="outline" onClick={pickFile} disabled={running}>
              <FileCode2 className="h-4 w-4 mr-1.5" />
              选择…
            </Button>
          </div>
        </div>

        <div className="space-y-2">
          <Label>表已存在时</Label>
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
                  <span className="font-medium">{opt.label}</span>
                  <span className="block text-xs text-muted-foreground mt-0.5">{opt.desc}</span>
                </span>
              </label>
            ))}
          </div>
        </div>

        <p className="text-xs text-muted-foreground">
          逐条执行，出错即中止并报告位置；大文件请耐心等待，窗口会暂时无响应属正常。
          不支持 pg_dump 的 \connect 等 psql 元命令。
        </p>
      </div>
    </Dialog>
  );
}
