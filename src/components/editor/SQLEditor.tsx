import { useEffect, useRef, useCallback } from "react";
import Editor, { loader } from "@monaco-editor/react";
import { useThemeStore } from "@/stores/themeStore";

import * as monaco from "monaco-editor";
import { useEditorStore } from "@/stores/editorStore";
import { Play, Square, Loader2, FastForward } from "lucide-react";
import { Button } from "@/components/ui";

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
  const tab = useEditorStore((s) => s.tabs.find((t) => t.id === tabId));
  const theme = useThemeStore((s) => s.theme);
  const updateTabContent = useEditorStore((s) => s.updateTabContent);

  const isExecuting = tab?.isExecuting ?? false;
  const editorRef = useRef<monaco.editor.IStandaloneCodeEditor | null>(null);

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
      sql = useEditorStore.getState().tabs.find((t) => t.id === tabId)?.content ?? "";
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
        查询页已被关闭
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
            取消查询
          </Button>
        ) : (
          <Button
            size="sm"
            onClick={handleExecute}
            disabled={!connectionId}
            className="h-7"
          >
            <Play className="mr-1.5 h-3.5 w-3.5" />
            执行 (Ctrl+Enter)
          </Button>
        )}
        {onExecuteAll && (
          <Button
            size="sm"
            variant="outline"
            disabled={!connectionId || isExecuting}
            className="h-7"
            title="把整页 SQL 按分号拆分，逐条批量执行"
            onClick={() => {
              const content = editorRef.current?.getModel()?.getValue() ?? tab?.content ?? "";
              const stmts = splitStatements(content);
              if (stmts.length > 0) onExecuteAll(stmts, tabId);
            }}
          >
            <FastForward className="mr-1.5 h-3.5 w-3.5" />
            执行全部
          </Button>
        )}
        {!connectionId && (
          <span className="text-xs text-muted-foreground">请先连接数据库</span>
        )}
        {isExecuting && <Loader2 className="h-3 w-3 animate-spin text-muted-foreground" />}
        {tab.executionTime !== undefined && !isExecuting && (
          <span className="text-xs text-muted-foreground ml-auto">
            耗时: {tab.executionTime}ms
          </span>
        )}
      </div>

      {/* 编辑器 */}
      <div className="flex-1 min-h-0">
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
      </div>
    </div>
  );
}
