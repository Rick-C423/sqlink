import { useEffect, useRef, useState, useCallback, lazy, Suspense } from "react";
import { ConnectionConfig, QueryTarget } from "@/types/database";
import { useConnectionStore } from "@/stores/connectionStore";
import { useEditorStore } from "@/stores/editorStore";
import { useWorkspaceStore } from "@/stores/workspaceStore";
import { tauriService } from "@/services/tauriService";
import { cn } from "@/lib/utils";
import { createQueryActions } from "@/lib/queryActions";
import { ConnectionTree } from "@/components/connection/ConnectionTree";
import { DatabaseInfoPanel } from "@/components/connection/DatabaseInfoPanel";
import { ConnectionDialog } from "@/components/connection/ConnectionDialog";
import { McpSettingsDialog } from "@/components/mcp/McpSettingsDialog";
// Monaco 体积大（含各语言 tokenizer），懒加载避免拖慢首屏
const SQLEditorPanel = lazy(() =>
  import("@/components/editor/SQLEditor").then((m) => ({ default: m.SQLEditorPanel }))
);
import { DataGrid } from "@/components/result/DataGrid";
import { DatabaseOverview } from "@/components/DatabaseOverview";
import { TableView } from "@/components/TableView";
import { ViewView } from "@/components/ViewView";
import { RoutineView } from "@/components/RoutineView";
import { UsersView } from "@/components/UsersView";
import { ServerOverview } from "@/components/ServerOverview";
import { CreateTableView } from "@/components/CreateTableView";
import { Menu, X, Database, Table2, FolderOpen, FileCode2, Plus, Sun, Moon, ChevronLeft, ChevronRight, Eye, Languages, ShieldCheck, Server, ScrollText, Zap, FileCog, Sigma, Settings2 } from "lucide-react";
import { Button } from "@/components/ui";
import { ContextMenu } from "@/components/ContextMenu";
import { Resizer } from "@/components/ui/Resizer";
import { useThemeStore } from "@/stores/themeStore";
import { useI18nStore, useT } from "@/i18n";

const queryActions = createQueryActions(useEditorStore, tauriService);

