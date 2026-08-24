import { useEffect, useRef, useCallback, useState } from "react";
import Editor, { loader } from "@monaco-editor/react";
import { useThemeStore } from "@/stores/themeStore";

import * as monaco from "monaco-editor";
import { useEditorStore } from "@/stores/editorStore";
import { Play, Square, Loader2, FastForward, History, Trash2, X } from "lucide-react";
import { Button } from "@/components/ui";
import { tauriService } from "@/services/tauriService";
import type { HistoryEntry } from "@/types/database";
import { useT } from "@/i18n";

// 配置 Monaco Editor loader
loader.config({ monaco });

const SQL_KEYWORDS = [
  "SELECT", "FROM", "WHERE", "INSERT", "UPDATE", "DELETE",
  "CREATE", "DROP", "ALTER", "TABLE", "INDEX", "DATABASE",
  "JOIN", "LEFT", "RIGHT", "INNER", "OUTER", "ON",
  "GROUP", "BY", "ORDER", "HAVING", "LIMIT", "OFFSET",
  "AND", "OR", "NOT", "IN", "EXISTS", "BETWEEN", "LIKE",
  "COUNT", "SUM", "AVG", "MAX", "MIN", "DISTINCT",
  "AS", "UNION", "ALL", "ASC", "DESC", "NULL", "IS",
];

// 补全 provider 全局只注册一次
let completionProviderRegistered = false;
function registerSqlCompletionOnce() {
  if (completionProviderRegistered) return;
  completionProviderRegistered = true;

  monaco.languages.registerCompletionItemProvider("sql", {
    triggerCharacters: [".", " ", "("],
    provideCompletionItems: (model, position) => {
      const word = model.getWordUntilPosition(position);
      const range = {
        startLineNumber: position.lineNumber,
        endLineNumber: position.lineNumber,
        startColumn: word.startColumn,
        endColumn: word.endColumn,
      };

      const suggestions: monaco.languages.CompletionItem[] = SQL_KEYWORDS.map((kw) => ({
        label: kw,
        kind: monaco.languages.CompletionItemKind.Keyword,
        insertText: kw,
        range,
        sortText: "1",
      }));

      return { suggestions };
    },
  });
}

// 取光标所在的语句（按 ; 分句，跳过引号内的分号）
function getStatementAt(content: string, offset: number): string {
  let start = 0;
  let inStr: string | null = null;
  for (let i = 0; i < content.length; i++) {
    const ch = content[i];
    if (inStr) {
      if (ch === "\\") {
        i++;
      } else if (ch === inStr) {
        inStr = null;
      }
      continue;
    }
    if (ch === "'" || ch === '"' || ch === "`") {
      inStr = ch;
      continue;
    }
    if (ch === ";") {
      if (offset <= i) {
        return content.slice(start, i).trim();
      }
      start = i + 1;
    }
  }
  return content.slice(start).trim();
}

/**
 * 把整段 SQL 拆成语句列表（跳过引号与 --、#、注释块内的分号）。
 * 用于「执行全部」批量跑脚本。
 */
export function splitStatements(content: string): string[] {
  const out: string[] = [];
  let start = 0;
  let inStr: string | null = null;
  const n = content.length;

  for (let i = 0; i < n; i++) {
    const ch = content[i];
    if (inStr) {
      if (ch === "\\") {
        i++;
      } else if (ch === inStr) {
        inStr = null;
      }
      continue;
    }
    // 行注释
    if (ch === "-" && content[i + 1] === "-") {
      while (i < n && content[i] !== "\n") i++;
      continue;
    }
    if (ch === "#") {
      while (i < n && content[i] !== "\n") i++;
      continue;
    }
    // 块注释
    if (ch === "/" && content[i + 1] === "*") {
      i += 2;
      while (i + 1 < n && !(content[i] === "*" && content[i + 1] === "/")) i++;
      i++;
      continue;
    }
    if (ch === "'" || ch === '"' || ch === "`") {
      inStr = ch;
      continue;
    }
    if (ch === ";") {
      const stmt = content.slice(start, i).trim();
      if (stmt) out.push(stmt);
      start = i + 1;
    }
  }
  const tail = content.slice(start).trim();
  if (tail) out.push(tail);
  return out;
}

interface SQLEditorProps {
  /** 对应的 editorStore tab id（tab 条由工作台层统一管理） */
  tabId: string;
  connectionId: string | null;
  onExecute: (sql: string, tabId: string) => void;
  /** 执行全部：把整页内容按分号拆成多条语句批量执行 */
  onExecuteAll?: (statements: string[], tabId: string) => void;
  onCancel: (tabId: string) => void;
}