export default function App() {
  const t = useT();
  const [dialogOpen, setDialogOpen] = useState(false);
  const [editingConnection, setEditingConnection] = useState<ConnectionConfig | null>(null);
  const [mcpSettingsOpen, setMcpSettingsOpen] = useState(false);
  const [sidebarVisible, setSidebarVisible] = useState(true);
  const [error, setError] = useState<string | null>(null);
  // tab 条右键菜单
  const [tabMenu, setTabMenu] = useState<{ x: number; y: number; tabId: string } | null>(null);
  // tab 条溢出检测：压缩到最小宽度后仍放不下时，显示左右移动按钮
  const tabBarRef = useRef<HTMLDivElement>(null);
  const [tabOverflow, setTabOverflow] = useState(false);

  // 侧栏宽度（可拖拽，持久化到 localStorage）
  const SIDEBAR_DEFAULT = 256;
  const [sidebarWidth, setSidebarWidth] = useState(() => {
    const v = Number(localStorage.getItem("sqlink.sidebarWidth"));
    return v >= 160 && v <= 560 ? v : SIDEBAR_DEFAULT;
  });
  const resizeSidebar = useCallback((delta: number) => {
    setSidebarWidth((w) => {
      const next = Math.min(560, Math.max(160, w + delta));
      localStorage.setItem("sqlink.sidebarWidth", String(next));
      return next;
    });
  }, []);

  const activeConnection = useConnectionStore((s) => s.activeConnection);
  const selectedDatabase = useConnectionStore((s) => s.selectedDatabase);
  const saved = useConnectionStore((s) => s.saved);
  const setSaved = useConnectionStore((s) => s.setSaved);

  const wsTabs = useWorkspaceStore((s) => s.tabs);
  const activeWsId = useWorkspaceStore((s) => s.activeId);
  const activateWs = useWorkspaceStore((s) => s.activate);
  const closeWs = useWorkspaceStore((s) => s.close);
  const openQuery = useWorkspaceStore((s) => s.openQuery);

  const theme = useThemeStore((s) => s.theme);
  const toggleTheme = useThemeStore((s) => s.toggle);

  const locale = useI18nStore((s) => s.locale);
  const toggleLocale = useI18nStore((s) => s.toggleLocale);
  // 顶栏设置菜单（MCP 设置 / 日志目录 / 语言切换）
  const [topMenu, setTopMenu] = useState<{ x: number; y: number } | null>(null);

  // 启动时加载已保存的连接
  useEffect(() => {
    tauriService
      .listSavedConnections()
      .then(setSaved)
      .catch((err) => console.error("加载已保存连接失败:", err));
  }, [setSaved]);

  // tab 条溢出检测：tab 数/窗口宽度变化时重新计算
  useEffect(() => {
    const el = tabBarRef.current;
    if (!el) return;
    const check = () => setTabOverflow(el.scrollWidth > el.clientWidth + 1);
    check();
    const ro = new ResizeObserver(check);
    ro.observe(el);
    window.addEventListener("resize", check);
    return () => {
      ro.disconnect();
      window.removeEventListener("resize", check);
    };
  }, [wsTabs.length, sidebarVisible]);

  // 全局禁用 webview 默认右键菜单（刷新/另存为/检查），自定义右键菜单不受影响
  useEffect(() => {
    const suppress = (e: MouseEvent) => e.preventDefault();
    window.addEventListener("contextmenu", suppress);
    return () => window.removeEventListener("contextmenu", suppress);
  }, []);

  const selectedQueryTarget = (): QueryTarget | null => {
    if (!activeConnection || !selectedDatabase) {
      setError(t("app.selectDatabaseFirst"));
      return null;
    }
    return {
      connectionId: activeConnection,
      database: selectedDatabase,
      driverType: saved.find((connection) => connection.id === activeConnection)?.driverType ?? "mysql",
    };
  };

  const handleExecute = async (sql: string, editorTabId: string) => {
    const target = selectedQueryTarget();
    if (!target) return;
    setError(null);
    try {
      await queryActions.execute(editorTabId, sql, target);
    } catch (err) {
      setError(t("app.executeFailed", { error: String(err) }));
    }
  };

  const handleCancel = (editorTabId: string) => {
    queryActions.cancel(editorTabId).catch((err) => {
      setError(t("app.cancelFailed", { error: String(err) }));
    });
  };

  /** 数据网格编辑提交：事务执行后重新跑一遍原 SQL 刷新结果 */
  const handleApplyEdits = async (editorTabId: string, sourceTaskId: number | undefined, statements: string[]) => {
    await queryActions.applyEdits(editorTabId, sourceTaskId, statements);
  };

  /** 执行全部：整页 SQL 拆成多条语句，批量执行（DDL 脚本用） */
  const handleExecuteAll = async (statements: string[], editorTabId: string) => {
    const target = selectedQueryTarget();
    if (!target) return;
    setError(null);
    try {
      await queryActions.executeAll(editorTabId, statements, target);
    } catch (err) {
      setError(t("app.batchFailed", { error: String(err) }));
    }
  };

  const activeWs = wsTabs.find((t) => t.id === activeWsId);

  const wsTabIcon = (kind: string) => {
    switch (kind) {
      case "database":
        return <FolderOpen className="h-3.5 w-3.5 text-blue-500" />;
      case "table":
        return <Table2 className="h-3.5 w-3.5 text-green-600" />;
      case "view":
        return <Eye className="h-3.5 w-3.5 text-teal-500" />;
      case "users":
        return <ShieldCheck className="h-3.5 w-3.5 text-blue-500" />;
      case "server":
        return <Server className="h-3.5 w-3.5 text-orange-500" />;
      case "createTable":
        return <Plus className="h-3.5 w-3.5 text-amber-500" />;
      default:
        return <FileCode2 className="h-3.5 w-3.5 text-purple-500" />;
    }
  };

  return (
    <div className="h-screen flex flex-col bg-background text-foreground overflow-hidden">
      {/* 主体区域（顶部分左右：侧栏标题栏 | tab 条） */}
      <div className="flex-1 flex overflow-hidden">
        {/* 左侧边栏（宽度可拖拽） */}
        {sidebarVisible && (
          <>
            <div className="flex-shrink-0 min-w-0 flex flex-col" style={{ width: sidebarWidth }}>
              <div className="flex-1 min-h-0">
                <ConnectionTree
                  onNewConnection={() => {
                    setEditingConnection(null);
                    setDialogOpen(true);
                  }}
                  onEditConnection={(cfg) => {
                    setEditingConnection(cfg);
                    setDialogOpen(true);
                  }}
                />
              </div>
              {activeConnection && selectedDatabase && (
                <DatabaseInfoPanel connectionId={activeConnection} database={selectedDatabase} />
              )}
            </div>
            <Resizer
              orientation="vertical"
              onDrag={resizeSidebar}
              onDoubleClick={() => {
                setSidebarWidth(SIDEBAR_DEFAULT);
                localStorage.setItem("sqlink.sidebarWidth", String(SIDEBAR_DEFAULT));
              }}
            />
          </>
        )}

        {/* 右侧工作台 */}
        <div className="flex-1 flex flex-col min-w-0">
          {error && (
            <div className="px-4 py-2 bg-red-50 text-red-800 text-sm border-b border-red-200 flex items-center justify-between">
              <span>{error}</span>
              <button onClick={() => setError(null)} className="text-red-600 hover:text-red-800">
                ✕
              </button>
            </div>
          )}

          {/* 工作台 tab 条：左侧栏开关，右侧主题/语言/连接状态 */}
          <div className="flex items-center border-b bg-muted/30 flex-shrink-0">
            <Button
              variant="ghost"
              size="icon"
              className="h-7 w-7 mx-1 flex-shrink-0"
              title={sidebarVisible ? t("app.collapseSidebar") : t("app.expandSidebar")}
              onClick={() => setSidebarVisible(!sidebarVisible)}
            >
              {sidebarVisible ? <X className="h-4 w-4" /> : <Menu className="h-4 w-4" />}
            </Button>
            {tabOverflow && (
              <Button
                variant="ghost"
                size="icon"
                className="h-7 w-6 flex-shrink-0"
                title={t("app.scrollLeft")}
                onClick={() => tabBarRef.current?.scrollBy({ left: -160, behavior: "smooth" })}
              >
                <ChevronLeft className="h-4 w-4" />
              </Button>
            )}
            <div ref={tabBarRef} className="flex-1 flex min-w-0 overflow-x-auto no-scrollbar">
              {wsTabs.map((tab) => (
                <div
                  key={tab.id}
                  className={`tree-node group flex items-center gap-1 px-3 py-1.5 text-xs cursor-pointer border-r min-w-[72px] max-w-[180px] flex-shrink ${
                    activeWsId === tab.id
                      ? "tree-node-active bg-background font-medium"
                      : "text-muted-foreground"
                  }`}
                  onClick={() => activateWs(tab.id)}
                  onContextMenu={(e) => {
                    e.preventDefault();
                    e.stopPropagation();
                    setTabMenu({ x: e.clientX, y: e.clientY, tabId: tab.id });
                  }}
                >
                  {tab.kind === "query" ? (
                    <FileCode2 className="h-3.5 w-3.5" />
                  ) : tab.kind === "database" ? (
                    <Database className="h-3.5 w-3.5" />
                  ) : tab.kind === "createTable" ? (
                    <Plus className="h-3.5 w-3.5" />
                  ) : tab.kind === "view" ? (
                    <Eye className="h-3.5 w-3.5 text-teal-500" />
                  ) : tab.kind === "routine" ? (
                    tab.routineKind === "trigger" ? (
                      <Zap className="h-3.5 w-3.5 text-amber-500" />
                    ) : tab.routineKind === "procedure" ? (
                      <FileCog className="h-3.5 w-3.5 text-indigo-500" />
                    ) : (
                      <Sigma className="h-3.5 w-3.5 text-purple-500" />
                    )
                  ) : tab.kind === "users" ? (
                    <ShieldCheck className="h-3.5 w-3.5 text-blue-500" />
                  ) : tab.kind === "server" ? (
                    <Server className="h-3.5 w-3.5 text-orange-500" />
                  ) : (
                    <Table2 className="h-3.5 w-3.5" />
                  )}
                  <span className="flex-1 min-w-0 truncate">{tab.title}</span>
                  <button
                    className="rounded p-0.5 flex-shrink-0 opacity-0 group-hover:opacity-100 hover:bg-muted"
                    onClick={(e) => {
                      e.stopPropagation();
                      closeWs(tab.id);
                    }}
                  >
                    <X className="h-3 w-3" />
                  </button>
                </div>
              ))}
            </div>
            {tabOverflow && (
              <Button
                variant="ghost"
                size="icon"
                className="h-7 w-6 flex-shrink-0"
                title={t("app.scrollRight")}
                onClick={() => tabBarRef.current?.scrollBy({ left: 160, behavior: "smooth" })}
              >
                <ChevronRight className="h-4 w-4" />
              </Button>
            )}
            <Button
              variant="ghost"
              size="icon"
              className="h-7 w-7 flex-shrink-0"
              title={t("app.newQuery")}
              onClick={() => {
                if (activeConnection) openQuery(activeConnection);
                else setError(t("app.selectConnectionFirst"));
              }}
            >
              <Plus className="h-4 w-4" />
            </Button>
            <Button
              variant="ghost"
              size="icon"
              className="h-7 w-7 flex-shrink-0"
              title={theme === "dark" ? t("app.toLightTheme") : t("app.toDarkTheme")}
              onClick={toggleTheme}
            >
              {theme === "dark" ? <Sun className="h-4 w-4" /> : <Moon className="h-4 w-4" />}
            </Button>
            <Button
              variant="ghost"
              size="icon"
              className="h-7 w-7 flex-shrink-0"
              title={t("app.settingsMenu")}
              onClick={(e) => {
                const rect = (e.currentTarget as HTMLElement).getBoundingClientRect();
                setTopMenu({ x: rect.left, y: rect.bottom + 4 });
              }}
            >
              <Settings2 className="h-4 w-4" />
            </Button>
            {activeConnection && (
              <div className="flex items-center gap-2 text-xs text-muted-foreground px-3 flex-shrink-0">
                <div className="w-2 h-2 rounded-full bg-green-500" />
                <span>
                  {saved.find((c) => c.id === activeConnection)?.name}
                  {selectedDatabase ? ` / ${selectedDatabase}` : ""}
                </span>
              </div>
            )}
          </div>

          {wsTabs.length === 0 ? (
            <div className="flex-1 flex flex-col items-center justify-center text-muted-foreground">
              <Database className="h-12 w-12 mb-3 opacity-30" />
              <p className="text-sm">{t("app.emptyHint")}</p>
              <p className="text-xs mt-1 opacity-70">
                {t("app.emptyOr")}{" "}
                <button
                  className="text-blue-600 hover:underline"
                  onClick={() => activeConnection && openQuery(activeConnection)}
                >
                  {t("app.newQuery")}
                </button>
              </p>
            </div>
          ) : (
            <>
              {/* tab 内容 */}
              <div className="flex-1 min-h-0">
                {activeWs?.kind === "database" && activeWs.database && (
                  <DatabaseOverview
                    connectionId={activeWs.connectionId}
                    database={activeWs.database}
                  />
                )}
                {activeWs?.kind === "table" && activeWs.database && activeWs.table && (
                  <TableView
                    // key 绑定表身份：切表/切页签时强制重建实例，
                    // 否则筛选、页码等 useState 会跨表残留
                    key={`${activeWs.connectionId}/${activeWs.database}/${activeWs.table}`}
                    connectionId={activeWs.connectionId}
                    database={activeWs.database}
                    table={activeWs.table}
                    initialSubTab={activeWs.tableSubTab}
                    subTabNonce={activeWs.subTabNonce}
                  />
                )}
                {activeWs?.kind === "view" && activeWs.database && activeWs.table && (
                  <ViewView
                    key={`${activeWs.connectionId}/${activeWs.database}/${activeWs.table}`}
                    connectionId={activeWs.connectionId}
                    database={activeWs.database}
                    view={activeWs.table}
                  />
                )}
                {activeWs?.kind === "routine" && activeWs.database && activeWs.table && activeWs.routineKind && (
                  <RoutineView
                    connectionId={activeWs.connectionId}
                    database={activeWs.database}
                    routineKind={activeWs.routineKind}
                    name={activeWs.table}
                  />
                )}
                {activeWs?.kind === "createTable" && activeWs.database && (
                  <CreateTableView
                    connectionId={activeWs.connectionId}
                    database={activeWs.database}
                    tabId={activeWs.id}
                  />
                )}
                {activeWs?.kind === "users" && (
                  <UsersView connectionId={activeWs.connectionId} />
                )}
                {activeWs?.kind === "server" && (
                  <ServerOverview connectionId={activeWs.connectionId} />
                )}
                {activeWs?.kind === "query" && activeWs.editorTabId && (
                  <QueryPanel
                    key={activeWs.editorTabId}
                    editorTabId={activeWs.editorTabId}
                    connectionId={activeConnection}
                    onExecute={handleExecute}
                    onExecuteAll={handleExecuteAll}
                    onCancel={handleCancel}
                    onApplyEdits={handleApplyEdits}
                  />
                )}
              </div>
            </>
          )}
        </div>
      </div>

      {/* tab 右键菜单：关闭 / 关闭其他 / 关闭全部 */}
      {tabMenu && (
        <ContextMenu
          x={tabMenu.x}
          y={tabMenu.y}
          items={[
            {
              label: t("app.closeTab"),
              onClick: () => closeWs(tabMenu.tabId),
            },
            {
              label: t("app.closeOthers"),
              onClick: () => {
                useWorkspaceStore
                  .getState()
                  .tabs.filter((t) => t.id !== tabMenu.tabId)
                  .forEach((t) => useWorkspaceStore.getState().close(t.id));
              },
            },
            {
              label: t("app.closeAll"),
              onClick: () => {
                useWorkspaceStore
                  .getState()
                  .tabs.forEach((t) => useWorkspaceStore.getState().close(t.id));
              },
            },
          ]}
          onClose={() => setTabMenu(null)}
        />
      )}

      {/* 顶栏设置菜单 */}
      {topMenu && (
        <ContextMenu
          x={topMenu.x}
          y={topMenu.y}
          items={[
            {
              label: t("app.mcpSettings"),
              icon: <Settings2 className="h-4 w-4" />,
              onClick: () => setMcpSettingsOpen(true),
            },
            {
              label: t("app.openLogFolder"),
              icon: <ScrollText className="h-4 w-4" />,
              onClick: () => tauriService.openLogFolder().catch((err) => setError(String(err))),
            },
            {
              label: locale === "zh-CN" ? "English" : "中文",
              icon: <Languages className="h-4 w-4" />,
              onClick: toggleLocale,
            },
          ]}
          onClose={() => setTopMenu(null)}
        />
      )}

      {/* 连接对话框 */}
      <ConnectionDialog
        open={dialogOpen}
        onClose={() => setDialogOpen(false)}
        editing={editingConnection}
      />

      {/* MCP 设置对话框 */}
      <McpSettingsDialog open={mcpSettingsOpen} onClose={() => setMcpSettingsOpen(false)} />
    </div>
  );
}

/** 查询工作台页：编辑器 + 结果网格 */
function QueryPanel({
  editorTabId,
  connectionId,
  onExecute,
  onExecuteAll,
  onCancel,
  onApplyEdits,
}: {
  editorTabId: string;
  connectionId: string | null;
  onExecute: (sql: string, tabId: string) => void;
  onExecuteAll: (statements: string[], tabId: string) => void;
  onCancel: (tabId: string) => void;
  onApplyEdits: (tabId: string, sourceTaskId: number | undefined, statements: string[]) => Promise<void>;
}) {
  const t = useT();
  const tab = useEditorStore((s) => s.tabs.find((t) => t.id === editorTabId));

  // 结果区高度（可拖拽，持久化到 localStorage）
  const RESULT_DEFAULT = 256;
  const [resultHeight, setResultHeight] = useState(() => {
    const v = Number(localStorage.getItem("sqlink.resultHeight"));
    return v >= 80 && v <= 800 ? v : RESULT_DEFAULT;
  });
  // 向上拖（delta 为负）结果区变高；给编辑器至少保留 120px
  const resizeResult = useCallback((delta: number) => {
    setResultHeight((h) => {
      const max = Math.max(200, window.innerHeight - 260);
      const next = Math.min(max, Math.max(80, h - delta));
      localStorage.setItem("sqlink.resultHeight", String(next));
      return next;
    });
  }, []);

  return (
    <div className="flex flex-col h-full">
      <div className="flex-1 min-h-0">
        <Suspense
          fallback={
            <div className="flex items-center justify-center h-full text-muted-foreground text-sm">
              {t("app.loadingEditor")}
            </div>
          }
        >
          <SQLEditorPanel
            tabId={editorTabId}
            connectionId={connectionId}
            onExecute={onExecute}
            onExecuteAll={onExecuteAll}
            onCancel={onCancel}
          />
        </Suspense>
      </div>
      <Resizer
        orientation="horizontal"
        onDrag={resizeResult}
        onDoubleClick={() => {
          setResultHeight(RESULT_DEFAULT);
          localStorage.setItem("sqlink.resultHeight", String(RESULT_DEFAULT));
        }}
      />
      <div className="border-t flex-shrink-0" style={{ height: resultHeight }}>
        <DataGrid
          result={tab?.result}
          editContext={tab?.isExecuting ? null : tab?.editContext}
          onApplyEdits={(statements) => onApplyEdits(editorTabId, tab?.resultSource?.taskId, statements)}
        />
      </div>
    </div>
  );
}