export function SQLEditorPanel({ tabId, connectionId, onExecute, onExecuteAll, onCancel }: SQLEditorProps) {
  const t = useT();
  const tab = useEditorStore((s) => s.tabs.find((tb) => tb.id === tabId));
  const theme = useThemeStore((s) => s.theme);
  const updateTabContent = useEditorStore((s) => s.updateTabContent);

  const isExecuting = tab?.isExecuting ?? false;
  const editorRef = useRef<monaco.editor.IStandaloneCodeEditor | null>(null);

  // 查询历史面板
  const [historyOpen, setHistoryOpen] = useState(false);
  const [historyList, setHistoryList] = useState<HistoryEntry[]>([]);

  const loadHistory = useCallback(async () => {
    if (!connectionId) {
      setHistoryList([]);
      return;
    }
    try {
      setHistoryList(await tauriService.listHistory(connectionId, 100));
    } catch {
      // 历史加载失败不影响编辑
    }
  }, [connectionId]);

  // 面板打开时加载；执行完成后（executionTime 变化）刷新
  const lastExecTime = tab?.executionTime;
  useEffect(() => {
    if (historyOpen) loadHistory();
  }, [historyOpen, loadHistory, lastExecTime]);

  /** 回填编辑器：空则直接填入，非空追加到末尾 */
  const fillFromHistory = useCallback(
    (sql: string) => {
      const current = editorRef.current?.getModel()?.getValue() ?? "";
      const next = current.trim() ? `${current.replace(/\s+$/, "")}\n\n${sql}` : sql;
      updateTabContent(tabId, next);
      editorRef.current?.focus();
    },
    [tabId, updateTabContent]
  );

  const handleDeleteHistory = useCallback(async (id: string) => {
    try {
      await tauriService.deleteHistory(id);
      setHistoryList((list) => list.filter((e) => e.id !== id));
    } catch {
      // ignore
    }
  }, []);

  const handleClearHistory = useCallback(async () => {
    if (!connectionId) return;
    if (!window.confirm(t("editor.confirmClearHistory"))) return;
    try {
      await tauriService.clearHistory(connectionId);
      setHistoryList([]);
    } catch {
      // ignore
    }
  }, [connectionId, t]);

  /** 今天显示时分秒，跨天显示月-日 时:分 */
  const formatHistoryTime = (ts: number) => {
    const d = new Date(ts);
    const now = new Date();
    const sameDay = d.toDateString() === now.toDateString();
    const pad = (n: number) => String(n).padStart(2, "0");
    return sameDay
      ? `${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`
      : `${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}`;
  };

  const handleExecute = useCallback(() => {
    if (!connectionId) return;
    const editor = editorRef.current;

    let sql = "";
    const selection = editor?.getSelection();
    const selectedText =
      selection && editor ? editor.getModel()?.getValueInRange(selection) : "";

    if (selectedText && selectedText.trim()) {
      // 1) 有选中内容：执行选中部分
      sql = selectedText;
    } else if (editor) {
      // 2) 无选中：执行光标所在的语句
      const model = editor.getModel();
      const position = editor.getPosition();
      if (model && position) {
        const offset = model.getOffsetAt(position);
        sql = getStatementAt(model.getValue(), offset);
      }
    }

    // 3) 兜底：整个 tab 内容
    if (!sql.trim()) {
      sql = useEditorStore.getState().tabs.find((tb) => tb.id === tabId)?.content ?? "";
    }

    const trimmed = sql.trim();
    if (trimmed) {
      onExecute(trimmed, tabId);
    }
  }, [connectionId, onExecute, tabId]);

  // 用 ref 持有最新的 handleExecute，避免 Monaco command 闭包捕获过期状态
  const handleExecuteRef = useRef(handleExecute);
  useEffect(() => {
    handleExecuteRef.current = handleExecute;
  }, [handleExecute]);

  const handleEditorMount = useCallback((editor: monaco.editor.IStandaloneCodeEditor) => {
    editorRef.current = editor;
    registerSqlCompletionOnce();

    editor.addCommand(monaco.KeyMod.CtrlCmd | monaco.KeyCode.Enter, () => {
      handleExecuteRef.current();
    });
  }, []);

  if (!tab) {
    return (
      <div className="flex items-center justify-center h-full text-muted-foreground text-sm">
        {t("editor.closed")}
      </div>
    );
  }

  return (
    <div className="flex flex-col h-full">
      {/* 工具栏 */}
      <div className="flex items-center gap-2 px-3 py-1.5 border-b bg-muted/20">
        {isExecuting ? (
          <Button size="sm" variant="destructive" onClick={() => onCancel(tabId)} className="h-7">
            <Square className="mr-1.5 h-3.5 w-3.5" />
            {t("editor.cancelQuery")}
          </Button>
        ) : (
          <Button
            size="sm"
            onClick={handleExecute}
            disabled={!connectionId}
            className="h-7"
          >
            <Play className="mr-1.5 h-3.5 w-3.5" />
            {t("editor.run")}
          </Button>
        )}
        {onExecuteAll && (
          <Button
            size="sm"
            variant="outline"
            disabled={!connectionId || isExecuting}
            className="h-7"
            title={t("editor.runAllTitle")}
            onClick={() => {
              const content = editorRef.current?.getModel()?.getValue() ?? tab?.content ?? "";
              const stmts = splitStatements(content);
              if (stmts.length > 0) onExecuteAll(stmts, tabId);
            }}
          >
            <FastForward className="mr-1.5 h-3.5 w-3.5" />
            {t("editor.runAll")}
          </Button>
        )}
        <Button
          size="sm"
          variant="outline"
          disabled={!connectionId}
          className={`h-7${historyOpen ? " bg-muted" : ""}`}
          title={t("editor.historyTitle")}
          onClick={() => setHistoryOpen((v) => !v)}
        >
          <History className="mr-1.5 h-3.5 w-3.5" />
          {t("editor.history")}
        </Button>
        {!connectionId && (
          <span className="text-xs text-muted-foreground">{t("editor.connectFirst")}</span>
        )}
        {isExecuting && <Loader2 className="h-3 w-3 animate-spin text-muted-foreground" />}
        {tab.executionTime !== undefined && !isExecuting && (
          <span className="text-xs text-muted-foreground ml-auto">
            {t("editor.elapsed", { ms: tab.executionTime })}
          </span>
        )}
      </div>

      {/* 编辑器 + 历史面板（面板覆盖在编辑器右侧） */}
      <div className="flex-1 min-h-0 relative">
        <Editor
          height="100%"
          language="sql"
          theme={theme === "dark" ? "vs-dark" : "vs"}
          value={tab.content}
          onChange={(value) => {
            if (value !== undefined) {
              updateTabContent(tabId, value);
            }
          }}
          onMount={handleEditorMount}
          options={{
            minimap: { enabled: false },
            scrollBeyondLastLine: false,
            fontSize: 13,
            fontFamily: "'JetBrains Mono', 'Fira Code', Consolas, monospace",
            tabSize: 2,
            insertSpaces: true,
            automaticLayout: true,
            folding: true,
            bracketPairColorization: { enabled: true },
            formatOnPaste: true,
          }}
        />

        {historyOpen && (
          <div className="absolute right-0 top-0 bottom-0 w-80 border-l bg-background z-10 flex flex-col shadow-lg">
            {/* 面板头 */}
            <div className="flex items-center gap-2 px-3 py-2 border-b flex-shrink-0">
              <History className="h-3.5 w-3.5 text-muted-foreground" />
              <span className="text-xs font-medium">{t("editor.historyTitle")}</span>
              <span className="text-[10px] text-muted-foreground">
                {t("editor.historyNote")}
              </span>
              <div className="ml-auto flex items-center gap-1">
                <Button
                  size="sm"
                  variant="ghost"
                  className="h-6 px-1.5 text-xs text-muted-foreground"
                  title={t("editor.clearHistoryTitle")}
                  onClick={handleClearHistory}
                  disabled={historyList.length === 0}
                >
                  <Trash2 className="h-3 w-3" />
                </Button>
                <Button
                  size="sm"
                  variant="ghost"
                  className="h-6 px-1.5 text-xs"
                  title={t("editor.close")}
                  onClick={() => setHistoryOpen(false)}
                >
                  <X className="h-3 w-3" />
                </Button>
              </div>
            </div>

            {/* 历史列表 */}
            <div className="flex-1 overflow-y-auto">
              {historyList.length === 0 ? (
                <div className="px-3 py-8 text-center text-xs text-muted-foreground">
                  {t("editor.noHistory")}
                  <br />
                  {t("editor.noHistoryHint")}
                </div>
              ) : (
                historyList.map((entry) => (
                  <div
                    key={entry.id}
                    className="group px-3 py-2 border-b border-border/50 hover:bg-muted/40 cursor-pointer"
                    title={t("editor.clickToFill")}
                    onClick={() => fillFromHistory(entry.sql)}
                  >
                    <div className="font-mono text-xs leading-4 line-clamp-2 break-all">
                      {entry.sql}
                    </div>
                    <div className="mt-1 flex items-center gap-2 text-[10px] text-muted-foreground">
                      <span>{formatHistoryTime(entry.executedAt)}</span>
                      <span>{entry.durationMs}ms</span>
                      <span>
                        {t("editor.rows", { count: entry.rowCount ?? 0 })}
                        {entry.truncated ? t("editor.truncatedMark") : ""}
                      </span>
                      <button
                        className="ml-auto opacity-0 group-hover:opacity-100 hover:text-destructive transition-opacity"
                        title={t("editor.deleteEntry")}
                        onClick={(e) => {
                          e.stopPropagation();
                          handleDeleteHistory(entry.id);
                        }}
                      >
                        <Trash2 className="h-3 w-3" />
                      </button>
                    </div>
                  </div>
                ))
              )}
            </div>
          </div>
        )}
      </div>
    </div>
  );
}
